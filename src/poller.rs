use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use std::os::windows::process::CommandExt;

use crate::diagnose;
use crate::localization::Strings;
use crate::models::{UsageData, UsageSection};

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Debug)]
pub enum PollError {
    AuthRequired,
    NoCredentials,
    TokenExpired,
    RequestFailed,
}

#[derive(Deserialize)]
struct CodexAuth {
    tokens: Option<CodexTokens>,
}

#[derive(Clone, Deserialize)]
struct CodexTokens {
    access_token: Option<String>,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct CodexUsageResponse {
    rate_limit: Option<CodexRateLimit>,
}

#[derive(Deserialize)]
struct CodexRateLimit {
    primary_window: Option<CodexUsageBucket>,
    secondary_window: Option<CodexUsageBucket>,
}

#[derive(Deserialize)]
struct CodexUsageBucket {
    used_percent: f64,
    reset_at: Option<i64>,
}

#[derive(Deserialize)]
struct CodexRefreshResponse {
    access_token: String,
}

pub fn poll_codex() -> Result<UsageData, PollError> {
    match read_first_codex_tokens() {
        Some(tokens) => match fetch_codex_usage(&tokens) {
            Ok(data) => return Ok(data),
            Err(error) => diagnose::log(format!("Codex direct usage fetch failed: {error:?}")),
        },
        None => diagnose::log("codex direct usage fetch skipped: no Codex credentials found"),
    };

    match fetch_wsl_codex_usage() {
        Ok(data) => Ok(data),
        Err(error) => {
            diagnose::log(format!("Codex WSL usage fetch unavailable: {error:?}"));
            Err(error)
        }
    }
}

/// Spawn a command and wait up to `timeout` for it to finish.
/// Returns None if the process fails to start or exceeds the deadline.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    let mut child = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

fn build_agent() -> Result<ureq::Agent, PollError> {
    let tls = native_tls::TlsConnector::new().map_err(|_| PollError::RequestFailed)?;
    Ok(ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .tls_connector(std::sync::Arc::new(tls))
        .build())
}

fn fetch_codex_usage(tokens: &CodexTokens) -> Result<UsageData, PollError> {
    let access_token = tokens
        .access_token
        .as_deref()
        .ok_or(PollError::AuthRequired)?;

    match try_codex_usage_endpoint(access_token, tokens.account_id.as_deref()) {
        Ok(data) => Ok(data),
        Err(PollError::AuthRequired) => {
            let refreshed = refresh_codex_access_token(tokens)?;
            try_codex_usage_endpoint(&refreshed, tokens.account_id.as_deref())
        }
        Err(error) => Err(error),
    }
}

fn try_codex_usage_endpoint(
    access_token: &str,
    account_id: Option<&str>,
) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    let mut request = agent
        .get(CODEX_USAGE_URL)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("Accept", "application/json")
        .set(
            "User-Agent",
            "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
        );

    if let Some(account_id) = account_id.filter(|value| !value.is_empty()) {
        request = request.set("chatgpt-account-id", account_id);
    }

    let resp = match request.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "codex usage endpoint returned auth error status {code}; refresh required"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log(format!("codex usage endpoint request failed: {error}"));
            return Err(PollError::RequestFailed);
        }
    };

    let response: CodexUsageResponse = resp.into_json().map_err(|_| PollError::RequestFailed)?;
    codex_usage_response_to_data(response)
}

fn codex_usage_response_to_data(response: CodexUsageResponse) -> Result<UsageData, PollError> {
    let mut data = UsageData::default();
    let rate_limit = response.rate_limit.ok_or(PollError::RequestFailed)?;

    if let Some(bucket) = rate_limit.primary_window {
        data.session.percentage = bucket.used_percent;
        data.session.resets_at = unix_to_system_time(bucket.reset_at);
    }

    if let Some(bucket) = rate_limit.secondary_window {
        data.weekly.percentage = bucket.used_percent;
        data.weekly.resets_at = unix_to_system_time(bucket.reset_at);
    }

    Ok(data)
}

