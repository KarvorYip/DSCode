//! Limit recovery (limits.zh.md): provider-agnostic error classification, the backoff
//! schedule, and the suspension runtime types consumed by the chat loop.
//!
//! Classification parses the error body — never provider identity or the HTTP status
//! alone: a `reset` field (relative seconds or an absolute timestamp) marks quota-class;
//! a 429 without one is rate-class; everything else is unclassifiable and never suspends.

use std::time::Duration;

/// Three-way classification (limits.zh.md §错误分类).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// Quota/window exhaustion; `reset_at` is the epoch-ms window boundary when the
    /// error body carries a parseable reset time.
    Quota { reset_at: Option<u64> },
    /// 429 transient rate limiting without quota semantics (DeepSeek's shape).
    Rate,
    /// Neither — ordinary error handling; never suspends, never auto-retries.
    Unknown,
}

/// Quota-exhaustion wording for non-429 statuses (402/403 billing shapes); a bare 429
/// never consults this list — no-reset 429s are rate-class per spec.
const QUOTA_MARKERS: &[&str] = &["quota", "credit", "billing", "balance", "配额", "余额"];

/// Classify one provider error from its HTTP status + error body (limits.zh.md
/// §错误分类): a parseable reset time is the tell of quota-class regardless of status;
/// a 429 without one is rate-class; quota wording on other statuses infers quota-class
/// without a reset (ladder probing); everything else stays unclassified.
pub fn classify(status: u16, body: &str) -> ErrorClass {
    if let Some(reset_at) = parse_reset(body) {
        return ErrorClass::Quota {
            reset_at: Some(reset_at),
        };
    }
    if status == 429 {
        return ErrorClass::Rate;
    }
    let lower = body.to_lowercase();
    if QUOTA_MARKERS.iter().any(|m| lower.contains(m)) {
        return ErrorClass::Quota { reset_at: None };
    }
    ErrorClass::Unknown
}

/// Classify a full provider error string. DeepSeek wire format is
/// `DeepSeek API <status>：<body>`; errors without an HTTP status (transport/parse
/// failures) are unclassifiable — never suspend.
pub fn classify_error(err: &str) -> ErrorClass {
    if let Some(pos) = err.find("API ") {
        let rest = &err[pos + 4..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() == 3 {
            if let Ok(status) = digits.parse::<u16>() {
                return classify(status, &rest[digits.len()..]);
            }
        }
    }
    ErrorClass::Unknown
}

/// Extract a reset time (epoch ms) from an error body (limits.zh.md §错误分类):
/// JSON fields whose name contains `reset` (or retry-after variants) carrying either a
/// relative seconds count or an absolute timestamp (epoch s/ms or RFC 3339). Falls back
/// to a sweep over non-JSON bodies.
pub fn parse_reset(body: &str) -> Option<u64> {
    let now = now_ms();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        return find_reset_value(&v, now);
    }
    sweep_reset(body, now)
}

/// Sweep a non-JSON body for a `"…reset…": <value>` pair.
fn sweep_reset(body: &str, now: u64) -> Option<u64> {
    let mut rest = body;
    while let Some(i) = rest.find("reset") {
        let after = &rest[i..];
        // Skip until the value after the key's colon.
        let Some(colon) = after.find(':') else { break };
        let tail = after[colon + 1..].trim_start();
        let value: String = match tail.strip_prefix('"') {
            Some(t) => t.chars().take_while(|&c| c != '"').collect(),
            None => tail
                .chars()
                .take_while(|&c| c.is_ascii_digit() || c == '.')
                .collect(),
        };
        if !value.is_empty() {
            if let Some(t) = parse_reset_value(&value, now) {
                return Some(t);
            }
        }
        rest = &rest[i + 5..];
    }
    None
}

fn is_reset_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("reset") || k == "retry_after" || k == "retry-after" || k == "retryafter"
}

fn find_reset_value(v: &serde_json::Value, now: u64) -> Option<u64> {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map {
                if is_reset_key(k) {
                    let raw = match val {
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::String(s) => s.clone(),
                        _ => continue,
                    };
                    if let Some(t) = parse_reset_value(&raw, now) {
                        return Some(t);
                    }
                } else if let Some(t) = find_reset_value(val, now) {
                    return Some(t);
                }
            }
            None
        }
        serde_json::Value::Array(a) => a.iter().find_map(|x| find_reset_value(x, now)),
        _ => None,
    }
}

