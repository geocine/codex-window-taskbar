use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_R,
};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::diagnose;
use crate::localization::{self, Strings};
use crate::models::UsageData;
use crate::native_interop::{
    self, Color, TIMER_COUNTDOWN, TIMER_POLL, TIMER_RESET_POLL, TIMER_TASKBAR_RETRY, WM_APP_TRAY,
    WM_APP_USAGE_UPDATED,
};
use crate::poller;
use crate::theme;
use crate::tray_icon;

/// Wrapper to make HWND sendable across threads (safe for PostMessage usage)
#[derive(Clone, Copy)]
struct SendHwnd(isize);

unsafe impl Send for SendHwnd {}

impl SendHwnd {
    fn from_hwnd(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
    fn to_hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

/// One painted usage row (label + bar + remaining text).
#[derive(Clone, Debug)]
struct DisplayRow {
    label: String,
    percent: f64,
    text: String,
}

/// Shared application state
struct AppState {
    hwnd: SendHwnd,
    taskbar_hwnd: Option<HWND>,
    tray_notify_hwnd: Option<HWND>,
    tooltip_hwnd: Option<HWND>,
    tooltip_visible: bool,
    mouse_tracking: bool,
    tooltip_text: String,
    win_event_hook: Option<HWINEVENTHOOK>,
    is_dark: bool,
    embedded: bool,

    /// 1 or 2 rows depending on what the usage API returns.
    display_rows: Vec<DisplayRow>,

    codex_data: Option<UsageData>,

    poll_interval_ms: u32,
    retry_count: u32,
    last_poll_ok: bool,

    tray_offset: i32,
    dragging: bool,
    drag_start_mouse_x: i32,
    drag_start_offset: i32,

    widget_visible: bool,
}

fn default_display_rows(strings: Strings) -> Vec<DisplayRow> {
    vec![
        DisplayRow {
            label: strings.session_window.to_string(),
            percent: 0.0,
            text: "--".to_string(),
        },
        DisplayRow {
            label: strings.weekly_window.to_string(),
            percent: 0.0,
            text: "--".to_string(),
        },
    ]
}

const RETRY_BASE_MS: u32 = 30_000; // 30 seconds

const POLL_1_MIN: u32 = 60_000;
const POLL_5_MIN: u32 = 300_000;
const POLL_15_MIN: u32 = 900_000;
const POLL_1_HOUR: u32 = 3_600_000;

// Menu item IDs for update frequency
const IDM_FREQ_1MIN: u16 = 10;
const IDM_FREQ_5MIN: u16 = 11;
const IDM_FREQ_15MIN: u16 = 12;
const IDM_FREQ_1HOUR: u16 = 13;
const IDM_START_WITH_WINDOWS: u16 = 20;
const IDM_RESET_POSITION: u16 = 30;

const DIVIDER_HIT_ZONE: i32 = 13; // LEFT_DIVIDER_W + DIVIDER_RIGHT_MARGIN

const WM_DPICHANGED_MSG: u32 = 0x02E0;
const WM_APP_REFRESH_SHORTCUT: u32 = native_interop::WM_APP + 4;
const TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS: u64 = 750;
const REFRESH_KEY_DEBOUNCE_MS: u64 = 500;
const WM_MOUSELEAVE_MSG: u32 = 0x02A3;
const TOOLTIP_HEIGHT: i32 = 36;
const TOOLTIP_MIN_WIDTH: i32 = 150;
const TOOLTIP_RADIUS: i32 = 2;
const TOOLTIP_TEXT_PADDING_X: i32 = 8;
const TOOLTIP_MEASURE_MAX_WIDTH: i32 = 1000;
const WINDOWS_TICK: u64 = 10_000_000;
const SEC_TO_UNIX_EPOCH: u64 = 11_644_473_600;

static SUPPRESS_TRAY_REPOSITION_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
static LAST_REFRESH_KEY_AT: Mutex<Option<Instant>> = Mutex::new(None);
static KEYBOARD_HOOK: AtomicIsize = AtomicIsize::new(0);

/// Current system DPI (96 = 100% scaling, 144 = 150%, 192 = 200%, etc.)
static CURRENT_DPI: AtomicU32 = AtomicU32::new(96);

/// Scale a base pixel value (designed at 96 DPI) to the current DPI.
fn sc(px: i32) -> i32 {
    let dpi = CURRENT_DPI.load(Ordering::Relaxed);
    (px as f64 * dpi as f64 / 96.0).round() as i32
}

/// Re-query the monitor DPI for our window and update the cached value.
/// Uses GetDpiForWindow which returns the live DPI (unlike GetDpiForSystem
/// which is cached at process startup and never changes).
fn refresh_dpi() {
    let hwnd = {
        let state = lock_state();
        state.as_ref().map(|s| s.hwnd.to_hwnd())
    };
    if let Some(hwnd) = hwnd {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi > 0 {
            CURRENT_DPI.store(dpi, Ordering::Relaxed);
        }
    }
}

fn load_embedded_app_icons() -> (HICON, HICON) {
    unsafe {
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return (HICON::default(), HICON::default());
        }

        let mut large_icon = HICON::default();
        let mut small_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR::from_raw(exe_buf.as_ptr()),
            0,
            Some(&mut large_icon),
            Some(&mut small_icon),
            1,
        );

        if extracted == 0 {
            (HICON::default(), HICON::default())
        } else {
            (large_icon, small_icon)
        }
    }
}

fn install_keyboard_refresh_hook() {
    if KEYBOARD_HOOK.load(Ordering::Relaxed) != 0 {
        return;
    }

    unsafe {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        match SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(low_level_keyboard_proc),
            HINSTANCE(hmodule.0),
            0,
        ) {
            Ok(hook) => {
                KEYBOARD_HOOK.store(hook.0 as isize, Ordering::Relaxed);
                diagnose::log("keyboard refresh hook installed");
            }
            Err(error) => diagnose::log(format!("keyboard refresh hook failed: {error}")),
        }
    }
}

fn uninstall_keyboard_refresh_hook() {
    let hook = KEYBOARD_HOOK.swap(0, Ordering::Relaxed);
    if hook == 0 {
        return;
    }

    unsafe {
        let _ = UnhookWindowsHookEx(HHOOK(hook as *mut _));
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code == HC_ACTION as i32 && wparam.0 as u32 == WM_KEYDOWN {
        let kb = *(lparam.0 as *const KBDLLHOOKSTRUCT);
        if kb.vkCode == VK_R.0 as u32 {
            if let Some(hwnd) = hovered_refresh_hwnd() {
                if refresh_key_allowed() {
                    let _ = PostMessageW(hwnd, WM_APP_REFRESH_SHORTCUT, WPARAM(0), LPARAM(0));
                }
            }
        }
    }

    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

fn refresh_key_allowed() -> bool {
    let mut last = LAST_REFRESH_KEY_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if last
        .map(|previous| now.duration_since(previous).as_millis() < REFRESH_KEY_DEBOUNCE_MS as u128)
        .unwrap_or(false)
    {
        return false;
    }

    *last = Some(now);
    true
}

fn hovered_refresh_hwnd() -> Option<HWND> {
    let (hwnd, widget_visible) = {
        let state = lock_state();
        let s = state.as_ref()?;
        (s.hwnd.to_hwnd(), s.widget_visible)
    };

    let mut pt = POINT::default();
    unsafe {
        if GetCursorPos(&mut pt).is_err() {
            return None;
        }
    }

    if widget_visible {
        if let Some(rect) = native_interop::get_window_rect_safe(hwnd) {
            if point_in_rect(pt, rect) {
                return Some(hwnd);
            }
        }
    }

    if tray_icon::contains_point(hwnd, pt) {
        return Some(hwnd);
    }

    None
}

fn point_in_rect(pt: POINT, rect: RECT) -> bool {
    pt.x >= rect.left && pt.x < rect.right && pt.y >= rect.top && pt.y < rect.bottom
}

unsafe impl Send for AppState {}

static STATE: Mutex<Option<AppState>> = Mutex::new(None);

/// Lock STATE safely, recovering from poisoned mutex
fn lock_state() -> MutexGuard<'static, Option<AppState>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn settings_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata)
        .join("CodexWindowsTaskbar")
        .join("settings.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    tray_offset: i32,
    #[serde(default = "default_poll_interval")]
    poll_interval_ms: u32,
    #[serde(default = "default_widget_visible")]
    widget_visible: bool,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            tray_offset: 0,
            poll_interval_ms: default_poll_interval(),
            widget_visible: true,
        }
    }
}

