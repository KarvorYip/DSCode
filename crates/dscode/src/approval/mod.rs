//! Approval system (approval.zh.md): six-step decision chain as pure functions, critical/bash pattern tables,
//! multi-layer merging (deny union), three-mode policy, three remember-decision tiers, paired audit event construction.
//! The decision chain sits in front of every ApprovalProvider — table-driven and independently testable; this module does not depend on config.

pub mod provider;

use crate::tool::Tier;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Three approval modes (§2.1): ask human / auto reviewer / yolo skips prompts (deny still hard-enforced).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Ask,
    Auto,
    Yolo,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ask" => Some(Mode::Ask),
            "auto" => Some(Mode::Auto),
            "yolo" => Some(Mode::Yolo),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Ask => "ask",
            Mode::Auto => "auto",
            Mode::Yolo => "yolo",
        }
    }
}

/// Per-tool approval override (§3: `tools.approval.<tool>`), effective in all modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolApproval {
    Allow,
    Deny,
    Prompt,
}

/// bash pattern rule table (§2.5): allow matches only the whole compound command; deny/prompt are screened segment by segment.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct BashPatterns {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub prompt: Vec<String>,
}

/// Approval rules fed into the decision chain: global/project layers stored separately; the deny union is evaluated inside the chain (§2.7).
#[derive(Clone, Debug, Default)]
pub struct ApprovalRules {
    pub tools_global: BTreeMap<String, ToolApproval>,
    pub tools_project: BTreeMap<String, ToolApproval>,
    pub bash_global: BashPatterns,
    pub bash_project: BashPatterns,
}

/// Multi-layer merge of per-tool entries (§2.7 pure function): deny in any layer means deny; allow/prompt are overridden by the project layer.
pub fn merge_tool_entry(
    global: Option<ToolApproval>,
    project: Option<ToolApproval>,
) -> Option<ToolApproval> {
    if global == Some(ToolApproval::Deny) || project == Some(ToolApproval::Deny) {
        return Some(ToolApproval::Deny);
    }
    project.or(global)
}

impl ApprovalRules {
    /// Merged per-tool entry lookup: the deny union applies.
    pub fn tool_entry(&self, tool: &str) -> Option<ToolApproval> {
        merge_tool_entry(
            self.tools_global.get(tool).copied(),
            self.tools_project.get(tool).copied(),
        )
    }
}

/// Critical pattern table (§2.4/§2.5): destructive commands, always a human;
/// it sits above every configuration layer — no layer, override, or mode (including yolo) can allow them away.
pub const CRITICAL_PATTERNS: &[&str] = &[
    "rm -rf /",       // recursive root deletion
    "rm -fr /",       // root (argument-order variant)
    "rm -rf /*",      // first-level wildcard under root
    "rm -fr /*",      // same as above (variant)
    "rm -rf ~",       // home directory
    "rm -rf ~/*",     // home directory wildcard
    "rm -rf $HOME",   // home directory (env form)
    "rm -rf $HOME/*", // home directory wildcard (env form)
    ":()*|:*&*",      // fork bomb: :(){ :|:& };: plus compact/prefixed variants
    "mkfs*",          // format a filesystem
    "mkfs.* *",       // mkfs.ext4 /dev/sdX
    "dd *of=/dev/*",  // raw write to a block device
    "dd of=/dev/*",   // same as above (no if)
    "chmod -R * /",   // permission flip on root
    "> /dev/sd*",     // redirect write to a block device
    "*> /dev/sd*",    // same as above (with prefix)
    "*>/dev/sd*",     // same as above (no space)
    "shutdown*",      // shut down the machine
    "reboot",         // reboot
    "halt",           // halt
    "poweroff",       // power off
];

/// Glob matching; returns false on an invalid pattern (validity is fail-loud checked at config load time).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    globset::Glob::new(pattern)
        .map(|g| g.compile_matcher().is_match(text))
        .unwrap_or(false)
}