/// Parse one reset value against `now` (epoch ms): a bare number is epoch ms (>= 1e12),
/// epoch seconds (>= 1e9) or relative seconds; RFC 3339 strings are absolute; forms like
/// `"3600s"` fall back to relative seconds.
fn parse_reset_value(raw: &str, now: u64) -> Option<u64> {
    let raw = raw.trim();
    if let Ok(f) = raw.parse::<f64>() {
        return Some(if f >= 1e12 {
            f as u64
        } else if f >= 1e9 {
            (f * 1000.0) as u64
        } else {
            now + (f * 1000.0) as u64
        });
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.timestamp_millis().max(0) as u64);
    }
    let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().ok().map(|n| now + n * 1000)
}

/// Backoff knobs (limits.zh.md §退避与防抖). Defaults are the spec's production values;
/// tests shrink them to milliseconds.
#[derive(Clone, Debug)]
pub struct BackoffCfg {
    /// Rate-class exponential base (1s).
    pub rate_base_ms: u64,
    /// Rate-class cap (60s).
    pub rate_cap_ms: u64,
    /// Consecutive rate-class failures that escalate to quota-class handling (5).
    pub rate_escalate_after: u32,
    /// Quota-class no-reset ladder: 1min → 5min → 15min → 30min cap.
    pub ladder_ms: [u64; 4],
    /// Probe margin past a known reset time (30s).
    pub reset_margin_ms: u64,
    /// Suspension tick (countdown refresh + key poll).
    pub tick_ms: u64,
}

impl Default for BackoffCfg {
    fn default() -> Self {
        Self {
            rate_base_ms: 1_000,
            rate_cap_ms: 60_000,
            rate_escalate_after: 5,
            ladder_ms: [60_000, 300_000, 900_000, 1_800_000],
            reset_margin_ms: 30_000,
            tick_ms: 1_000,
        }
    }
}

/// Rate-class backoff for the nth consecutive failure (1-based): 1s → 2s → … capped.
pub fn rate_backoff(attempt: u32, cfg: &BackoffCfg) -> Duration {
    let shift = (attempt.saturating_sub(1)).min(63);
    let ms = cfg
        .rate_base_ms
        .saturating_mul(1u64 << shift)
        .min(cfg.rate_cap_ms);
    Duration::from_millis(ms)
}

/// Ladder step for the current index (clamped to the last rung).
pub fn ladder_wait(idx: usize, cfg: &BackoffCfg) -> Duration {
    Duration::from_millis(cfg.ladder_ms[idx.min(cfg.ladder_ms.len() - 1)])
}

/// Debounce advance (limits.zh.md §防抖): a repeated error from the same quota window
/// climbs the ladder (never resets to the short end — anti hot-loop); a new window
/// restarts it.
pub fn advance_ladder(idx: usize, same_window: bool) -> usize {
    if same_window {
        (idx + 1).min(3)
    } else {
        0
    }
}

/// Debounce key (limits.zh.md §防抖): the same reset time is the same quota window;
/// without one the error text identity stands in.
pub fn window_key(err: &str, reset_at: Option<u64>) -> String {
    match reset_at {
        Some(t) => format!("reset:{t}"),
        None => format!("body:{err}"),
    }
}

/// Session-level limit-recovery state threaded through ChatCtx.
#[derive(Clone, Debug)]
pub struct LimitsCtl {
    /// autoContinue.enabled (limits.zh.md §配置): auto probe at reset+margin / ladder
    /// expiry and auto continue after recovery; false = manual 「立即重试」 only.
    pub auto_continue: bool,
    /// Backoff schedule (production defaults; tests shrink).
    pub backoff: BackoffCfg,
    /// One-time-per-session auto-resume highlight guard (limits.zh.md §恢复触发).
    pub highlight_shown: bool,
}

impl Default for LimitsCtl {
    fn default() -> Self {
        Self {
            auto_continue: true,
            backoff: BackoffCfg::default(),
            highlight_shown: false,
        }
    }
}