fn default_poll_interval() -> u32 {
    POLL_15_MIN
}

fn default_widget_visible() -> bool {
    true
}

fn load_settings() -> SettingsFile {
    let content = match std::fs::read_to_string(settings_path()) {
        Ok(c) => c,
        Err(_) => return SettingsFile::default(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_settings(settings: &SettingsFile) {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}

fn save_state_settings() {
    let state = lock_state();
    if let Some(s) = state.as_ref() {
        save_settings(&SettingsFile {
            tray_offset: s.tray_offset,
            poll_interval_ms: s.poll_interval_ms,
            widget_visible: s.widget_visible,
        });
    }
}

fn tray_icon_data_from_state() -> (Option<f64>, String) {
    let state = lock_state();
    match state.as_ref() {
        Some(s) if s.last_poll_ok && !s.display_rows.is_empty() => {
            let tooltip = s
                .display_rows
                .iter()
                .map(|row| format!("{}: {}", row.label, row.text))
                .collect::<Vec<_>>()
                .join(" | ");
            let max_used = s
                .display_rows
                .iter()
                .map(|row| row.percent)
                .fold(0.0_f64, f64::max);
            (
                Some(poller::remaining_percent(max_used)),
                format!("Codex {tooltip}"),
            )
        }
        _ => (None, "Codex Windows Taskbar".to_string()),
    }
}

fn toggle_widget_visibility(hwnd: HWND) {
    let new_visible = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.widget_visible = !s.widget_visible;
            s.widget_visible
        } else {
            return;
        }
    };
    save_state_settings();
    unsafe {
        if new_visible {
            ensure_taskbar_attachment();
            position_at_taskbar();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            render_layered();
        } else {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

fn refresh_usage_texts(state: &mut AppState) {
    if !state.last_poll_ok {
        return;
    }

    let strings = localization::strings();
    if let Some(data) = state.codex_data.as_ref() {
        state.display_rows = data
            .windows
            .iter()
            .map(|window| DisplayRow {
                label: poller::window_label(window, strings),
                percent: window.percentage,
                text: poller::format_line(window, strings),
            })
            .collect();
    }
}

const STARTUP_REGISTRY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_REGISTRY_KEY: &str = "CodexWindowsTaskbar";

/// Returns true only if the startup registry value points to this executable.
fn is_startup_enabled() -> bool {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);
        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        );
        if result.is_err() {
            return false;
        }

        // Query the size of the value
        let mut data_size: u32 = 0;
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut data_size),
        );
        if result.is_err() || data_size == 0 {
            let _ = RegCloseKey(hkey);
            return false;
        }

        // Read the value
        let mut buf = vec![0u8; data_size as usize];
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut data_size),
        );
        let _ = RegCloseKey(hkey);
        if result.is_err() {
            return false;
        }

        // Convert the registry value (UTF-16) to a string
        let wide_slice =
            std::slice::from_raw_parts(buf.as_ptr() as *const u16, data_size as usize / 2);
        let reg_value = String::from_utf16_lossy(wide_slice)
            .trim_end_matches('\0')
            .to_string();

        // Get the current executable path
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return false;
        }
        let current_exe = String::from_utf16_lossy(&exe_buf[..len]);

        // Case-insensitive comparison (Windows paths are case-insensitive)
        reg_value.eq_ignore_ascii_case(&current_exe)
    }
}

fn set_startup_enabled(enable: bool) {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if result.is_err() {
            return;
        }

        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        if enable {
            let mut exe_buf = [0u16; 260];
            let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
            if len > 0 {
                // Write the wide string including null terminator
                let byte_len = ((len + 1) * 2) as u32;
                let _ = RegSetValueExW(
                    hkey,
                    PCWSTR::from_raw(key_name.as_ptr()),
                    0,
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        exe_buf.as_ptr() as *const u8,
                        byte_len as usize,
                    )),
                );
            }
        } else {
            let _ = RegDeleteValueW(hkey, PCWSTR::from_raw(key_name.as_ptr()));
        }

        let _ = RegCloseKey(hkey);
    }
}

// Dimensions matching the C# version
const SEGMENT_W: i32 = 10;
const SEGMENT_H: i32 = 13;
const SEGMENT_GAP: i32 = 1;
const PROVIDER_SEGMENT_COUNT: i32 = 10;
const CORNER_RADIUS: i32 = 2;

const LEFT_DIVIDER_W: i32 = 3;
const DIVIDER_RIGHT_MARGIN: i32 = 10;
const LABEL_WIDTH: i32 = 18;
const LABEL_RIGHT_MARGIN: i32 = 10;
const BAR_RIGHT_MARGIN: i32 = 3;
const TEXT_WIDTH: i32 = 86;
const RIGHT_MARGIN: i32 = 0;
const TRAY_EDGE_OVERLAP: i32 = 8;
const WIDGET_HEIGHT: i32 = 46;
const UI_MONO_FONT: &str = "Consolas";

fn total_widget_width() -> i32 {
    sc(LEFT_DIVIDER_W)
        + sc(DIVIDER_RIGHT_MARGIN)
        + sc(LABEL_WIDTH)
        + sc(LABEL_RIGHT_MARGIN)
        + provider_bar_width()
        + sc(BAR_RIGHT_MARGIN)
        + sc(TEXT_WIDTH)
        + sc(RIGHT_MARGIN)
}

fn provider_bar_width() -> i32 {
    (sc(SEGMENT_W) + sc(SEGMENT_GAP)) * PROVIDER_SEGMENT_COUNT - sc(SEGMENT_GAP)
}

fn usage_label_x() -> i32 {
    sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN)
}

fn usage_bar_x() -> i32 {
    usage_label_x() + sc(LABEL_WIDTH) + sc(LABEL_RIGHT_MARGIN)
}

/// Vertical Y origins for each usage row inside the widget.
/// One row is centered; two rows keep the existing dual-row layout.
fn row_ys(height: i32, row_count: usize) -> Vec<i32> {
    let seg_h = sc(SEGMENT_H);
    match row_count {
        0 => Vec::new(),
        1 => vec![(height - seg_h) / 2],
        _ => {
            let row2_y = height - sc(7) - seg_h;
            let row1_y = row2_y - sc(6) - seg_h;
            vec![row1_y, row2_y]
        }
    }
}

fn bar_rect_for_row(row_index: usize, row_count: usize) -> Option<RECT> {
    let height = sc(WIDGET_HEIGHT);
    let bar_x = usage_bar_x();
    let top = *row_ys(height, row_count).get(row_index)?;
    Some(RECT {
        left: bar_x,
        top,
        right: bar_x + provider_bar_width(),
        bottom: top + sc(SEGMENT_H),
    })
}