/// Compound-command segmentation (§2.5): split on `&&` / `||` / `;` / `|` / newlines; each segment is trimmed.
/// Pipes and newlines likewise hand the command to another process, so they must be screened exactly like logical chaining.
pub fn split_compound(command: &str) -> Vec<String> {
    command
        .replace("&&", "\u{0}")
        .replace("||", "\u{0}")
        .replace(';', "\u{0}")
        .replace('|', "\u{0}")
        .replace('\n', "\u{0}")
        .replace('\r', "\u{0}")
        .split('\u{0}')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Strips repeated leading `sudo ` prefixes so the critical/pattern tables need not enumerate sudo variants one by one.
fn strip_sudo(segment: &str) -> &str {
    let mut s = segment.trim();
    while let Some(rest) = s.strip_prefix("sudo ") {
        s = rest.trim_start();
    }
    s
}

/// Any segment hits the pattern table (segment-by-segment screening; shared by deny/prompt/critical).
fn segments_hit(patterns: &[String], segments: &[String]) -> bool {
    segments
        .iter()
        .any(|seg| patterns.iter().any(|p| glob_match(p, strip_sudo(seg))))
}

/// The whole command hits the pattern table (allow semantics: matches only the entire compound command).
fn whole_hit(patterns: &[String], command: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, command.trim()))
}

/// The whole command or any segment hits the critical table: always escalate to human; no configuration can allow it.
/// The whole-command check covers fork-bomb-style patterns that themselves contain a pipe (splitting on `|` would cut them apart);
/// the segment check covers dangerous commands on the right side of a pipe/newline, e.g. `echo x | rm -rf /`.
pub fn critical_hit(command: &str, segments: &[String]) -> bool {
    CRITICAL_PATTERNS
        .iter()
        .any(|p| glob_match(p, command.trim()))
        || segments.iter().any(|seg| {
            CRITICAL_PATTERNS
                .iter()
                .any(|p| glob_match(p, strip_sudo(seg)))
        })
}

/// Decision-chain step that escalated/blocked: shared by the card's "why escalated" display (§2.11) and audit events (§2.12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainStep {
    ToolPolicyDeny,   // Step 1: tools.approval deny
    UserDeny,         // Step 2: remembered-decision/session-level user deny
    YoloBypass,       // Step 3: yolo skips prompts
    ToolPrompt,       // Step 4: per-tool prompt override
    ToolAllow,        // Step 4: per-tool allow override
    PatternDeny,      // Step 5: bash pattern deny
    CriticalPattern,  // Step 5: critical pattern (always human)
    PatternPrompt,    // Step 5: bash pattern prompt
    PatternAllow,     // Step 5: whole-command allow hit
    ModeDefault,      // Step 6: mode default (ask escalates / auto reviewer)
    UserSessionAllow, // remembered decision, session tier approved
    TierRead,         // read tier has no side effects, allowed outright
}

impl ChainStep {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChainStep::ToolPolicyDeny => "tool-policy-deny",
            ChainStep::UserDeny => "user-deny",
            ChainStep::YoloBypass => "yolo",
            ChainStep::ToolPrompt => "tool-prompt",
            ChainStep::ToolAllow => "tool-allow",
            ChainStep::PatternDeny => "pattern-deny",
            ChainStep::CriticalPattern => "critical-pattern",
            ChainStep::PatternPrompt => "pattern-prompt",
            ChainStep::PatternAllow => "pattern-allow",
            ChainStep::ModeDefault => "mode-default",
            ChainStep::UserSessionAllow => "user-session-allow",
            ChainStep::TierRead => "tier-read",
        }
    }
}

/// Decision-chain final value (§2.2).
pub type ChainOutcome = ChainAction;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainAction {
    /// Allow; no prompt needed.
    Allow(ChainStep),
    /// Final deny; the reason step flows back to the model with the result.
    Deny(ChainStep),
    /// Escalate to the human decision card (mode default / prompt / pattern prompt / critical / reviewer escalation).
    EscalateHuman(ChainStep),
    /// auto mode default: hand to AutoReviewer for review (critical and pattern prompt never enter this path).
    AutoReview(ChainStep),
}

/// Session-level remembered decisions (§2.6 session tier): an in-memory map, never persisted.
/// The once tier applies to the current turn only (the caller applies it directly); the always tier takes effect through rules once written to config.
#[derive(Default)]
pub struct UserDecisions {
    session: BTreeMap<String, bool>,
}

impl UserDecisions {
    /// Remember a session-level decision: non-bash by tool name, bash by first word (e.g. "git").
    pub fn remember(&mut self, tool: &str, command: Option<&str>, allowed: bool) {
        self.session.insert(decision_key(tool, command), allowed);
    }

    pub fn lookup(&self, tool: &str, command: Option<&str>) -> Option<bool> {
        self.session.get(&decision_key(tool, command)).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.session.is_empty()
    }