/// One suspension snapshot handed to the UI each tick.
#[derive(Clone, Debug)]
pub struct SuspendInfo {
    /// Reason for the panel: limit kind summary (limits.zh.md §TUI: kind + provider).
    pub reason: String,
    /// Known reset time (epoch ms); None = ladder probing without a known window.
    pub reset_at: Option<u64>,
    /// Next probe time (epoch ms): reset+30s when known, else the current ladder step.
    pub next_probe_at: u64,
}

/// The UI's wish after one suspension tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuspendAction {
    /// Keep suspending (auto probe still fires from the chat loop when enabled).
    Wait,
    /// Manual 「立即重试」: probe now; never re-arms a disarmed goal.
    RetryNow,
    /// Reload provider/model configuration and immediately retry the unfinished request.
    ReloadProvider,
    /// 「取消挂起」: abandon this suspension permanently (no more probes; records kept).
    Cancel,
}

/// Human-readable suspension reason: quotes the error body's message field when
/// present, else the error head.
pub fn suspend_reason(err: &str) -> String {
    let msg =
        extract_json_string_field(err, "message").unwrap_or_else(|| err.chars().take(80).collect());
    format!("用量限额：{msg}")
}

fn extract_json_string_field(text: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let i = text.find(&needle)? + needle.len();
    let rest = text[i..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

pub fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// mm:ss-style countdown from now to an epoch-ms deadline (hh:mm:ss past an hour).
pub fn fmt_countdown(until_ms: u64, now_ms: u64) -> String {
    let left = until_ms.saturating_sub(now_ms) / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        left / 3600,
        (left % 3600) / 60,
        left % 60
    )
}

/// Compact duration label ("2s", "1min", "5min30s").
pub fn fmt_secs(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        _ if s % 60 == 0 => format!("{}min", s / 60),
        _ => format!("{}min{}s", s / 60, s % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 分类器三分类表驱动() {
        let cases: Vec<(u16, &str, ErrorClass)> = vec![
            // 429 carrying a reset time → quota-class regardless of wording.
            (
                429,
                r#"{"error":{"type":"rate_limit_error","reset_time":"2030-01-01T00:00:00Z"}}"#,
                ErrorClass::Quota {
                    reset_at: Some(1_893_456_000_000),
                },
            ),
            // 429 without a reset → rate-class (DeepSeek's shape).
            (
                429,
                r#"{"error":{"message":"Too Many Requests","type":"rate_limit"}}"#,
                ErrorClass::Rate,
            ),
            (
                429,
                r#"{"detail":"rate limited, retry later"}"#,
                ErrorClass::Rate,
            ),
            // Non-429 quota wording without a reset → quota-class, ladder probing.
            (
                402,
                r#"{"error":{"message":"Insufficient Balance"}}"#,
                ErrorClass::Quota { reset_at: None },
            ),
            (
                403,
                r#"{"error":{"message":"quota exceeded for this tier"}}"#,
                ErrorClass::Quota { reset_at: None },
            ),
            // Everything else → unclassifiable, never suspends.
            (
                500,
                r#"{"error":{"message":"Internal error"}}"#,
                ErrorClass::Unknown,
            ),
            (503, "overloaded_error", ErrorClass::Unknown),
            (200, "{}", ErrorClass::Unknown),
        ];
        for (status, body, want) in cases {
            assert_eq!(classify(status, body), want, "status {status} body {body}");
        }
    }

    #[test]
    fn 全错误字符串分类() {
        assert_eq!(
            classify_error(r#"DeepSeek API 429：{"error":{"message":"Too Many Requests"}}"#),
            ErrorClass::Rate
        );
        assert_eq!(
            classify_error(r#"DeepSeek API 402：{"error":{"message":"Insufficient Balance"}}"#),
            ErrorClass::Quota { reset_at: None }
        );
        // Transport / parse failures carry no HTTP status → never suspend.
        assert_eq!(
            classify_error("请求失败：connection reset by peer"),
            ErrorClass::Unknown
        );
        assert_eq!(classify_error("解析响应失败：eof"), ErrorClass::Unknown);
        assert_eq!(
            classify_error("DeepSeek API 500：bad gateway"),
            ErrorClass::Unknown
        );
    }

    #[test]
    fn reset解析_相对秒与绝对时间戳() {
        let before = now_ms();
        let rel = parse_reset(r#"{"reset": 30}"#).unwrap();
        let after = now_ms();
        assert!(
            (before + 30_000..=after + 30_000).contains(&rel),
            "相对秒应落在 now+30s 邻域，实际 {rel}"
        );
        let ra = parse_reset(r#"{"retry_after": 45}"#).unwrap();
        assert!(
            (before + 45_000..=after + 45_000).contains(&ra),
            "retry_after 按相对秒解析"
        );
        // Absolute forms are exact regardless of now.
        assert_eq!(
            parse_reset(r#"{"reset_time": 1755331200}"#),
            Some(1_755_331_200_000)
        );
        assert_eq!(
            parse_reset(r#"{"reset_at": 1755331200000}"#),
            Some(1_755_331_200_000)
        );
        assert_eq!(
            parse_reset(r#"{"reset": 1755331200.5}"#),
            Some(1_755_331_200_500)
        );
        assert_eq!(
            parse_reset(r#"{"error":{"reset_time":"2030-01-01T00:00:00Z"}}"#),
            Some(1_893_456_000_000)
        );
        // Trailing-unit relative seconds are covered by the sweep below.
        assert_eq!(
            parse_reset(r#"{"error":{"message":"no reset here"}}"#),
            None
        );
        assert_eq!(parse_reset("plain text body"), None);
        // Non-JSON body: the sweep still finds a reset pair.
        assert_eq!(
            parse_reset(r#"limited until {"reset": 1755331200} please retry"#),
            Some(1_755_331_200_000)
        );
        // Trailing-unit relative seconds.
        let before_units = now_ms();
        let s = parse_reset(r#"{"reset": "2s"}"#).unwrap();
        let after_units = now_ms();
        assert!((before_units + 2_000..=after_units + 2_000).contains(&s));
    }

    #[test]
    fn 速率退避指数阶梯与封顶() {
        let cfg = BackoffCfg::default();
        let want = [1, 2, 4, 8, 16, 32, 60, 60];
        for (i, secs) in want.iter().enumerate() {
            assert_eq!(
                rate_backoff(i as u32 + 1, &cfg),
                Duration::from_secs(*secs),
                "attempt {}",
                i + 1
            );
        }
    }

    #[test]
    fn 无reset阶梯与封顶() {
        let cfg = BackoffCfg::default();
        assert_eq!(ladder_wait(0, &cfg), Duration::from_secs(60));
        assert_eq!(ladder_wait(1, &cfg), Duration::from_secs(300));
        assert_eq!(ladder_wait(2, &cfg), Duration::from_secs(900));
        assert_eq!(ladder_wait(3, &cfg), Duration::from_secs(1800));
        assert_eq!(
            ladder_wait(99, &cfg),
            Duration::from_secs(1800),
            "越界钳到末档"
        );
    }

    #[test]
    fn 防抖_同窗口不重置阶梯() {
        assert_eq!(advance_ladder(0, true), 1, "同窗口推进");
        assert_eq!(advance_ladder(2, true), 3);
        assert_eq!(advance_ladder(3, true), 3, "封顶 30min 档");
        assert_eq!(advance_ladder(5, false), 0, "新窗口重置");
        assert_eq!(advance_ladder(0, false), 0);
    }

    #[test]
    fn 窗口键与原因文案() {
        assert_eq!(window_key("e", Some(123)), "reset:123");
        assert_eq!(window_key("body text", None), "body:body text");
        assert_eq!(
            suspend_reason(r#"DeepSeek API 429：{"error":{"message":"rate limited until noon"}}"#),
            "用量限额：rate limited until noon"
        );
        let head = suspend_reason("plain failure text");
        assert!(head.starts_with("用量限额："));
    }

    #[test]
    fn 倒计时与时长文案() {
        assert_eq!(fmt_countdown(1_000, 1_000), "00:00:00");
        assert_eq!(fmt_countdown(66_000, 1_000), "00:01:05");
        assert_eq!(fmt_countdown(3_661_000, 1_000), "01:01:00");
        assert_eq!(fmt_secs(Duration::from_secs(2)), "2s");
        assert_eq!(fmt_secs(Duration::from_secs(300)), "5min");
        assert_eq!(fmt_secs(Duration::from_secs(330)), "5min30s");
    }
}