fn reset_tooltip_text(label: &str, resets_at: Option<SystemTime>) -> String {
    match resets_at.and_then(format_human_local_reset_time) {
        Some(reset) => format!("{label} resets {reset}"),
        None => format!("{label} reset time unavailable"),
    }
}

fn format_human_local_reset_time(time: SystemTime) -> Option<String> {
    let duration = time.duration_since(UNIX_EPOCH).ok()?;
    let ticks = duration
        .as_secs()
        .checked_add(SEC_TO_UNIX_EPOCH)?
        .checked_mul(WINDOWS_TICK)?
        .checked_add((duration.subsec_nanos() / 100) as u64)?;

    let file_time = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();

    unsafe {
        FileTimeToSystemTime(&file_time, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }

    let weekday = match local.wDayOfWeek {
        0 => "Sun",
        1 => "Mon",
        2 => "Tue",
        3 => "Wed",
        4 => "Thu",
        5 => "Fri",
        6 => "Sat",
        _ => "",
    };
    let month = match local.wMonth {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "",
    };
    Some(format!(
        "{weekday} {month} {:02} {:02}:{:02}:{:02}",
        local.wDay, local.wHour, local.wMinute, local.wSecond
    ))
}

fn usage_tooltip_text(state: &AppState) -> String {
    let strings = localization::strings();
    if state.last_poll_ok {
        if let Some(data) = state.codex_data.as_ref() {
            if !data.windows.is_empty() {
                return data
                    .windows
                    .iter()
                    .map(|window| {
                        reset_tooltip_text(
                            &poller::window_label(window, strings),
                            window.resets_at,
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
            }
        }
    }

    // Loading / error fallback matches current placeholder rows.
    state
        .display_rows
        .iter()
        .map(|row| reset_tooltip_text(&row.label, None))
        .collect::<Vec<_>>()
        .join("\n")
}

fn point_is_over_usage_bar(x: i32, y: i32) -> bool {
    let row_count = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| s.display_rows.len().max(1))
            .unwrap_or(1)
    };
    let pt = POINT { x, y };
    for index in 0..row_count {
        let Some(mut rect) = bar_rect_for_row(index, row_count) else {
            continue;
        };
        rect.left -= sc(3);
        rect.top -= sc(4);
        rect.right += sc(3);
        rect.bottom += sc(4);
        if point_in_rect(pt, rect) {
            return true;
        }
    }
    false
}

fn track_mouse_leave(hwnd: HWND) {
    unsafe {
        let mut event = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        let _ = TrackMouseEvent(&mut event);
    }
}

fn show_usage_tooltip(hwnd: HWND) {
    let (tooltip, tooltip_text) = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        let Some(tooltip) = s.tooltip_hwnd else {
            return;
        };
        if !s.mouse_tracking {
            s.mouse_tracking = true;
            track_mouse_leave(hwnd);
        }
        s.tooltip_visible = true;
        s.tooltip_text = usage_tooltip_text(s);
        (tooltip, s.tooltip_text.clone())
    };

    unsafe {
        let widget_rect = native_interop::get_window_rect_safe(hwnd).unwrap_or_default();
        let text = native_interop::wide_str(&tooltip_text);
        let _ = SetWindowTextW(tooltip, PCWSTR::from_raw(text.as_ptr()));
        let x = widget_rect.left + usage_label_x() - sc(TOOLTIP_TEXT_PADDING_X);
        let width = tooltip_width_for_text(tooltip, &tooltip_text);
        let line_count = tooltip_text.lines().count().max(1) as i32;
        // One line for a single window, two lines when both 5h + 7d are present.
        let height = sc(18 + line_count * 14).max(sc(TOOLTIP_HEIGHT / 2));
        let mut y = widget_rect.top - height - sc(6);
        if y < 0 {
            y = widget_rect.bottom + sc(6);
        }
        let _ = SetWindowPos(
            tooltip,
            HWND_TOPMOST,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let rgn = CreateRoundRectRgn(
            0,
            0,
            width + 1,
            height + 1,
            sc(TOOLTIP_RADIUS) * 2,
            sc(TOOLTIP_RADIUS) * 2,
        );
        let _ = SetWindowRgn(tooltip, rgn, true);
        let _ = InvalidateRect(tooltip, None, false);
    }
}

unsafe fn tooltip_width_for_text(hwnd: HWND, text: &str) -> i32 {
    let hdc = GetDC(hwnd);
    if hdc.is_invalid() {
        return sc(TOOLTIP_MIN_WIDTH);
    }

    let font = create_tooltip_font();
    let old_font = SelectObject(hdc, font);
    let mut text_wide: Vec<u16> = text.encode_utf16().collect();
    let mut text_rect = RECT {
        left: 0,
        top: 0,
        right: sc(TOOLTIP_MEASURE_MAX_WIDTH),
        bottom: sc(TOOLTIP_HEIGHT),
    };
    let _ = DrawTextW(
        hdc,
        &mut text_wide,
        &mut text_rect,
        DT_LEFT | DT_NOPREFIX | DT_CALCRECT,
    );

    SelectObject(hdc, old_font);
    let _ = DeleteObject(font);
    let _ = ReleaseDC(hwnd, hdc);

    (text_rect.right - text_rect.left + sc(TOOLTIP_TEXT_PADDING_X * 2)).max(sc(TOOLTIP_MIN_WIDTH))
}

fn hide_usage_tooltip() {
    let tooltip = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        let Some(tooltip) = s.tooltip_hwnd else {
            return;
        };
        if !s.tooltip_visible && !s.mouse_tracking {
            return;
        }
        s.tooltip_visible = false;
        s.mouse_tracking = false;
        tooltip
    };

    unsafe {
        let _ = ShowWindow(tooltip, SW_HIDE);
    }
}

fn install_usage_tooltip(hwnd: HWND, hinstance: HINSTANCE) {
    unsafe {
        let tooltip = match CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(native_interop::wide_str("CodexWindowsTaskbarTooltip").as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            sc(TOOLTIP_MIN_WIDTH),
            sc(TOOLTIP_HEIGHT),
            hwnd,
            HMENU::default(),
            hinstance,
            None,
        ) {
            Ok(tooltip) if !tooltip.0.is_null() => tooltip,
            _ => {
                diagnose::log("usage tooltip creation failed");
                return;
            }
        };

        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.tooltip_hwnd = Some(tooltip);
            s.tooltip_text = usage_tooltip_text(s);
        }
    }
}

fn update_usage_tooltip() {
    let tooltip = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        let Some(tooltip) = s.tooltip_hwnd else {
            return;
        };
        s.tooltip_text = usage_tooltip_text(s);
        tooltip
    };

    unsafe {
        let _ = InvalidateRect(tooltip, None, false);
    }
}

unsafe extern "system" fn tooltip_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_usage_tooltip(hdc, hwnd);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn paint_usage_tooltip(hdc: HDC, hwnd: HWND) {
    let (is_dark, text) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (s.is_dark, s.tooltip_text.clone()),
            None => (theme::is_dark_mode(), String::new()),
        }
    };

    unsafe {
        let mut rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut rect);

        let bg = if is_dark {
            Color::from_hex("#2D2D2D")
        } else {
            Color::from_hex("#FAFAFA")
        };
        let text_color = if is_dark {
            Color::from_hex("#F2F2F2")
        } else {
            Color::from_hex("#202020")
        };

        let bg_brush = CreateSolidBrush(COLORREF(bg.to_colorref()));
        let old_brush = SelectObject(hdc, bg_brush);
        let old_pen = SelectObject(hdc, CreatePen(PS_SOLID, sc(1), COLORREF(bg.to_colorref())));
        let _ = RoundRect(
            hdc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            sc(TOOLTIP_RADIUS) * 2,
            sc(TOOLTIP_RADIUS) * 2,
        );
        let pen = SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
        let _ = DeleteObject(bg_brush);

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
        let font = create_tooltip_font();
        let old_font = SelectObject(hdc, font);

        let mut text_rect = RECT {
            left: sc(TOOLTIP_TEXT_PADDING_X),
            top: sc(3),
            right: rect.right - sc(TOOLTIP_TEXT_PADDING_X),
            bottom: rect.bottom - sc(2),
        };
        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let _ = DrawTextW(hdc, &mut text_wide, &mut text_rect, DT_LEFT | DT_NOPREFIX);

        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