    /// Remembered decision keys (tool name or bash:<first-word>) — the approved-rules digest for reviewer requests and decision cards.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.session.keys().map(|s| s.as_str())
    }

    /// Session-tier snapshot for sub-agents (approval.zh.md §2.10: user deny stays effective in children).
    pub fn snapshot(&self) -> BTreeMap<String, bool> {
        self.session.clone()
    }

    /// Rebuild from a snapshot (the sub-agent gate consumes this).
    pub fn from_map(map: BTreeMap<String, bool>) -> Self {
        Self { session: map }
    }
}

fn decision_key(tool: &str, command: Option<&str>) -> String {
    if tool == "bash" {
        let first = command
            .and_then(|c| c.split_whitespace().next())
            .unwrap_or("");
        format!("bash:{first}")
    } else {
        tool.to_string()
    }
}

/// Extract the bash command from tool args: {"command":"..."} or a bare string (the shape bash.rs accepts).
fn command_of(args: &Value) -> Option<&str> {
    args.get("command")
        .and_then(Value::as_str)
        .or_else(|| args.as_str())
}

/// Six-step decision chain (§2.2) — pure function, table-driven:
/// 1. tool policy deny (deny union, final)
/// 2. user deny (remembered/session-level, final; yolo cannot override a human)
/// 3. yolo special case (skips all prompts, but critical and pattern deny stay hard-enforced)
/// 4. per-tool override (prompt escalates; allow is not a final exit — bash still passes the pattern table)
/// 5. bash pattern table (critical > deny > prompt > whole-command allow)
/// 6. mode default (ask escalates / auto reviewer / read allows)
pub fn decide(
    tool_name: &str,
    tier: Tier,
    args: &Value,
    rules: &ApprovalRules,
    mode: Mode,
    user: &UserDecisions,
) -> ChainOutcome {
    let is_bash = tool_name == "bash";
    let command = if is_bash { command_of(args) } else { None };
    let segments = command.map(split_compound).unwrap_or_default();
    let deny_hit = segments_hit(&rules.bash_global.deny, &segments)
        || segments_hit(&rules.bash_project.deny, &segments);

    // Step 1: tool policy deny — deny in any layer is final; yolo cannot overturn it.
    if rules.tool_entry(tool_name) == Some(ToolApproval::Deny) {
        return ChainAction::Deny(ChainStep::ToolPolicyDeny);
    }
    // Step 2: user deny — a remembered-decision/session-level deny is final.
    if user.lookup(tool_name, command) == Some(false) {
        return ChainAction::Deny(ChainStep::UserDeny);
    }
    // Step 3: yolo special case — skips all prompt logic, but critical and pattern deny stay hard-enforced.
    if mode == Mode::Yolo {
        if is_bash {
            if critical_hit(command.unwrap_or(""), &segments) {
                return ChainAction::EscalateHuman(ChainStep::CriticalPattern);
            }
            if deny_hit {
                return ChainAction::Deny(ChainStep::PatternDeny);
            }
        }
        return ChainAction::Allow(ChainStep::YoloBypass);
    }
    // Step 2b: user allow (session-tier approval, suppresses prompts) — bash's allow is deferred until
    // after the pattern deny/critical evaluation; deny cannot be overturned by user allow.
    if !is_bash && user.lookup(tool_name, command) == Some(true) {
        return ChainAction::Allow(ChainStep::UserSessionAllow);
    }
    // Step 4: per-tool override. Deny was handled in step 1; prompt forces a human (auto skips the reviewer too);
    // allow is not an exit for bash — it still has to pass the pattern table (worked example 1).
    match rules.tool_entry(tool_name) {
        Some(ToolApproval::Prompt) => return ChainAction::EscalateHuman(ChainStep::ToolPrompt),
        Some(ToolApproval::Allow) if !is_bash => return ChainAction::Allow(ChainStep::ToolAllow),
        _ => {}
    }
    // Step 5: bash pattern table.
    if let Some(cmd) = command {
        if critical_hit(command.unwrap_or(""), &segments) {
            return ChainAction::EscalateHuman(ChainStep::CriticalPattern);
        }
        if deny_hit {
            return ChainAction::Deny(ChainStep::PatternDeny);
        }
        if user.lookup(tool_name, Some(cmd)) == Some(true) {
            return ChainAction::Allow(ChainStep::UserSessionAllow);
        }
        if segments_hit(&rules.bash_project.prompt, &segments)
            || (segments_hit(&rules.bash_global.prompt, &segments)
                && !whole_hit(&rules.bash_project.allow, cmd))
        {
            return ChainAction::EscalateHuman(ChainStep::PatternPrompt);
        }
        if whole_hit(&rules.bash_project.allow, cmd) || whole_hit(&rules.bash_global.allow, cmd) {
            return ChainAction::Allow(ChainStep::PatternAllow);
        }
    }
    // Step 6: mode default.
    match (mode, tier) {
        (_, Tier::Read) => ChainAction::Allow(ChainStep::TierRead),
        (Mode::Ask, _) => ChainAction::EscalateHuman(ChainStep::ModeDefault),
        (Mode::Auto, _) => ChainAction::AutoReview(ChainStep::ModeDefault),
        (Mode::Yolo, _) => ChainAction::Allow(ChainStep::YoloBypass),
    }
}