fn fetch_wsl_codex_usage() -> Result<UsageData, PollError> {
    let distros = list_wsl_distros();
    diagnose::log(format!("Codex WSL usage probe distros={distros:?}"));

    if distros.is_empty() {
        return Err(PollError::NoCredentials);
    }

    for distro in distros {
        match fetch_wsl_codex_usage_for_distro(&distro) {
            Ok(data) => {
                diagnose::log(format!(
                    "read Codex usage via WSL Python in distro {distro}"
                ));
                return Ok(data);
            }
            Err(error) => diagnose::log(format!(
                "WSL Codex usage fetch failed for distro {distro}: {error}"
            )),
        }
    }

    Err(PollError::RequestFailed)
}

fn fetch_wsl_codex_usage_for_distro(distro: &str) -> Result<UsageData, String> {
    let script = r#"
import json
import pathlib
import urllib.request

auth = json.loads((pathlib.Path.home() / ".codex" / "auth.json").read_text())
tokens = auth.get("tokens") or {}
access_token = tokens.get("access_token")
if not access_token:
    raise SystemExit(2)

headers = {
    "Authorization": "Bearer " + access_token,
    "User-Agent": "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
    "Accept": "application/json",
}
account_id = tokens.get("account_id")
if account_id:
    headers["chatgpt-account-id"] = account_id

req = urllib.request.Request("https://chatgpt.com/backend-api/wham/usage", headers=headers)
with urllib.request.urlopen(req, timeout=15) as resp:
    print(resp.read().decode("utf-8"))
"#;

    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("python3")
            .arg("-c")
            .arg(script)
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(20),
    )
    .ok_or_else(|| "wsl.exe/python3 timed out or failed to start".to_string())?;

    if !output.status.success() {
        return Err(format!("status {}", output.status));
    }

    let response: CodexUsageResponse = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid JSON from WSL Codex usage fetch: {error}"))?;
    codex_usage_response_to_data(response).map_err(|error| format!("{error:?}"))
}

fn refresh_codex_access_token(tokens: &CodexTokens) -> Result<String, PollError> {
    let refresh_token = tokens
        .refresh_token
        .as_deref()
        .ok_or(PollError::TokenExpired)?;
    let agent = build_agent()?;
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        url_encode(refresh_token),
        url_encode(CODEX_CLIENT_ID)
    );

    let resp = match agent
        .post(CODEX_TOKEN_URL)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "codex token refresh returned auth error status {code}; re-login required"
            ));
            return Err(PollError::TokenExpired);
        }
        Err(error) => {
            diagnose::log(format!("codex token refresh request failed: {error}"));
            return Err(PollError::RequestFailed);
        }
    };

    let response: CodexRefreshResponse = resp.into_json().map_err(|_| PollError::RequestFailed)?;
    Ok(response.access_token)
}

fn unix_to_system_time(unix_secs: Option<i64>) -> Option<SystemTime> {
    let secs = unix_secs?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

fn url_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn read_first_codex_tokens() -> Option<CodexTokens> {
    if let Some(tokens) = read_windows_codex_tokens() {
        return Some(tokens);
    }

    if let Some(tokens) = read_wsl_unc_codex_tokens() {
        return Some(tokens);
    }

    for distro in list_wsl_distros() {
        if let Some(tokens) = read_wsl_codex_tokens(&distro) {
            return Some(tokens);
        }
    }

    None
}

fn read_windows_codex_tokens() -> Option<CodexTokens> {
    let profile = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let path = PathBuf::from(profile).join(".codex").join("auth.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            if diagnose::is_enabled() {
                diagnose::log_error(
                    &format!(
                        "unable to read Windows Codex credentials at {}",
                        path.display()
                    ),
                    error,
                );
            }
            return None;
        }
    };
    parse_codex_tokens(&content)
}

fn read_wsl_unc_codex_tokens() -> Option<CodexTokens> {
    for distro in list_wsl_distros() {
        let users_root = PathBuf::from(format!(r"\\wsl$\{distro}\home"));
        let users = match std::fs::read_dir(&users_root) {
            Ok(users) => users,
            Err(error) => {
                diagnose::log_error(
                    &format!("unable to read WSL users via {}", users_root.display()),
                    error,
                );
                continue;
            }
        };

        for user in users.flatten() {
            let Ok(file_type) = user.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }

            let path = user.path().join(".codex").join("auth.json");
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };

            if let Some(tokens) = parse_codex_tokens(&content) {
                diagnose::log(format!(
                    "read Codex credentials via WSL UNC path {}",
                    path.display()
                ));
                return Some(tokens);
            }
        }
    }

    None
}