unsafe fn create_tooltip_font() -> HFONT {
    let font_name = native_interop::wide_str(UI_MONO_FONT);
    CreateFontW(
        sc(-11),
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_TT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
        PCWSTR::from_raw(font_name.as_ptr()),
    )
}

pub fn run() {
    // Enable Per-Monitor DPI Awareness V2 for crisp rendering at any scale factor
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CURRENT_DPI.store(GetDpiForSystem(), Ordering::Relaxed);
    }
    diagnose::log("window::run started");

    // Single-instance guard: silently exit if another instance is running
    let mutex_name = native_interop::wide_str("Global\\CodexWindowsTaskbar");
    let _mutex = unsafe {
        let handle = CreateMutexW(None, false, PCWSTR::from_raw(mutex_name.as_ptr()));
        match handle {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    diagnose::log("startup aborted: another instance is already running");
                    return;
                }
                h
            }
            Err(error) => {
                diagnose::log_error(
                    "startup aborted: unable to create single-instance mutex",
                    error,
                );
                return;
            }
        }
    };

    let class_name = native_interop::wide_str("CodexWindowsTaskbar");

    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap();
        let (large_icon, small_icon) = load_embedded_app_icons();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hIcon: large_icon,
            hIconSm: small_icon,
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };

        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            diagnose::log("RegisterClassExW returned 0");
        }

        let tooltip_class_name = native_interop::wide_str("CodexWindowsTaskbarTooltip");
        let tooltip_wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(tooltip_wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(tooltip_class_name.as_ptr()),
            ..Default::default()
        };

        let tooltip_atom = RegisterClassExW(&tooltip_wc);
        if tooltip_atom == 0 {
            diagnose::log("tooltip RegisterClassExW returned 0");
        }

        let settings = load_settings();

        // Create as layered popup (will be reparented into taskbar)
        let title = native_interop::wide_str(localization::strings().window_title);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            total_widget_width(),
            sc(WIDGET_HEIGHT),
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .unwrap();

        if !large_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(large_icon.0 as isize),
            );
        }
        if !small_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(small_icon.0 as isize),
            );
        }

        diagnose::log(format!("main window created hwnd={:?}", hwnd));

        let is_dark = theme::is_dark_mode();
        let mut embedded = false;
        let strings = localization::strings();

        {
            let mut state = lock_state();
            *state = Some(AppState {
                hwnd: SendHwnd::from_hwnd(hwnd),
                taskbar_hwnd: None,
                tray_notify_hwnd: None,
                tooltip_hwnd: None,
                tooltip_visible: false,
                mouse_tracking: false,
                tooltip_text: String::new(),
                win_event_hook: None,
                is_dark,
                embedded: false,
                display_rows: default_display_rows(strings),
                codex_data: None,
                poll_interval_ms: settings.poll_interval_ms,
                retry_count: 0,
                last_poll_ok: false,
                tray_offset: settings.tray_offset,
                dragging: false,
                drag_start_mouse_x: 0,
                drag_start_offset: 0,
                widget_visible: settings.widget_visible,
            });
        }

        install_usage_tooltip(hwnd, HINSTANCE(hinstance.0));

        // Try to embed in taskbar
        if attach_to_taskbar(hwnd) {
            embedded = true;
        } else {
            diagnose::log("taskbar not found; using fallback popup window");
        }

        // If not embedded, fall back to topmost popup with SetLayeredWindowAttributes
        if !embedded {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            // Explorer may not be ready at login; retry attachment shortly.
            SetTimer(hwnd, TIMER_TASKBAR_RETRY, 2_000, None);
        }

        // Register system tray icon
        let (tray_pct, tray_tooltip) = tray_icon_data_from_state();
        tray_icon::add(hwnd, tray_pct, &tray_tooltip);
        install_keyboard_refresh_hook();

        // Position and show (only if widget_visible preference is true)
        position_at_taskbar();
        if settings.widget_visible {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        diagnose::log("window shown");

        // Initial render via UpdateLayeredWindow (for embedded) or InvalidateRect (fallback)
        render_layered();

        // Poll timer: 15 minutes
        let initial_poll_ms = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| s.poll_interval_ms)
                .unwrap_or(POLL_15_MIN)
        };
        SetTimer(hwnd, TIMER_POLL, initial_poll_ms, None);

        // Initial poll
        let send_hwnd = SendHwnd::from_hwnd(hwnd);
        std::thread::spawn(move || {
            diagnose::log("initial poll thread started");
            do_poll(send_hwnd);
        });

        // Initial theme check
        check_theme_change();

        // Message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Render widget content and push to the layered window via UpdateLayeredWindow.
/// Renders fully opaque with the actual taskbar background colour so that
/// ClearType sub-pixel font rendering can be used for crisp, OS-native text.
fn render_layered() {
    refresh_dpi();
    let (hwnd_val, is_dark, embedded, display_rows) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (s.hwnd, s.is_dark, s.embedded, s.display_rows.clone()),
            None => return,
        }
    };

    let hwnd = hwnd_val.to_hwnd();

    // For non-embedded fallback, just invalidate and let WM_PAINT handle it
    if !embedded {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
        return;
    }

    let width = total_widget_width();
    let height = sc(WIDGET_HEIGHT);

    let codex_accent = Color::from_hex("#3BAFDA");
    let track = if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let text_color = if is_dark {
        Color::from_hex("#888888")
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

    unsafe {
        let screen_dc = GetDC(hwnd);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();

        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(hwnd, screen_dc);
            return;
        }

        let old_bmp = SelectObject(mem_dc, dib);
        let pixel_count = (width * height) as usize;

        // Render once with the actual taskbar background colour.
        // Using an opaque background lets us use CLEARTYPE_QUALITY for
        // sub-pixel font rendering that matches the rest of the OS.
        paint_content(
            mem_dc,
            width,
            height,
            is_dark,
            &bg_color,
            &text_color,
            &codex_accent,
            &track,
            &display_rows,
        );

        // Background pixels → alpha 1 (nearly invisible but still hittable for right-click).
        // Content pixels → fully opaque (preserves ClearType sub-pixel rendering).
        let bg_bgr = bg_color.to_colorref();
        let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
        for px in pixel_data.iter_mut() {
            let rgb = *px & 0x00FFFFFF;
            if rgb == bg_bgr {
                *px = 0x01000000;
            } else {
                *px = rgb | 0xFF000000;
            }
        }

        // Push to window via UpdateLayeredWindow
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: width,
            cy: height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: 0, // AC_SRC_OVER
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1, // AC_SRC_ALPHA
        };

        let _ = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            None,
            Some(&sz),
            mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        // Cleanup
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(hwnd, screen_dc);
    }
}