/// Effective mode (§2.1): auto requires modelRoles.approver to be configured; when it is not, the effective mode falls to yolo
/// and a one-time onboarding flag is set (the onboarding prompt is the integration layer's job).
pub struct EffectiveMode {
    pub mode: Mode,
    pub onboarding_needed: bool,
}

pub fn effective_mode(mode: Mode, approver_configured: bool) -> EffectiveMode {
    if mode == Mode::Auto && !approver_configured {
        EffectiveMode {
            mode: Mode::Yolo,
            onboarding_needed: true,
        }
    } else {
        EffectiveMode {
            mode,
            onboarding_needed: false,
        }
    }
}

/// Remembered-decision tiers (§2.6): once for the current turn only / session in memory / always written to config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remember {
    Once,
    Session,
    Always,
}

impl Remember {
    pub fn as_str(&self) -> &'static str {
        match self {
            Remember::Once => "once",
            Remember::Session => "session",
            Remember::Always => "always",
        }
    }
}

/// Always-tier rule proposal (§2.6): non-bash by tool name; bash proposes a first-word pattern (e.g. "git *").
/// The proposal is shown on the card and written to config only after the user confirms — never applied automatically.
pub fn propose_always_rule(tool_name: &str, args: &Value) -> Option<String> {
    if tool_name == "bash" {
        let first = command_of(args)?.split_whitespace().next()?;
        Some(format!("{first} *"))
    } else {
        Some(tool_name.to_string())
    }
}

/// Decider (§2.12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decider {
    Human,
    Reviewer,
    HeadlessReject,
    Rule,
}

impl Decider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Decider::Human => "human",
            Decider::Reviewer => "reviewer",
            Decider::HeadlessReject => "HeadlessReject",
            Decider::Rule => "rule",
        }
    }
}

/// Decision result (§2.12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
    RejectHeadless,
}

impl Decision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::Approve => "approve",
            Decision::Deny => "deny",
            Decision::RejectHeadless => "reject-headless",
        }
    }
}

/// `approval/asked` event material (§2.12).
pub struct Asked {
    pub tool: String,
    pub tier: Tier,
    pub args: Value,
    pub step: ChainStep,
    pub mode: Mode,
    /// The answering side at escalation time: Human for the human card, Reviewer for auto-review.
    pub decider: Decider,
}

/// `approval/decided` event material (§2.12).
pub struct Decided {
    pub decision: Decision,
    pub decider: Decider,
    /// The reviewer's reason, if any.
    pub reason: Option<String>,
    /// The chosen remember tier; the always tier carries the rule text (`always:<rule>`).
    pub remember: Option<Remember>,
    pub always_rule: Option<String>,
}

/// Args digest: truncate the payload; audit events store only the digest, never the full text.
fn summarize_args(args: &Value) -> String {
    let s = args.to_string();
    if s.chars().count() > 500 {
        let cut: String = s.chars().take(500).collect();
        format!("{cut}…(截断)")
    } else {
        s
    }
}

/// Paired audit event construction (§2.9/§2.12): `approval/asked` + `approval/decided`,
/// log-only — the model sees only tool results, never the audit trail.
pub fn audit_pair(asked: &Asked, decided: &Decided) -> Vec<(String, Value)> {
    vec![
        (
            "approval/asked".to_string(),
            serde_json::json!({
                "tool": asked.tool,
                "tier": format!("{:?}", asked.tier),
                "args": summarize_args(&asked.args),
                "step": asked.step.as_str(),
                "mode": asked.mode.as_str(),
                "decider": asked.decider.as_str(),
            }),
        ),
        (
            "approval/decided".to_string(),
            serde_json::json!({
                "tool": asked.tool,
                "decision": decided.decision.as_str(),
                "decider": decided.decider.as_str(),
                "reason": decided.reason,
                "remember": decided.remember.map(|r| match (&decided.always_rule, r) {
                    (Some(rule), Remember::Always) => format!("always:{rule}"),
                    _ => r.as_str().to_string(),
                }),
            }),
        ),
    ]
}