fn read_wsl_codex_tokens(distro: &str) -> Option<CodexTokens> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg("cat ~/.codex/auth.json")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    if !output.status.success() {
        diagnose::log(format!(
            "WSL Codex credentials probe failed for distro {distro} with status {}",
            output.status
        ));
        return None;
    }

    let content = String::from_utf8(output.stdout).ok()?;
    parse_codex_tokens(&content)
}

fn parse_codex_tokens(content: &str) -> Option<CodexTokens> {
    let auth: CodexAuth = serde_json::from_str(content).ok()?;
    let tokens = auth.tokens?;
    tokens.access_token.as_ref()?;
    Some(tokens)
}

fn list_wsl_distros() -> Vec<String> {
    let output = match run_with_timeout(
        Command::new("wsl.exe")
            .args(["-l", "-q"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    ) {
        Some(output) if output.status.success() => output,
        _ => {
            diagnose::log("unable to enumerate WSL distros");
            return Vec::new();
        }
    };

    let stdout = decode_wsl_text(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn decode_wsl_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    if let Some(decoded) = decode_utf16le(bytes) {
        return decoded;
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return None;
    }

    let body = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else if looks_like_utf16le(bytes) {
        bytes
    } else {
        return None;
    };

    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    Some(String::from_utf16_lossy(&units))
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    let sample_len = bytes.len().min(128);
    let units = sample_len / 2;
    if units == 0 {
        return false;
    }

    let nul_high_bytes = bytes[..sample_len]
        .chunks_exact(2)
        .filter(|chunk| chunk[1] == 0)
        .count();

    nul_high_bytes * 2 >= units
}

pub fn remaining_percent(used_percent: f64) -> f64 {
    (100.0 - used_percent.clamp(0.0, 100.0)).clamp(0.0, 100.0)
}

/// Format a usage window as "X% · Yh" style text, where X is remaining quota.
pub fn format_line(section: &UsageSection, strings: Strings) -> String {
    let pct = format!("{:.0}%", remaining_percent(section.percentage));
    let cd = format_countdown(section.resets_at, strings);
    if cd.is_empty() {
        pct
    } else {
        format!("{pct} \u{00b7} {cd}")
    }
}

fn format_countdown(resets_at: Option<SystemTime>, strings: Strings) -> String {
    let reset = match resets_at {
        Some(t) => t,
        None => return String::new(),
    };

    let remaining = match reset.duration_since(SystemTime::now()) {
        Ok(d) => d,
        Err(_) => return strings.now.to_string(),
    };

    format_countdown_from_secs(remaining.as_secs(), strings)
}

/// Calculate how long until the display text would change
pub fn time_until_display_change(resets_at: Option<SystemTime>) -> Option<Duration> {
    let reset = resets_at?;
    let remaining = reset.duration_since(SystemTime::now()).ok()?;
    Some(time_until_display_change_from_secs(remaining.as_secs()))
}

fn format_countdown_from_secs(total_secs: u64, strings: Strings) -> String {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    if total_days >= 1 {
        format!("{total_days}{}", strings.day_suffix)
    } else if total_hours >= 1 {
        format!("{total_hours}{}", strings.hour_suffix)
    } else if total_mins >= 1 {
        format!("{total_mins}{}", strings.minute_suffix)
    } else {
        format!("{total_secs}{}", strings.second_suffix)
    }
}

fn time_until_display_change_from_secs(total_secs: u64) -> Duration {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    let current_bucket_start = if total_days >= 1 {
        total_days * 86400
    } else if total_hours >= 1 {
        total_hours * 3600
    } else if total_mins >= 1 {
        total_mins * 60
    } else {
        total_secs
    };

    Duration::from_secs(total_secs.saturating_sub(current_bucket_start) + 1)
}

/// Returns true if either section has reached "now" (reset time has passed).
pub fn is_past_reset(data: &UsageData) -> bool {
    let now = SystemTime::now();
    let past = |s: &UsageSection| matches!(s.resets_at, Some(t) if now.duration_since(t).is_ok());
    past(&data.session) || past(&data.weekly)
}
