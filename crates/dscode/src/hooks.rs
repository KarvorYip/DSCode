//! Declarative hooks (config-onboarding.zh.md §declarative hooks): built-in event bus +
//! YAML triggers → actions (block / rewrite input / notify) from the `.dscode` config.
//! The event enum copies the Claude Code hooks spec (§16 ruling 2); phase-1 notify only
//! prints to stderr. WASM/JS hook runtimes are deferred long-term.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// hooks event enum (copied from the Claude Code hooks spec).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Notification,
    Stop,
    SubagentStop,
    PreCompact,
    SessionStart,
    SessionEnd,
}

impl HookEvent {
    /// The event name is the YAML config key (PascalCase, matching the Claude Code spec).
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "PreToolUse" => HookEvent::PreToolUse,
            "PostToolUse" => HookEvent::PostToolUse,
            "UserPromptSubmit" => HookEvent::UserPromptSubmit,
            "Notification" => HookEvent::Notification,
            "Stop" => HookEvent::Stop,
            "SubagentStop" => HookEvent::SubagentStop,
            "PreCompact" => HookEvent::PreCompact,
            "SessionStart" => HookEvent::SessionStart,
            "SessionEnd" => HookEvent::SessionEnd,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::Notification => "Notification",
            HookEvent::Stop => "Stop",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
        }
    }
}

/// One trigger rule: matcher matches tool names (glob; default matches all); exactly one action is taken.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HookRule {
    /// Tool name or glob (e.g. "bash*"); only meaningful for tool events; defaults to matching all.
    #[serde(default)]
    pub matcher: Option<String>,
    /// Action one: block — veto the operation (intercepts the tool call on PreToolUse).
    #[serde(default)]
    pub block: Option<String>,
    /// Action two: rewrite input (tool arguments / user prompt, depending on the event).
    #[serde(default)]
    pub rewrite: Option<String>,
    /// Action three: notify — phase 1 only prints to stderr.
    #[serde(default)]
    pub notify: Option<String>,
}

impl HookRule {
    /// Action count is exactly one: 0 or >1 is a config error, fail loud at load time.
    fn action_count(&self) -> usize {
        [
            self.block.is_some(),
            self.rewrite.is_some(),
            self.notify.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count()
    }

    fn matches(&self, tool_name: Option<&str>) -> bool {
        match (&self.matcher, tool_name) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(m), Some(t)) => t == m || crate::approval::glob_match(m, t),
        }
    }
}

/// hooks config (`hooks.*` keys): event name → rule list.
/// Keys are kept as strings; event-name validity is validated in `Hooks::load` and fails loud.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HooksConfig {
    #[serde(default, flatten)]
    pub rules: BTreeMap<String, Vec<HookRule>>,
}

/// Two-layer merge (leaf level): the project layer wholly overrides the global layer per event key.
pub fn merge_hooks(global: &HooksConfig, project: &HooksConfig) -> HooksConfig {
    let mut rules = global.rules.clone();
    for (k, v) in &project.rules {
        rules.insert(k.clone(), v.clone());
    }
    HooksConfig { rules }
}

/// Dispatch result: allow by default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookOutcome {
    /// Allow (no rule matched, or notify only).
    Proceed,
    /// Veto the operation (block action).
    Blocked { reason: String },
    /// input was rewritten (rewrite action, chainable).
    Rewritten { input: String },
}

/// Event bus: holds the validated rule table; dispatch is a pure table lookup.
#[derive(Clone, Debug)]
pub struct Hooks {
    rules: BTreeMap<HookEvent, Vec<HookRule>>,
}

impl Hooks {
    /// Load from config: invalid event names or rule shapes fail loud (reported at startup, §two error tiers).
    pub fn load(config: &crate::config::Config) -> Result<Self, String> {
        let mut rules = BTreeMap::new();
        for (name, list) in &config.hooks.rules {
            let event = HookEvent::parse(name)
                .ok_or_else(|| format!("未知 hook 事件「{name}」（合法值见 HookEvent 枚举）"))?;
            for (i, rule) in list.iter().enumerate() {
                let n = rule.action_count();
                if n != 1 {
                    return Err(format!(
                        "hook 规则非法：{name}[{i}] 必须恰有一个动作（block/rewrite/notify），实际 {n} 个"
                    ));
                }
                if let Some(m) = &rule.matcher {
                    if globset::Glob::new(m).is_err() {
                        return Err(format!("hook matcher 非法 glob：{name}[{i}]「{m}」"));
                    }
                }
            }
            rules.insert(event, list.clone());
        }
        Ok(Hooks { rules })
    }