/// `approval/policy` mode-switch event construction (§2.8/§2.12):
/// from/to modes, the trigger, and whether the approver is configured (effective flag).
pub fn policy_event(
    from: Mode,
    to: Mode,
    trigger: &str,
    approver_effective: bool,
) -> (String, Value) {
    (
        "approval/policy".to_string(),
        serde_json::json!({
            "from": from.as_str(),
            "to": to.as_str(),
            "trigger": trigger,
            "approverEffective": approver_effective,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(cmd: &str) -> Value {
        json!({ "command": cmd })
    }

    fn pats(allow: &[&str], deny: &[&str], prompt: &[&str]) -> BashPatterns {
        BashPatterns {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            prompt: prompt.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn no_user() -> UserDecisions {
        UserDecisions::default()
    }

    /// Table-driven: every precedence combination of the six-step decision chain (§4 acceptance).
    #[test]
    fn 决议链_表驱动_全步骤优先级() {
        struct Case {
            name: &'static str,
            tool: &'static str,
            tier: Tier,
            args: Value,
            rules: ApprovalRules,
            mode: Mode,
            user: UserDecisions,
            expect: ChainAction,
        }
        let mk = |rules: ApprovalRules, mode: Mode, expect: ChainAction| Case {
            name: "",
            tool: "bash",
            tier: Tier::Exec,
            args: bash("true"),
            rules,
            mode,
            user: no_user(),
            expect,
        };
        // Local constructor: avoids handwriting every field in each case.
        #[rustfmt::skip]
        let cases: Vec<(&str, Box<dyn Fn() -> Case>)> = vec![
            // Step 1: tools.approval deny still blocks under yolo.
            ("步骤1 每工具deny在yolo下仍拦截", Box::new(|| {
                let mut r = ApprovalRules::default();
                r.tools_project.insert("bash".into(), ToolApproval::Deny);
                mk(r, Mode::Yolo, ChainAction::Deny(ChainStep::ToolPolicyDeny))
            })),
            // Step 1: deny union — a global deny cannot be overturned by a project allow.
            ("步骤1 deny并集_全局deny项目allow仍拒", Box::new(|| {
                let mut r = ApprovalRules::default();
                r.tools_global.insert("write".into(), ToolApproval::Deny);
                r.tools_project.insert("write".into(), ToolApproval::Allow);
                let mut c = mk(r, Mode::Ask, ChainAction::Deny(ChainStep::ToolPolicyDeny));
                c.tool = "write"; c.tier = Tier::Write; c.args = json!({"path": "a"});
                c
            })),
            // Step 1: project deny + global allow also denies (any layer of the union).
            ("步骤1 deny并集_项目deny全局allow仍拒", Box::new(|| {
                let mut r = ApprovalRules::default();
                r.tools_global.insert("write".into(), ToolApproval::Allow);
                r.tools_project.insert("write".into(), ToolApproval::Deny);
                let mut c = mk(r, Mode::Ask, ChainAction::Deny(ChainStep::ToolPolicyDeny));
                c.tool = "write"; c.tier = Tier::Write; c.args = json!({"path": "a"});
                c
            })),
            // Step 2: yolo + a session-level remembered user deny denies (worked example 2).
            ("步骤2 yolo下会话级user_deny仍拒绝", Box::new(|| {
                let mut u = UserDecisions::default();
                u.remember("write", None, false);
                let mut c = mk(ApprovalRules::default(), Mode::Yolo, ChainAction::Deny(ChainStep::UserDeny));
                c.tool = "write"; c.tier = Tier::Write; c.args = json!({"path": "src/lib.rs"}); c.user = u;
                c
            })),
            // Step 3: yolo skips prompts; ordinary calls are allowed.
            ("步骤3 yolo放行普通调用", Box::new(|| {
                let mut c = mk(ApprovalRules::default(), Mode::Yolo, ChainAction::Allow(ChainStep::YoloBypass));
                c.tool = "write"; c.tier = Tier::Write; c.args = json!({"path": "a"});
                c
            })),
            // Step 3: bash pattern deny stays hard-enforced under yolo.
            ("步骤3 yolo下pattern_deny硬生效", Box::new(|| {
                let mut r = ApprovalRules::default();
                r.bash_global = pats(&[], &["rm -rf *"], &[]);
                let mut c = mk(r, Mode::Yolo, ChainAction::Deny(ChainStep::PatternDeny));
                c.args = bash("rm -rf build");
                c
            })),
            // Step 4: per-tool prompt escalates to a human; auto skips the reviewer too.
            ("步骤4 每工具prompt在auto下仍升级人工", Box::new(|| {
                let mut r = ApprovalRules::default();
                r.tools_project.insert("write".into(), ToolApproval::Prompt);
                let mut c = mk(r, Mode::Auto, ChainAction::EscalateHuman(ChainStep::ToolPrompt));
                c.tool = "write"; c.tier = Tier::Write; c.args = json!({"path": "a"});
                c
            })),
            // Step 4: per-tool prompt can escalate a read-tier call.
            ("步骤4 每工具prompt可升级read", Box::new(|| {
                let mut r = ApprovalRules::default();
                r.tools_project.insert("read".into(), ToolApproval::Prompt);
                let mut c = mk(r, Mode::Ask, ChainAction::EscalateHuman(ChainStep::ToolPrompt));
                c.tool = "read"; c.tier = Tier::Read; c.args = json!({"path": "a"});
                c
            })),
            // Step 4: per-tool allow (non-bash) allows under ask.
            ("步骤4 每工具allow非bash放行", Box::new(|| {
                let mut r = ApprovalRules::default();
                r.tools_project.insert("write".into(), ToolApproval::Allow);
                let mut c = mk(r, Mode::Ask, ChainAction::Allow(ChainStep::ToolAllow));
                c.tool = "write"; c.tier = Tier::Write; c.args = json!({"path": "a"});
                c
            })),
            // Step 5: compound-command segmented deny (worked example 1) — the project's per-tool allow
            // is not an allow exit; policy deny is evaluated before any relaxation.
            ("步骤5 复合命令分段deny_演算示例一", Box::new(|| {
                let mut r = ApprovalRules::default();
                r.bash_global = pats(&[], &["rm -rf *"], &[]);
                r.tools_project.insert("bash".into(), ToolApproval::Allow);
                let mut c = mk(r, Mode::Auto, ChainAction::Deny(ChainStep::PatternDeny));
                c.args = bash("cd /tmp && rm -rf build");
                c
            })),
            // Step 5: allow matches only the whole compound command — allowed only on a whole-command hit.
            ("步骤5 整条复合命令allow命中放行", Box::new(|| {
                let mut r = ApprovalRules::default();
                r.bash_project = pats(&["cd /tmp && rm -rf build"], &[], &[]);
                let mut c = mk(r, Mode::Ask, ChainAction::Allow(ChainStep::PatternAllow));
                c.args = bash("cd /tmp && rm -rf build");
                c
            })),
            // Step 5: segments do not constitute an allow hit — falls through to mode default.
            ("步骤5 分段allow不命中落到mode默认", Box::new(|| {
                let mut r = ApprovalRules::default();
                r.bash_project = pats(&["rm -rf build"], &[], &[]);
                let mut c = mk(r, Mode::Ask, ChainAction::EscalateHuman(ChainStep::ModeDefault));
                c.args = bash("cd /tmp && rm -rf build");
                c
            })),
            // Step 5: prompt pattern forces a human; auto skips the reviewer too.
            ("步骤5 pattern_prompt在auto下仍升级人工", Box::new(|| {
                let mut r = ApprovalRules::default();
                r.bash_global = pats(&[], &[], &["git push*"]);
                let mut c = mk(r, Mode::Auto, ChainAction::EscalateHuman(ChainStep::PatternPrompt));
                c.args = bash("git push origin main");
                c
            })),
            // Step 5: project allow overrides global prompt.
            ("步骤5 项目allow覆盖全局prompt", Box::new(|| {
                let mut r = ApprovalRules::default();
                r.bash_global = pats(&[], &[], &["git *"]);
                r.bash_project = pats(&["git status"], &[], &[]);
                let mut c = mk(r, Mode::Ask, ChainAction::Allow(ChainStep::PatternAllow));
                c.args = bash("git status");
                c
            })),
            // Remembered decision, session tier: non-bash allowed by tool name.
            ("记住决定_session档非bash放行", Box::new(|| {
                let mut u = UserDecisions::default();
                u.remember("write", None, true);
                let mut c = mk(ApprovalRules::default(), Mode::Ask, ChainAction::Allow(ChainStep::UserSessionAllow));
                c.tool = "write"; c.tier = Tier::Write; c.args = json!({"path": "a"}); c.user = u;
                c
            })),
            // Remembered decision, session tier: bash allowed by first word, but pattern deny cannot be overturned.
            ("记住决定_session档bash放行但deny优先", Box::new(|| {
                let mut u = UserDecisions::default();
                u.remember("bash", Some("git push origin"), true);
                let mut r = ApprovalRules::default();
                r.bash_global = pats(&[], &["git push*"], &[]);
                let mut c = mk(r, Mode::Ask, ChainAction::Deny(ChainStep::PatternDeny));
                c.args = bash("git push origin main"); c.user = u;
                c
            })),
            // Step 6: ask defaults to escalating to a human.
            ("步骤6 ask默认升级人工", Box::new(|| {
                let mut c = mk(ApprovalRules::default(), Mode::Ask, ChainAction::EscalateHuman(ChainStep::ModeDefault));
                c.tool = "write"; c.tier = Tier::Write; c.args = json!({"path": "a"});
                c
            })),
            // Step 6: auto defaults to the reviewer.
            ("步骤6 auto默认代审", Box::new(|| {
                let mut c = mk(ApprovalRules::default(), Mode::Auto, ChainAction::AutoReview(ChainStep::ModeDefault));
                c.tool = "write"; c.tier = Tier::Write; c.args = json!({"path": "a"});
                c
            })),
            // Step 6: read tier has no side effects, allowed outright.
            ("步骤6 read_tier直接放行", Box::new(|| {
                let mut c = mk(ApprovalRules::default(), Mode::Ask, ChainAction::Allow(ChainStep::TierRead));
                c.tool = "read"; c.tier = Tier::Read; c.args = json!({"path": "a"});
                c
            })),
        ];
        for (name, build) in cases {
            let c = build();
            let got = decide(c.tool, c.tier, &c.args, &c.rules, c.mode, &c.user);
            assert_eq!(got, c.expect, "用例「{name}」失败");
        }
    }

    /// Critical patterns can never be allowed in any mode (including per-tool allow and pattern allow).
    #[test]
    fn critical_pattern_任何模式不可allow() {
        for mode in [Mode::Ask, Mode::Auto, Mode::Yolo] {
            let mut rules = ApprovalRules::default();
            rules
                .tools_project
                .insert("bash".into(), ToolApproval::Allow);
            rules.bash_project = pats(&["rm -rf *"], &[], &[]);
            let got = decide(
                "bash",
                Tier::Exec,
                &bash("rm -rf /"),
                &rules,
                mode,
                &no_user(),
            );
            assert_eq!(
                got,
                ChainAction::EscalateHuman(ChainStep::CriticalPattern),
                "mode={} 下 critical 必须升级人工",
                mode.as_str()
            );
        }
    }

    /// Critical-pattern calls never reach AutoReviewer (§4 acceptance).
    #[test]
    fn critical_pattern_绝不进代审() {
        let got = decide(
            "bash",
            Tier::Exec,
            &bash("rm -rf ~"),
            &ApprovalRules::default(),
            Mode::Auto,
            &no_user(),
        );
        assert_eq!(got, ChainAction::EscalateHuman(ChainStep::CriticalPattern));
    }

    /// Critical-table per-line hit samples: rm -rf on root/home, fork bomb, mkfs, dd, block-device redirect, shutdown.
    #[test]
    fn critical_pattern_表逐行命中样本() {
        let samples = [
            "rm -rf /",
            "rm -fr /etc",
            "rm -rf ~",
            "rm -rf $HOME",
            ":(){ :|:& };:",
            ":(){:|:&};:",
            "mkfs.ext4 /dev/sda1",
            "dd if=img of=/dev/sda",
            "echo x > /dev/sda",
            "sudo shutdown -h now",
            "reboot",
        ];
        for cmd in samples {
            let segs = split_compound(cmd);
            assert!(critical_hit(cmd, &segs), "critical 表未命中：{cmd}");
        }
        // Ordinary commands must not hit.
        for cmd in [
            "rm -rf build",
            "cd /tmp && rm -rf build",
            "git status",
            "dd if=a of=b",
        ] {
            let segs = split_compound(cmd);
            assert!(!critical_hit(cmd, &segs), "critical 表误命中：{cmd}");
        }
    }

    /// Compound-command segmentation: split on && / || / ; per segment; empty segments are dropped.
    #[test]
    fn 复合命令分段() {
        assert_eq!(
            split_compound("cd /tmp && rm -rf build || echo fail; ls"),
            vec!["cd /tmp", "rm -rf build", "echo fail", "ls"]
        );
    }

    /// Per-tool entry multi-layer merge pure function: deny union, allow/prompt project override (§2.7 hierarchy grid).
    #[test]
    fn merge_tool_entry_deny并集_项目覆盖() {
        use ToolApproval::*;
        assert_eq!(merge_tool_entry(Some(Deny), Some(Allow)), Some(Deny));
        assert_eq!(merge_tool_entry(Some(Allow), Some(Deny)), Some(Deny));
        assert_eq!(merge_tool_entry(Some(Deny), None), Some(Deny));
        assert_eq!(merge_tool_entry(Some(Prompt), Some(Allow)), Some(Allow));
        assert_eq!(merge_tool_entry(None, Some(Prompt)), Some(Prompt));
        assert_eq!(merge_tool_entry(Some(Prompt), None), Some(Prompt));
        assert_eq!(merge_tool_entry(Some(Allow), None), Some(Allow));
        assert_eq!(merge_tool_entry(None, None), None);
    }

    /// Remembered-decision keys: non-bash by tool name, bash by first word.
    #[test]
    fn 记住决定_键位() {
        let mut u = UserDecisions::default();
        u.remember("bash", Some("git push origin"), true);
        assert_eq!(u.lookup("bash", Some("git status")), Some(true));
        assert_eq!(u.lookup("bash", Some("npm test")), None);
        u.remember("write", None, false);
        assert_eq!(u.lookup("write", None), Some(false));
        assert_eq!(u.lookup("write", Some("任意")), Some(false));
    }

    /// Always-tier rule proposal: non-bash by tool name; bash proposes a first-word pattern.
    #[test]
    fn always档规则提议() {
        assert_eq!(
            propose_always_rule("write", &json!({"path": "a"})),
            Some("write".to_string())
        );
        assert_eq!(
            propose_always_rule("bash", &bash("git push origin main")),
            Some("git *".to_string())
        );
    }

    /// Without an approver configured, auto effectively falls to yolo and sets the one-time onboarding flag.
    #[test]
    fn effective_mode_未配approver落yolo() {
        let e = effective_mode(Mode::Auto, false);
        assert_eq!(e.mode, Mode::Yolo);
        assert!(e.onboarding_needed);
        let e = effective_mode(Mode::Auto, true);
        assert_eq!(e.mode, Mode::Auto);
        assert!(!e.onboarding_needed);
        let e = effective_mode(Mode::Ask, false);
        assert_eq!(e.mode, Mode::Ask);
    }

    /// Paired audit event payloads: decider / reason / remember tier all present (§2.12).
    #[test]
    fn 审计事件_配对载荷齐全() {
        let asked = Asked {
            tool: "bash".into(),
            tier: Tier::Exec,
            args: bash("rm -rf build"),
            step: ChainStep::PatternPrompt,
            mode: Mode::Auto,
            decider: Decider::Human,
        };
        let decided = Decided {
            decision: Decision::Approve,
            decider: Decider::Human,
            reason: Some("代审升级，人工复核".into()),
            remember: Some(Remember::Always),
            always_rule: Some("rm -rf *".into()),
        };
        let events = audit_pair(&asked, &decided);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "approval/asked");
        let asked_data = events[0].1.as_object().unwrap();
        assert_eq!(asked_data["tool"], "bash");
        assert_eq!(asked_data["tier"], "Exec");
        assert_eq!(asked_data["step"], "pattern-prompt");
        assert_eq!(asked_data["mode"], "auto");
        assert_eq!(asked_data["decider"], "human");
        assert_eq!(events[1].0, "approval/decided");
        let decided_data = events[1].1.as_object().unwrap();
        assert_eq!(decided_data["decision"], "approve");
        assert_eq!(decided_data["decider"], "human");
        assert_eq!(decided_data["reason"], "代审升级，人工复核");
        assert_eq!(decided_data["remember"], "always:rm -rf *");
    }

    /// Mode-switch event payload: from/to/trigger/approver effective flag.
    #[test]
    fn policy事件_载荷齐全() {
        let (kind, data) = policy_event(Mode::Ask, Mode::Auto, "/approval-mode", true);
        assert_eq!(kind, "approval/policy");
        assert_eq!(data["from"], "ask");
        assert_eq!(data["to"], "auto");
        assert_eq!(data["trigger"], "/approval-mode");
        assert_eq!(data["approverEffective"], true);
    }

    /// Every glob in the critical table itself compiles (an invalid pattern in the internal constant would silently fail to block).
    #[test]
    fn critical_pattern_表全部可编译() {
        for p in CRITICAL_PATTERNS {
            assert!(globset::Glob::new(p).is_ok(), "critical pattern 非法：{p}");
        }
    }
}