/// Paint all widget content onto a DC with a given background color.
fn paint_content(
    hdc: HDC,
    width: i32,
    height: i32,
    is_dark: bool,
    bg: &Color,
    text_color: &Color,
    codex_accent: &Color,
    track: &Color,
    rows: &[DisplayRow],
) {
    unsafe {
        let client_rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };

        let bg_brush = CreateSolidBrush(COLORREF(bg.to_colorref()));
        FillRect(hdc, &client_rect, bg_brush);
        let _ = DeleteObject(bg_brush);

        // Left divider
        let divider_h = sc(25);
        let divider_top = (height - divider_h) / 2;
        let divider_bottom = divider_top + divider_h;

        let (div_left, div_right) = if is_dark {
            ((80, 80, 80), (40, 40, 40))
        } else {
            ((160, 160, 160), (230, 230, 230))
        };

        let left_brush = CreateSolidBrush(COLORREF(native_interop::colorref(
            div_left.0, div_left.1, div_left.2,
        )));
        let left_rect = RECT {
            left: 0,
            top: divider_top,
            right: sc(2),
            bottom: divider_bottom,
        };
        FillRect(hdc, &left_rect, left_brush);
        let _ = DeleteObject(left_brush);

        let right_brush = CreateSolidBrush(COLORREF(native_interop::colorref(
            div_right.0,
            div_right.1,
            div_right.2,
        )));
        let right_rect = RECT {
            left: sc(2),
            top: divider_top,
            right: sc(3),
            bottom: divider_bottom,
        };
        FillRect(hdc, &right_rect, right_brush);
        let _ = DeleteObject(right_brush);

        let content_x = usage_label_x();
        let ys = row_ys(height, rows.len().max(1));

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));

        let font_name = native_interop::wide_str(UI_MONO_FONT);
        let font = CreateFontW(
            sc(-12),
            0,
            0,
            0,
            FW_MEDIUM.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let old_font = SelectObject(hdc, font);

        for (row, y) in rows.iter().zip(ys.iter()) {
            draw_row(
                hdc,
                content_x,
                *y,
                &row.label,
                row.percent,
                &row.text,
                codex_accent,
                track,
            );
        }

        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

fn request_refresh(hwnd: HWND) {
    {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.display_rows.is_empty() {
                s.display_rows = default_display_rows(localization::strings());
            }
            for row in &mut s.display_rows {
                row.text = "...".to_string();
            }
        }
    }
    render_layered();
    let sh = SendHwnd::from_hwnd(hwnd);
    std::thread::spawn(move || {
        do_poll(sh);
    });
}

fn do_poll(send_hwnd: SendHwnd) {
    let hwnd = send_hwnd.to_hwnd();
    let codex_result = poller::poll_codex();

    if let Ok(data) = codex_result {
        {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                let any_past_reset = poller::is_past_reset(&data);
                s.codex_data = Some(data);

                if !any_past_reset {
                    unsafe {
                        let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                    }
                }

                s.last_poll_ok = true;
                refresh_usage_texts(s);

                // Recovered from errors — restore normal poll interval
                if s.retry_count > 0 {
                    s.retry_count = 0;
                    let interval = s.poll_interval_ms;
                    unsafe {
                        SetTimer(hwnd, TIMER_POLL, interval, None);
                    }
                }
            }
        }

        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
        }
    } else {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.last_poll_ok = false;
            if s.display_rows.is_empty() {
                s.display_rows = default_display_rows(localization::strings());
            }
            for row in &mut s.display_rows {
                row.text = "...".to_string();
            }
            s.retry_count = s.retry_count.saturating_add(1);
            let backoff = RETRY_BASE_MS
                .saturating_mul(1u32.checked_shl(s.retry_count - 1).unwrap_or(u32::MAX));
            let retry_ms = backoff.min(s.poll_interval_ms);
            unsafe {
                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                SetTimer(hwnd, TIMER_POLL, retry_ms, None);
            }
        }

        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
        }
    }
}

fn schedule_countdown_timer() {
    let state = lock_state();
    let s = match state.as_ref() {
        Some(s) => s,
        None => return,
    };

    let hwnd = s.hwnd.to_hwnd();
    if !s.last_poll_ok {
        unsafe {
            let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
            let _ = KillTimer(hwnd, TIMER_RESET_POLL);
        }
        return;
    }

    // If a reset time has passed, poll every 5s to pick up fresh data
    if s.codex_data
        .as_ref()
        .map_or(false, |data| poller::is_past_reset(data))
    {
        unsafe {
            SetTimer(hwnd, TIMER_RESET_POLL, 5_000, None);
        }
    }

    let mut min_delay: Option<Duration> = None;
    for data in [&s.codex_data].into_iter().flatten() {
        for window in &data.windows {
            if let Some(delay) = poller::time_until_display_change(window.resets_at) {
                min_delay = Some(min_delay.map_or(delay, |current| current.min(delay)));
            }
        }
    }

    let ms = min_delay
        .unwrap_or(Duration::from_secs(60))
        .as_millis()
        .max(1000) as u32;

    unsafe {
        SetTimer(hwnd, TIMER_COUNTDOWN, ms, None);
    }
}

fn check_theme_change() {
    let new_dark = theme::is_dark_mode();
    let changed = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.is_dark != new_dark {
                s.is_dark = new_dark;
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if changed {
        render_layered();
    }
}

fn update_display() {
    let mut state = lock_state();
    let s = match state.as_mut() {
        Some(s) => s,
        None => return,
    };

    // Don't overwrite error text with stale cached data
    if !s.last_poll_ok {
        return;
    }

    refresh_usage_texts(s);
}

fn suppress_tray_reposition_for(duration: Duration) {
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *until = Some(Instant::now() + duration);
}

fn tray_reposition_is_suppressed() -> bool {
    let now = Instant::now();
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    match *until {
        Some(deadline) if now < deadline => true,
        Some(_) => {
            *until = None;
            false
        }
        None => false,
    }
}

/// Find the taskbar, reparent the widget into it, and install the tray hook.
/// Returns true when embedding succeeds.
fn attach_to_taskbar(hwnd: HWND) -> bool {
    let Some(taskbar_hwnd) = native_interop::find_taskbar() else {
        return false;
    };
    if !native_interop::is_window(taskbar_hwnd) {
        return false;
    }

    diagnose::log(format!("taskbar found hwnd={:?}", taskbar_hwnd));
    native_interop::embed_in_taskbar(hwnd, taskbar_hwnd);

    let tray_notify = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd");
    if tray_notify.is_some() {
        diagnose::log("TrayNotifyWnd found");
    } else {
        diagnose::log("TrayNotifyWnd not found");
    }

    let old_hook = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return false;
        };
        let old_hook = s.win_event_hook.take();
        s.taskbar_hwnd = Some(taskbar_hwnd);
        s.tray_notify_hwnd = tray_notify;
        s.embedded = true;
        old_hook
    };

    if let Some(hook) = old_hook {
        native_interop::unhook_win_event(hook);
    }

    if let Some(tray_hwnd) = tray_notify {
        let thread_id = native_interop::get_window_thread_id(tray_hwnd);
        let hook = native_interop::set_tray_event_hook(thread_id, on_tray_location_changed);
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.win_event_hook = hook;
        }
        if hook.is_some() {
            diagnose::log("tray event hook installed");
        } else {
            diagnose::log("tray event hook could not be installed");
        }
    }

    true
}

