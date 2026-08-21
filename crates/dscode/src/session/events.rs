//! Session event envelope and the first-release event-key registry (session.zh.md).
//! The envelope copies dsh SessionEvent field-for-field: type/seq/time (epoch millis)/data/ignorable;
//! known dsh keys have ignorable=false; DSCode-owned keys must have ignorable=true (dsh reader skip guarantee).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Mirrors dsh SESSION_FORMAT_VERSION: 0, no compatibility promise, free to evolve pre-release.
pub const SESSION_FORMAT_VERSION: u64 = 0;

/// Event key origin: known dsh keys may be written with ignorable=false; DSCode-owned keys must be ignorable=true.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    Dsh,
    Dscode,
}

/// First-release event-key registry: table-driven, constant, complete set at startup.
/// Read contract: an unknown key without ignorable:true must error and name the key (see validate).
/// No pre-registered keys: unimplemented subsystems (sandbox/schedule/hook/tool-workflow/…) register nothing until implemented.
const REGISTRY: &[(&str, Origin)] = &[
    // —— Base: verbatim copy of the dsh SessionEventMap main declarations (todo/write renamed to task/write with ticket 005) ——
    ("turn/start", Origin::Dsh),
    ("turn/end", Origin::Dsh),
    ("step/start", Origin::Dsh),
    ("step/end", Origin::Dsh),
    ("user/message", Origin::Dsh),
    ("assistant/chunk", Origin::Dsh),
    ("assistant/message", Origin::Dsh),
    ("tool/call", Origin::Dsh),
    ("tool/result", Origin::Dsh),
    ("task/write", Origin::Dscode),
    ("request/header", Origin::Dsh),
    ("request/context", Origin::Dsh),
    ("session/end-seed", Origin::Dsh),
    // —— approval (3): paired audit events + mode switches (approval.zh.md) ——
    ("approval/asked", Origin::Dsh),
    ("approval/decided", Origin::Dsh),
    ("approval/policy", Origin::Dsh),
    // —— goal (2): goal/change (goal.zh.md) + auto-continue rearm audit (limits.zh.md §goal rearm) ——
    ("goal/change", Origin::Dsh),
    ("goal/rearm", Origin::Dscode),
    // —— compaction (3) ——
    ("compaction/start", Origin::Dsh),
    ("compaction/summary", Origin::Dsh),
    ("compaction/end", Origin::Dsh),
    // —— plan (1) ——
    ("plan/mode", Origin::Dsh),
    // —— title (1): written when the commit role generates a title ——
    ("session/title", Origin::Dsh),
    // —— commands (2): slash command execution ——
    ("command/run", Origin::Dsh),
    ("command/done", Origin::Dsh),
    // —— agent (3): sub-agent dispatch lifecycle + hub messaging (tools.zh.md §3.8/§3.9) ——
    ("agent/spawned", Origin::Dscode),
    ("agent/completed", Origin::Dscode),
    ("agent/message", Origin::Dscode),
    // —— Crash repair marker: appended after truncating a half line, records that a repair happened (session.zh.md resume section) ——
    ("session/repair", Origin::Dscode),
];

/// Look up the registry; None = unknown key.
pub fn lookup(kind: &str) -> Option<Origin> {
    REGISTRY.iter().find(|(k, _)| *k == kind).map(|(_, o)| *o)
}

/// Per-event read-time validation: an unknown key with ignorable!=true must be rejected, naming the key.
pub fn validate(ev: &Event) -> Result<(), String> {
    match lookup(&ev.kind) {
        Some(_) => Ok(()),
        None if ev.ignorable => Ok(()),
        None => Err(format!(
            "未知事件键且不可忽略：\"{}\"（seq {}）",
            ev.kind, ev.seq
        )),
    }
}

/// One log-line event (envelope). time is Unix epoch millis, copied from dsh.
/// Writers always emit ignorable; readers treat a missing ignorable as false (required event, per the dsh contract).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: String,
    pub seq: u64,
    pub time: u64,
    pub data: Value,
    #[serde(default)]
    pub ignorable: bool,
}

/// Surface event types: keys that produce model-visible messages and can enter the ordered surface (copied from dsh SurfaceEventType).
pub const SURFACE_TYPES: &[&str] = &["user/message", "assistant/message", "tool/result"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 注册表含首发键且无未实装子系统键() {
        for key in [
            "turn/start",
            "turn/end",
            "step/start",
            "step/end",
            "user/message",
            "assistant/chunk",
            "assistant/message",
            "tool/call",
            "tool/result",
            "task/write",
            "request/header",
            "request/context",
            "session/end-seed",
            "approval/asked",
            "approval/decided",
            "approval/policy",
            "goal/change",
            "compaction/start",
            "compaction/summary",
            "compaction/end",
            "plan/mode",
            "session/title",
            "command/run",
            "command/done",
            "session/repair",
            "agent/spawned",
            "agent/completed",
            "agent/message",
        ] {
            assert!(lookup(key).is_some(), "首发键缺失：{key}");
        }
        for (key, _) in REGISTRY {
            let unimplemented = ["sandbox/", "schedule/", "hook/", "tool-workflow/"];
            assert!(
                !unimplemented.iter().any(|p| key.starts_with(p)),
                "注册了未实装子系统的键：{key}"
            );
        }
    }

    #[test]
    fn dsh键与自有键的ignorable走向() {
        assert_eq!(lookup("turn/start"), Some(Origin::Dsh));
        assert_eq!(lookup("task/write"), Some(Origin::Dscode));
        assert_eq!(lookup("session/repair"), Some(Origin::Dscode));
    }

    #[test]
    fn 未知键校验按ignorable区分() {
        let ev = |kind: &str, ignorable: bool| Event {
            kind: kind.to_string(),
            seq: 0,
            time: 0,
            data: serde_json::json!({}),
            ignorable,
        };
        assert!(validate(&ev("turn/start", false)).is_ok());
        let err = validate(&ev("mystery/x", false)).unwrap_err();
        assert!(err.contains("mystery/x"), "报错须点名键：{err}");
        assert!(validate(&ev("mystery/x", true)).is_ok());
    }

    #[test]
    fn envelope序列化字段名照抄dsh() {
        let ev = Event {
            kind: "turn/start".into(),
            seq: 7,
            time: 1755234481000,
            data: serde_json::json!({ "turn": 1 }),
            ignorable: false,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""type":"turn/start""#), "字段名须为 type：{s}");
        assert!(s.contains(r#""ignorable":false"#), "ignorable 总写出：{s}");
        let back: Event = serde_json::from_str(&s).unwrap();
        assert_eq!(back.kind, ev.kind);
        assert_eq!(back.seq, ev.seq);
        assert_eq!(back.time, ev.time);
        assert_eq!(back.ignorable, ev.ignorable);
    }
}
