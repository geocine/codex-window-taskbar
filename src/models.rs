use std::time::SystemTime;

#[derive(Clone, Debug, Default)]
pub struct UsageSection {
    pub percentage: f64,
    pub resets_at: Option<SystemTime>,
    /// Window length from the API (e.g. 18000 for 5h, 604800 for 7d).
    pub limit_window_seconds: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct UsageData {
    /// Present rate-limit windows in API order (primary, then secondary).
    /// Codex currently returns 1 or 2 windows; the 5h window may be omitted.
    pub windows: Vec<UsageSection>,
}