/// Recover from explorer restarts / lost parents so the widget stays on the taskbar.
fn ensure_taskbar_attachment() {
    let (hwnd, taskbar_hwnd, tray_notify_hwnd, embedded, widget_visible) = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };
        (
            s.hwnd.to_hwnd(),
            s.taskbar_hwnd,
            s.tray_notify_hwnd,
            s.embedded,
            s.widget_visible,
        )
    };

    let taskbar_alive = taskbar_hwnd
        .map(native_interop::is_window)
        .unwrap_or(false);

    if !taskbar_alive {
        diagnose::log("taskbar handle invalid; attempting re-attach");
        if attach_to_taskbar(hwnd) {
            if widget_visible {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
            }
            unsafe {
                let _ = KillTimer(hwnd, TIMER_TASKBAR_RETRY);
            }
        } else {
            // Keep retrying until explorer is back.
            unsafe {
                SetTimer(hwnd, TIMER_TASKBAR_RETRY, 2_000, None);
            }
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.taskbar_hwnd = None;
                s.tray_notify_hwnd = None;
                s.embedded = false;
            }
        }
        return;
    }

    let taskbar_hwnd = taskbar_hwnd.expect("checked alive");

    // Child can become orphaned (parent desktop / null) after explorer restarts.
    if embedded {
        let parent = native_interop::get_parent(hwnd);
        if parent != taskbar_hwnd {
            diagnose::log("widget parent lost; re-embedding into taskbar");
            native_interop::embed_in_taskbar(hwnd, taskbar_hwnd);
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.embedded = true;
            }
            if widget_visible {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
            }
        }
    } else if attach_to_taskbar(hwnd) {
        if widget_visible {
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
        }
        unsafe {
            let _ = KillTimer(hwnd, TIMER_TASKBAR_RETRY);
        }
        return;
    }

    let tray_alive = tray_notify_hwnd
        .map(native_interop::is_window)
        .unwrap_or(false);
    if !tray_alive {
        let tray_notify = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd");
        let old_hook = {
            let mut state = lock_state();
            let Some(s) = state.as_mut() else {
                return;
            };
            let old_hook = s.win_event_hook.take();
            s.tray_notify_hwnd = tray_notify;
            old_hook
        };
        if let Some(hook) = old_hook {
            native_interop::unhook_win_event(hook);
        }
        if let Some(tray_hwnd) = tray_notify {
            let thread_id = native_interop::get_window_thread_id(tray_hwnd);
            let hook = native_interop::set_tray_event_hook(thread_id, on_tray_location_changed);
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.win_event_hook = hook;
            }
        }
    }
}

fn position_at_taskbar() {
    refresh_dpi();
    ensure_taskbar_attachment();

    // Drop the app-state lock before any Win32 call that may synchronously
    // re-enter our window procedure.
    let (hwnd, embedded, tray_offset, taskbar_hwnd, widget_visible) = {
        let state = lock_state();
        let s = match state.as_ref() {
            Some(s) => s,
            None => return,
        };

        // Don't fight the user's drag
        if s.dragging {
            return;
        }

        let taskbar_hwnd = match s.taskbar_hwnd {
            Some(h) if native_interop::is_window(h) => h,
            _ => {
                diagnose::log("position_at_taskbar skipped: no taskbar handle");
                return;
            }
        };

        (
            s.hwnd.to_hwnd(),
            s.embedded,
            s.tray_offset,
            taskbar_hwnd,
            s.widget_visible,
        )
    };

    let taskbar_rect = match native_interop::get_taskbar_rect(taskbar_hwnd) {
        Some(r) if r.right > r.left && r.bottom > r.top => r,
        _ => {
            diagnose::log("position_at_taskbar skipped: unable to query taskbar rect");
            return;
        }
    };

    let taskbar_width = taskbar_rect.right - taskbar_rect.left;
    let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
    let mut tray_left = taskbar_rect.right;
    let anchor_top = taskbar_rect.top;
    let anchor_height = taskbar_height;

    if let Some(tray_hwnd) = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd") {
        if let Some(tray_rect) = native_interop::get_window_rect_safe(tray_hwnd) {
            // Ignore stale/zero tray rects that can push the widget off-screen.
            if tray_rect.left >= taskbar_rect.left && tray_rect.left <= taskbar_rect.right {
                tray_left = tray_rect.left;
            }
        }
    }

    let widget_width = total_widget_width();
    let widget_height = sc(WIDGET_HEIGHT);
    let y = compute_anchor_y(anchor_top, anchor_height, widget_height);

    if embedded {
        // Child window: coordinates relative to parent (taskbar)
        let mut x =
            tray_left - taskbar_rect.left - widget_width - tray_offset + sc(TRAY_EDGE_OVERLAP);
        let max_x = (taskbar_width - widget_width).max(0);
        x = x.clamp(0, max_x);
        let mut rel_y = y - taskbar_rect.top;
        let max_y = (taskbar_height - widget_height).max(0);
        rel_y = rel_y.clamp(0, max_y);
        native_interop::move_window(hwnd, x, rel_y, widget_width, widget_height);
        diagnose::log(format!(
            "positioned embedded widget at x={x} y={rel_y} w={widget_width} h={widget_height}"
        ));
    } else {
        // Topmost popup: screen coordinates
        let mut x = tray_left - widget_width - tray_offset + sc(TRAY_EDGE_OVERLAP);
        let min_x = taskbar_rect.left;
        let max_x = (taskbar_rect.right - widget_width).max(min_x);
        x = x.clamp(min_x, max_x);
        let mut screen_y = y;
        let max_y = (taskbar_rect.bottom - widget_height).max(taskbar_rect.top);
        screen_y = screen_y.clamp(taskbar_rect.top, max_y);
        native_interop::move_window(hwnd, x, screen_y, widget_width, widget_height);
        diagnose::log(format!(
            "positioned fallback widget at x={x} y={screen_y} w={widget_width} h={widget_height}"
        ));
    }

    if widget_visible {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }
}

fn compute_anchor_y(anchor_top: i32, anchor_height: i32, widget_height: i32) -> i32 {
    let anchor_bottom = anchor_top + anchor_height;
    (anchor_bottom - widget_height).max(anchor_top)
}

/// WinEvent callback for tray icon location changes
unsafe extern "system" fn on_tray_location_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    static LAST_REPOSITION: Mutex<Option<std::time::Instant>> = Mutex::new(None);

    let is_tray = {
        let state = lock_state();
        state
            .as_ref()
            .and_then(|s| s.tray_notify_hwnd)
            .map(|h| h == hwnd)
            .unwrap_or(false)
    };

    if is_tray {
        if tray_reposition_is_suppressed() {
            return;
        }

        let should_reposition = {
            let mut last = LAST_REPOSITION.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            if last
                .map(|t| now.duration_since(t).as_millis() > 500)
                .unwrap_or(true)
            {
                *last = Some(now);
                true
            } else {
                false
            }
        };
        if should_reposition {
            ensure_taskbar_attachment();
            position_at_taskbar();
            render_layered();
        }
    }
}