    /// Dispatch an event: rules are evaluated in order; the first block vetoes, rewrites chain,
    /// notify prints to stderr. No rule matched → allow by default.
    pub fn dispatch(&self, event: HookEvent, tool_name: Option<&str>, input: &str) -> HookOutcome {
        let Some(list) = self.rules.get(&event) else {
            return HookOutcome::Proceed;
        };
        let mut current = input.to_string();
        for rule in list {
            if !rule.matches(tool_name) {
                continue;
            }
            if let Some(reason) = &rule.block {
                return HookOutcome::Blocked {
                    reason: reason.clone(),
                };
            }
            if let Some(next) = &rule.rewrite {
                current = next.clone();
            }
            if let Some(msg) = &rule.notify {
                eprintln!("[hook:{event}] {msg}", event = event.as_str());
            }
        }
        if current != input {
            HookOutcome::Rewritten { input: current }
        } else {
            HookOutcome::Proceed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// Build Hooks from minimal config YAML text (bypasses two-layer file loading).
    fn hooks_from(yaml: &str) -> Result<Hooks, String> {
        let config: Config = crate::config::Config::from_str_layers(yaml, "")?;
        Hooks::load(&config)
    }

    #[test]
    fn pretooluse_block否决匹配工具() {
        let h = hooks_from("hooks:\n  PreToolUse:\n    - matcher: bash\n      block: 禁止 bash\n")
            .unwrap();
        assert_eq!(
            h.dispatch(HookEvent::PreToolUse, Some("bash"), "{}"),
            HookOutcome::Blocked {
                reason: "禁止 bash".to_string()
            }
        );
        // Non-matching tools are allowed.
        assert_eq!(
            h.dispatch(HookEvent::PreToolUse, Some("read"), "{}"),
            HookOutcome::Proceed
        );
    }

    #[test]
    fn matcher支持glob() {
        let h =
            hooks_from("hooks:\n  PreToolUse:\n    - matcher: task*\n      block: 任务族禁用\n")
                .unwrap();
        assert_eq!(
            h.dispatch(HookEvent::PreToolUse, Some("taskCreate"), "{}"),
            HookOutcome::Blocked {
                reason: "任务族禁用".to_string()
            }
        );
    }

    #[test]
    fn rewrite改写input并可链式() {
        let h = hooks_from(
            "hooks:\n  UserPromptSubmit:\n    - rewrite: '第一段改写'\n    - rewrite: '最终输入'\n",
        )
        .unwrap();
        assert_eq!(
            h.dispatch(HookEvent::UserPromptSubmit, None, "原始输入"),
            HookOutcome::Rewritten {
                input: "最终输入".to_string()
            }
        );
    }

    #[test]
    fn notify不改变结果() {
        let h = hooks_from("hooks:\n  Stop:\n    - notify: 回合结束\n").unwrap();
        assert_eq!(h.dispatch(HookEvent::Stop, None, ""), HookOutcome::Proceed);
    }

    #[test]
    fn 无规则默认放行() {
        let h = Hooks::load(&Config::default()).unwrap();
        assert_eq!(
            h.dispatch(HookEvent::PreToolUse, Some("bash"), "{}"),
            HookOutcome::Proceed
        );
        let empty = hooks_from("").unwrap();
        assert_eq!(
            empty.dispatch(HookEvent::SessionEnd, None, ""),
            HookOutcome::Proceed
        );
    }

    #[test]
    fn block先于后续rewrite() {
        let h = hooks_from(
            "hooks:\n  PreToolUse:\n    - rewrite: 'x'\n    - matcher: bash\n      block: 拦截\n",
        )
        .unwrap();
        assert_eq!(
            h.dispatch(HookEvent::PreToolUse, Some("bash"), "y"),
            HookOutcome::Blocked {
                reason: "拦截".to_string()
            }
        );
    }

    #[test]
    fn 加载校验_未知事件名报错() {
        let err = hooks_from("hooks:\n  OnToolRun:\n    - block: x\n").unwrap_err();
        assert!(err.contains("未知 hook 事件"), "实际错误：{err}");
    }

    #[test]
    fn 加载校验_零动作报错() {
        let err = hooks_from("hooks:\n  Stop:\n    - matcher: bash\n").unwrap_err();
        assert!(err.contains("恰有一个动作"), "实际错误：{err}");
    }

    #[test]
    fn 加载校验_多动作报错() {
        let err = hooks_from("hooks:\n  Stop:\n    - block: x\n      notify: y\n").unwrap_err();
        assert!(err.contains("恰有一个动作"), "实际错误：{err}");
    }

    #[test]
    fn 九事件枚举parse往返() {
        for ev in [
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::UserPromptSubmit,
            HookEvent::Notification,
            HookEvent::Stop,
            HookEvent::SubagentStop,
            HookEvent::PreCompact,
            HookEvent::SessionStart,
            HookEvent::SessionEnd,
        ] {
            assert_eq!(HookEvent::parse(ev.as_str()), Some(ev));
        }
        assert_eq!(HookEvent::parse("OnToolRun"), None);
    }

    #[test]
    fn hooks双层合并_项目覆盖全局() {
        let global = HooksConfig {
            rules: BTreeMap::from([(
                "Stop".to_string(),
                vec![HookRule {
                    matcher: None,
                    block: None,
                    rewrite: None,
                    notify: Some("全局".into()),
                }],
            )]),
        };
        let project = HooksConfig {
            rules: BTreeMap::from([(
                "Stop".to_string(),
                vec![HookRule {
                    matcher: None,
                    block: Some("项目".into()),
                    rewrite: None,
                    notify: None,
                }],
            )]),
        };
        let merged = merge_hooks(&global, &project);
        assert_eq!(merged.rules["Stop"][0].block.as_deref(), Some("项目"));
        // Global-only events are preserved.
        let mut global2 = global.clone();
        global2.rules.insert(
            "SessionEnd".to_string(),
            vec![HookRule {
                matcher: None,
                block: None,
                rewrite: None,
                notify: Some("结束".into()),
            }],
        );
        let merged2 = merge_hooks(&global2, &HooksConfig::default());
        assert!(merged2.rules.contains_key("SessionEnd"));
    }
}