/// Main window procedure
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            // For non-embedded fallback, paint normally
            let embedded = {
                let state = lock_state();
                state.as_ref().map(|s| s.embedded).unwrap_or(false)
            };
            if embedded {
                // Layered windows don't use WM_PAINT; just validate the region
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            } else {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint(hdc, hwnd);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DISPLAYCHANGE | WM_DPICHANGED_MSG | WM_SETTINGCHANGE => {
            if msg == WM_DPICHANGED_MSG {
                let new_dpi = (wparam.0 & 0xFFFF) as u32;
                CURRENT_DPI.store(new_dpi, Ordering::Relaxed);
            }
            if msg == WM_SETTINGCHANGE {
                check_theme_change();
            }
            refresh_dpi();
            update_usage_tooltip();
            ensure_taskbar_attachment();
            position_at_taskbar();
            render_layered();
            LRESULT(0)
        }
        WM_TIMER => {
            let timer_id = wparam.0;
            match timer_id {
                TIMER_POLL => {
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                }
                TIMER_COUNTDOWN => {
                    update_display();
                    render_layered();
                    schedule_countdown_timer();
                }
                TIMER_RESET_POLL => {
                    let should_poll = {
                        let state = lock_state();
                        state.as_ref().is_some()
                    };
                    if should_poll {
                        let sh = SendHwnd::from_hwnd(hwnd);
                        std::thread::spawn(move || {
                            do_poll(sh);
                        });
                    }
                }
                TIMER_TASKBAR_RETRY => {
                    ensure_taskbar_attachment();
                    position_at_taskbar();
                    render_layered();
                    let embedded = {
                        let state = lock_state();
                        state.as_ref().map(|s| s.embedded).unwrap_or(false)
                    };
                    if embedded {
                        let _ = KillTimer(hwnd, TIMER_TASKBAR_RETRY);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_USAGE_UPDATED => {
            check_theme_change();
            update_usage_tooltip();
            render_layered();
            schedule_countdown_timer();
            let (pct, tooltip) = tray_icon_data_from_state();
            suppress_tray_reposition_for(Duration::from_millis(
                TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS,
            ));
            tray_icon::update(hwnd, pct, &tooltip);
            LRESULT(0)
        }
        WM_APP_REFRESH_SHORTCUT => {
            request_refresh(hwnd);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            // Always show resize cursor while dragging or when hovering divider zone
            let hit_test = (lparam.0 & 0xFFFF) as u16;
            if is_dragging {
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            if hit_test == 1 {
                // HTCLIENT
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                if pt.x < sc(DIVIDER_HIT_ZONE) {
                    let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                    SetCursor(cursor);
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let client_x = (lparam.0 & 0xFFFF) as i16 as i32;
            if client_x < sc(DIVIDER_HIT_ZONE) {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.dragging = true;
                    s.drag_start_mouse_x = pt.x;
                    s.drag_start_offset = s.tray_offset;
                }
                SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let client_x = (lparam.0 & 0xFFFF) as i16 as i32;
            let client_y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            if !is_dragging {
                if point_is_over_usage_bar(client_x, client_y) {
                    show_usage_tooltip(hwnd);
                } else {
                    hide_usage_tooltip();
                }
            } else {
                hide_usage_tooltip();
            }
            if is_dragging {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let move_target = {
                    let mut state = lock_state();
                    let s = match state.as_mut() {
                        Some(s) => s,
                        None => return LRESULT(0),
                    };

                    // Moving mouse left = positive delta = larger offset (further left)
                    let delta = s.drag_start_mouse_x - pt.x;
                    let mut new_offset = s.drag_start_offset + delta;

                    // Clamp: offset >= 0 (can't go right of default)
                    if new_offset < 0 {
                        new_offset = 0;
                    }

                    let taskbar_hwnd = s.taskbar_hwnd;
                    let embedded = s.embedded;
                    let hwnd_val = s.hwnd.to_hwnd();

                    // Clamp: don't go past left edge of taskbar
                    if let Some(taskbar_hwnd) = taskbar_hwnd {
                        if let Some(taskbar_rect) = native_interop::get_taskbar_rect(taskbar_hwnd) {
                            let mut tray_left = taskbar_rect.right;
                            if let Some(tray_hwnd) =
                                native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd")
                            {
                                if let Some(tray_rect) =
                                    native_interop::get_window_rect_safe(tray_hwnd)
                                {
                                    tray_left = tray_rect.left;
                                }
                            }
                            let widget_width = total_widget_width();
                            let max_offset = tray_left - taskbar_rect.left - widget_width
                                + sc(TRAY_EDGE_OVERLAP);
                            if new_offset > max_offset {
                                new_offset = max_offset;
                            }

                            s.tray_offset = new_offset;

                            let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
                            let anchor_top = taskbar_rect.top;
                            let anchor_height = taskbar_height;
                            let widget_height = sc(WIDGET_HEIGHT);
                            let y = compute_anchor_y(anchor_top, anchor_height, widget_height);
                            let x = if embedded {
                                tray_left - taskbar_rect.left - widget_width - new_offset
                                    + sc(TRAY_EDGE_OVERLAP)
                            } else {
                                tray_left - widget_width - new_offset + sc(TRAY_EDGE_OVERLAP)
                            };
                            Some((
                                hwnd_val,
                                embedded,
                                x,
                                y,
                                taskbar_rect.top,
                                widget_width,
                                widget_height,
                            ))
                        } else {
                            s.tray_offset = new_offset;
                            None
                        }
                    } else {
                        s.tray_offset = new_offset;
                        None
                    }
                };

                if let Some((hwnd_val, embedded, x, y, taskbar_top, widget_width, widget_height)) =
                    move_target
                {
                    if embedded {
                        native_interop::move_window(
                            hwnd_val,
                            x,
                            y - taskbar_top,
                            widget_width,
                            widget_height,
                        );
                    } else {
                        native_interop::move_window(hwnd_val, x, y, widget_width, widget_height);
                    }
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_MSG => {
            hide_usage_tooltip();
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let was_dragging = {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.dragging {
                        s.dragging = false;
                        let offset = s.tray_offset;
                        Some(offset)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if was_dragging.is_some() {
                let _ = ReleaseCapture();
                save_state_settings();
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_context_menu(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 as u16;
            match id {
                1 => {
                    request_refresh(hwnd);
                }
                2 => {
                    let hook = {
                        let state = lock_state();
                        state.as_ref().and_then(|s| s.win_event_hook)
                    };
                    if let Some(h) = hook {
                        native_interop::unhook_win_event(h);
                    }
                    uninstall_keyboard_refresh_hook();
                    PostQuitMessage(0);
                }
                IDM_RESET_POSITION => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.tray_offset = 0;
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                }
                IDM_START_WITH_WINDOWS => {
                    set_startup_enabled(!is_startup_enabled());
                }
                IDM_FREQ_1MIN | IDM_FREQ_5MIN | IDM_FREQ_15MIN | IDM_FREQ_1HOUR => {
                    let new_interval = match id {
                        IDM_FREQ_1MIN => POLL_1_MIN,
                        IDM_FREQ_5MIN => POLL_5_MIN,
                        IDM_FREQ_15MIN => POLL_15_MIN,
                        IDM_FREQ_1HOUR => POLL_1_HOUR,
                        _ => POLL_15_MIN,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.poll_interval_ms = new_interval;
                        }
                    }
                    save_state_settings();
                    // Reset the poll timer with the new interval
                    SetTimer(hwnd, TIMER_POLL, new_interval, None);
                }
                id if id == tray_icon::IDM_TOGGLE_WIDGET => {
                    toggle_widget_visibility(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ if msg == WM_APP_TRAY => {
            match tray_icon::handle_message(lparam) {
                tray_icon::TrayAction::ToggleWidget => {
                    toggle_widget_visibility(hwnd);
                }
                tray_icon::TrayAction::ShowContextMenu => {
                    show_context_menu(hwnd);
                }
                tray_icon::TrayAction::None => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let hook = {
                let state = lock_state();
                state.as_ref().and_then(|s| s.win_event_hook)
            };
            if let Some(h) = hook {
                native_interop::unhook_win_event(h);
            }
            uninstall_keyboard_refresh_hook();
            tray_icon::remove(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn show_context_menu(hwnd: HWND) {
    unsafe {
        let (current_interval, strings, widget_visible) = {
            let state = lock_state();
            match state.as_ref() {
                Some(s) => (
                    s.poll_interval_ms,
                    localization::strings(),
                    s.widget_visible,
                ),
                None => (POLL_15_MIN, localization::strings(), true),
            }
        };

        let menu = CreatePopupMenu().unwrap();

        let refresh_str = native_interop::wide_str(strings.refresh);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            1,
            PCWSTR::from_raw(refresh_str.as_ptr()),
        );

        // Update Frequency submenu
        let freq_menu = CreatePopupMenu().unwrap();
        let freq_items: [(u16, u32, &str); 4] = [
            (IDM_FREQ_1MIN, POLL_1_MIN, strings.one_minute),
            (IDM_FREQ_5MIN, POLL_5_MIN, strings.five_minutes),
            (IDM_FREQ_15MIN, POLL_15_MIN, strings.fifteen_minutes),
            (IDM_FREQ_1HOUR, POLL_1_HOUR, strings.one_hour),
        ];
        for (id, interval, label) in freq_items {
            let label_str = native_interop::wide_str(label);
            let flags = if interval == current_interval {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                freq_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let freq_label = native_interop::wide_str(strings.update_frequency);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            freq_menu.0 as usize,
            PCWSTR::from_raw(freq_label.as_ptr()),
        );

        // Settings submenu
        let settings_menu = CreatePopupMenu().unwrap();

        let startup_str = native_interop::wide_str(strings.start_with_windows);
        let startup_flags = if is_startup_enabled() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            startup_flags,
            IDM_START_WITH_WINDOWS as usize,
            PCWSTR::from_raw(startup_str.as_ptr()),
        );

        let reset_pos_str = native_interop::wide_str(strings.reset_position);
        let _ = AppendMenuW(
            settings_menu,
            MENU_ITEM_FLAGS(0),
            IDM_RESET_POSITION as usize,
            PCWSTR::from_raw(reset_pos_str.as_ptr()),
        );

        let settings_label = native_interop::wide_str(strings.settings);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            settings_menu.0 as usize,
            PCWSTR::from_raw(settings_label.as_ptr()),
        );

        let widget_label = native_interop::wide_str(strings.show_widget);
        let widget_flags = if widget_visible {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            menu,
            widget_flags,
            tray_icon::IDM_TOGGLE_WIDGET as usize,
            PCWSTR::from_raw(widget_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let exit_str = native_interop::wide_str(strings.exit);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            2,
            PCWSTR::from_raw(exit_str.as_ptr()),
        );

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

/// Paint for non-embedded fallback (normal WM_PAINT path)
fn paint(hdc: HDC, hwnd: HWND) {
    let (is_dark, display_rows) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (s.is_dark, s.display_rows.clone()),
            None => return,
        }
    };

    let codex_accent = Color::from_hex("#3BAFDA");
    let track = if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let text_color = if is_dark {
        Color::from_hex("#888888")
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

    unsafe {
        let mut client_rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut client_rect);
        let width = client_rect.right - client_rect.left;
        let height = client_rect.bottom - client_rect.top;

        if width <= 0 || height <= 0 {
            return;
        }

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem_dc, mem_bmp);

        paint_content(
            mem_dc,
            width,
            height,
            is_dark,
            &bg_color,
            &text_color,
            &codex_accent,
            &track,
            &display_rows,
        );

        let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

fn draw_row(
    hdc: HDC,
    x: i32,
    y: i32,
    label: &str,
    codex_percent: f64,
    codex_text: &str,
    codex_accent: &Color,
    track: &Color,
) {
    let seg_w = sc(SEGMENT_W);
    let seg_h = sc(SEGMENT_H);
    let seg_gap = sc(SEGMENT_GAP);
    let corner_r = sc(CORNER_RADIUS);

    unsafe {
        let mut label_wide: Vec<u16> = label.encode_utf16().collect();
        let mut label_rect = RECT {
            left: x,
            top: y,
            right: x + sc(LABEL_WIDTH),
            bottom: y + seg_h,
        };
        let _ = DrawTextW(
            hdc,
            &mut label_wide,
            &mut label_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );

        let bar_x = x + sc(LABEL_WIDTH) + sc(LABEL_RIGHT_MARGIN);
        draw_segments(
            hdc,
            bar_x,
            y,
            poller::remaining_percent(codex_percent),
            codex_accent,
            track,
            seg_w,
            seg_h,
            seg_gap,
            corner_r,
        );

        let text_x = bar_x + provider_bar_width() + sc(BAR_RIGHT_MARGIN);
        let text = compact_codex_text(codex_text);
        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let mut text_rect = RECT {
            left: text_x,
            top: y,
            right: text_x + sc(TEXT_WIDTH),
            bottom: y + seg_h,
        };
        let _ = DrawTextW(
            hdc,
            &mut text_wide,
            &mut text_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

fn draw_segments(
    hdc: HDC,
    x: i32,
    y: i32,
    percent: f64,
    accent: &Color,
    track: &Color,
    seg_w: i32,
    seg_h: i32,
    seg_gap: i32,
    corner_r: i32,
) {
    let percent_clamped = percent.clamp(0.0, 100.0);
    let segment_span = 100.0 / PROVIDER_SEGMENT_COUNT as f64;

    unsafe {
        for i in 0..PROVIDER_SEGMENT_COUNT {
            let seg_x = x + i * (seg_w + seg_gap);
            let seg_start = (i as f64) * segment_span;
            let seg_end = seg_start + segment_span;

            let seg_rect = RECT {
                left: seg_x,
                top: y,
                right: seg_x + seg_w,
                bottom: y + seg_h,
            };

            if percent_clamped >= seg_end {
                draw_rounded_rect(hdc, &seg_rect, accent, corner_r);
            } else if percent_clamped <= seg_start {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
            } else {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
                let fraction = (percent_clamped - seg_start) / segment_span;
                let fill_width = (seg_w as f64 * fraction) as i32;
                if fill_width > 0 {
                    let fill_rect = RECT {
                        left: seg_x,
                        top: y,
                        right: seg_x + fill_width,
                        bottom: y + seg_h,
                    };
                    let rgn = CreateRoundRectRgn(
                        seg_rect.left,
                        seg_rect.top,
                        seg_rect.right + 1,
                        seg_rect.bottom + 1,
                        corner_r * 2,
                        corner_r * 2,
                    );
                    let _ = SelectClipRgn(hdc, rgn);
                    let brush = CreateSolidBrush(COLORREF(accent.to_colorref()));
                    FillRect(hdc, &fill_rect, brush);
                    let _ = DeleteObject(brush);
                    let _ = SelectClipRgn(hdc, HRGN::default());
                    let _ = DeleteObject(rgn);
                }
            }
        }
    }
}

fn compact_codex_text(codex_text: &str) -> String {
    compact_provider_text(codex_text, true).unwrap_or_else(|| "--".to_string())
}

fn compact_provider_text(text: &str, include_countdown: bool) -> Option<String> {
    let percent = compact_percent(text)?;
    let mut compact = percent;
    if include_countdown {
        if let Some(countdown) = compact_countdown(text) {
            compact.push(' ');
            compact.push_str(&countdown);
        }
    }
    Some(compact)
}

fn compact_percent(text: &str) -> Option<String> {
    let first = text.split_whitespace().next()?;
    if first == "--" {
        return None;
    }

    let number = first.trim_end_matches('%').parse::<u32>().ok()?;
    Some(format!("{number:>3}%"))
}

fn compact_countdown(text: &str) -> Option<String> {
    let countdown = text
        .split_whitespace()
        .skip(2)
        .collect::<Vec<_>>()
        .join(" ");

    if countdown.is_empty() {
        None
    } else {
        Some(countdown)
    }
}

fn draw_rounded_rect(hdc: HDC, rect: &RECT, color: &Color, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        let rgn = CreateRoundRectRgn(
            rect.left,
            rect.top,
            rect.right + 1,
            rect.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, brush);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(brush);
    }
}
