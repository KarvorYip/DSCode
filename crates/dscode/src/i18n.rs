//! UI string table (config-onboarding.zh.md §TUI 显示语言): zh/en dual-language lookup
//! for user-visible strings (TUI surfaces, headless stdout lines, main startup errors).
//! Pure-Rust exhaustive matches — no i18n crate; a key missing from either language fails
//! the build. Out of scope by contract (stay Chinese): system prompts, tool outputs and
//! tool errors (model input), session log event data, code comments.

use serde::{Deserialize, Serialize};

/// Display language (`tui.language`): zh factory default, en opt-in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    Zh,
    En,
}

impl Lang {
    /// Parse "zh" / "en"; anything else is an error (config invalid values fail loud
    /// at deserialization time; this entry point serves tests and slash commands).
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "zh" => Ok(Lang::Zh),
            "en" => Ok(Lang::En),
            other => Err(format!("unknown language \"{other}\" (expected zh/en)")),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }
}

/// One user-visible UI string. Templates use positional `{}` args filled by the call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrKey {
    // ---- TUI chrome ----
    /// Welcome hint line (first transcript entry).
    WelcomeHint,
    StatusIdle,
    StatusStreaming,
    /// Approval status label; arg: tool name.
    StatusApproving,
    /// Shift+Tab mode switch notice; args: from mode, to mode.
    StatusModeSwitched,
    /// UserPromptSubmit hook veto status; arg: hook reason.
    StatusInputVetoed,
    PanelChatTitle,
    PanelTasksTitle,
    PanelInputTitle,
    /// Status-bar mode label prefix; arg: mode.
    ModeLabel,
    // ---- conversation transcript lines ----
    /// Echoed user message; arg: text.
    UserLine,
    /// Assistant message prefix; arg: text.
    AssistantLine,
    /// Live tool call line; args: name, arguments.
    ToolCallLine,
    /// Live tool result line; arg: output.
    ToolResultLine,
    /// Resume-rebuild tool call; arg: name.
    TranscriptToolCall,
    /// Resume-rebuild tool result; arg: output head.
    TranscriptToolResult,
    /// Resume-rebuild approval decision; args: tool, decision.
    TranscriptApproval,
    /// goal/change create card; arg: objective.
    GoalCreatedCard,
    /// goal/change generic card; args: action, status, objective.
    GoalChangedCard,
    /// Compaction boundary separator.
    CompactionSep,
    // ---- /goal command replies ----
    GoalNotEnabled,
    /// /goal show snapshot; args: objective, status, revision, rounds, max, tokens, budget.
    GoalShowCard,
    /// Budget label when unlimited.
    GoalUnlimited,
    GoalNoneHint,
    GoalPausedOk,
    GoalResumedOk,
    GoalClearedOk,
    GoalEditedOk,
    GoalCreatedOk,
    /// Failure wrapper; arg: error message.
    GoalOpFailed,
    GoalOpFailedDefault,
    /// Punctuation join between an ok message and the objective.
    OkColon,
    // ---- goal status-bar badge suffixes ----
    BadgePausedSuffix,
    BadgeStoppedSuffix,
    BadgeCompleteSuffix,
    // ---- limit suspension (panel + status bar + headless) ----
    SuspendPanelTitle,
    /// Panel reason row; arg: reason.
    SuspendPanelReason,
    /// Panel reset row; args: model, countdown.
    SuspendPanelReset,
    /// Panel probe row; args: model, countdown.
    SuspendPanelProbe,
    SuspendPanelKeys,
    /// Status-bar line with a known reset time; args: model, countdown.
    StatusSuspendReset,
    /// Status-bar line under periodic probing; args: model, countdown.
    StatusSuspendProbe,
    /// Suspension reason after five 429s; arg: failure count.
    Escalated429Reason,
    /// Rate-class retry status; args: delay, failure count.
    Status429Retry,
    /// Turn error after the user cancels a suspension.
    SuspendCancelledErr,
    HeadlessSuspendModeReset,
    HeadlessSuspendModeProbe,
    /// Headless one-line suspension status; args: reason, mode label.
    HeadlessSuspendLine,
    // ---- auto continue ----
    AutoResumedNote,
    /// Goal re-arm appendix; arg: objective.
    AutoResumedGoalNote,
    // ---- approval card ----
    /// Card title; args: tool, tier.
    ApprovalCardTitle,
    ApprovalCardArgs,
    ApprovalCardStep,
    ApprovalCardReviewer,
    ApprovalCriticalHit,
    /// Always-tier proposal notice; arg: rule.
    ApprovalAlwaysProposal,
    ApprovalKeysCritical,
    ApprovalKeysFull,
    /// Key legend; arg: key list.
    ApprovalKeysHint,
    ApprovalTimeoutNotice,
    ApprovedOnce,
    ApprovedSession,
    ApprovedAlways,
    DeniedOnce,
    DeniedSession,
    // ---- headless ----
    /// Tool call line; args: name, arguments.
    HeadlessToolCall,
    HeadlessPromptHint,
    // ---- main.rs startup ----
    ConfigErrorPrefix,
    HooksErrorPrefix,
    YoloNotice,
    UsageResume,
    UsageFork,
    UnknownApprovalMode,
    TaskRestoreFailed,
    MissingApiKey,
    SubagentApiKey,
    ReviewerApiKey,
    NoSessions,
    SessionsHeader,
    UntitledSession,
    // ---- /language command ----
    /// No-arg reply; arg: current language.
    LanguageCurrent,
    LanguageSwitched,
    /// Write-back failure notice; args: language, error.
    LanguageWriteFailed,
    LanguageInvalid,
}

impl StrKey {
    /// Every key, for the non-empty test — keep in sync when adding variants (the two
    /// exhaustive matches in `tr` are the compile-time language-parity guarantee).
    pub const ALL: &'static [StrKey] = &[
        StrKey::WelcomeHint,
        StrKey::StatusIdle,
        StrKey::StatusStreaming,
        StrKey::StatusApproving,
        StrKey::StatusModeSwitched,
        StrKey::StatusInputVetoed,
        StrKey::PanelChatTitle,
        StrKey::PanelTasksTitle,
        StrKey::PanelInputTitle,
        StrKey::ModeLabel,
        StrKey::UserLine,
        StrKey::AssistantLine,
        StrKey::ToolCallLine,
        StrKey::ToolResultLine,
        StrKey::TranscriptToolCall,
        StrKey::TranscriptToolResult,
        StrKey::TranscriptApproval,
        StrKey::GoalCreatedCard,
        StrKey::GoalChangedCard,
        StrKey::CompactionSep,
        StrKey::GoalNotEnabled,
        StrKey::GoalShowCard,
        StrKey::GoalUnlimited,
        StrKey::GoalNoneHint,
        StrKey::GoalPausedOk,
        StrKey::GoalResumedOk,
        StrKey::GoalClearedOk,
        StrKey::GoalEditedOk,
        StrKey::GoalCreatedOk,
        StrKey::GoalOpFailed,
        StrKey::GoalOpFailedDefault,
        StrKey::OkColon,
        StrKey::BadgePausedSuffix,
        StrKey::BadgeStoppedSuffix,
        StrKey::BadgeCompleteSuffix,
        StrKey::SuspendPanelTitle,
        StrKey::SuspendPanelReason,
        StrKey::SuspendPanelReset,
        StrKey::SuspendPanelProbe,
        StrKey::SuspendPanelKeys,
        StrKey::StatusSuspendReset,
        StrKey::StatusSuspendProbe,
        StrKey::Escalated429Reason,
        StrKey::Status429Retry,
        StrKey::SuspendCancelledErr,
        StrKey::HeadlessSuspendModeReset,
        StrKey::HeadlessSuspendModeProbe,
        StrKey::HeadlessSuspendLine,
        StrKey::AutoResumedNote,
        StrKey::AutoResumedGoalNote,
        StrKey::ApprovalCardTitle,
        StrKey::ApprovalCardArgs,
        StrKey::ApprovalCardStep,
        StrKey::ApprovalCardReviewer,
        StrKey::ApprovalCriticalHit,
        StrKey::ApprovalAlwaysProposal,
        StrKey::ApprovalKeysCritical,
        StrKey::ApprovalKeysFull,
        StrKey::ApprovalKeysHint,
        StrKey::ApprovalTimeoutNotice,
        StrKey::ApprovedOnce,
        StrKey::ApprovedSession,
        StrKey::ApprovedAlways,
        StrKey::DeniedOnce,
        StrKey::DeniedSession,
        StrKey::HeadlessToolCall,
        StrKey::HeadlessPromptHint,
        StrKey::ConfigErrorPrefix,
        StrKey::HooksErrorPrefix,
        StrKey::YoloNotice,
        StrKey::UsageResume,
        StrKey::UsageFork,
        StrKey::UnknownApprovalMode,
        StrKey::TaskRestoreFailed,
        StrKey::MissingApiKey,
        StrKey::SubagentApiKey,
        StrKey::ReviewerApiKey,
        StrKey::NoSessions,
        StrKey::SessionsHeader,
        StrKey::UntitledSession,
        StrKey::LanguageCurrent,
        StrKey::LanguageSwitched,
        StrKey::LanguageWriteFailed,
        StrKey::LanguageInvalid,
    ];
}

/// Look up one UI string. Both language arms are exhaustive matches over `StrKey`, so a
/// key present in one language but missing from the other fails the build.
pub fn tr(lang: Lang, key: StrKey) -> &'static str {
    match lang {
        Lang::Zh => tr_zh(key),
        Lang::En => tr_en(key),
    }
}

/// Fill a template's positional `{}` placeholders (rustc requires a string literal as the
/// format! template, so runtime table templates are filled manually; each argument is
/// inserted verbatim — a "{}" inside an argument is never re-substituted).
pub fn trf(lang: Lang, key: StrKey, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::new();
    let mut rest = tr(lang, key);
    for a in args {
        match rest.find("{}") {
            Some(i) => {
                out.push_str(&rest[..i]);
                out.push_str(&a.to_string());
                rest = &rest[i + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

fn tr_zh(key: StrKey) -> &'static str {
    match key {
        StrKey::WelcomeHint => "DSCode — 输入消息后回车发送，Shift+Tab 切换审批模式，Ctrl+C / Ctrl+D 退出",
        StrKey::StatusIdle => "空闲",
        StrKey::StatusStreaming => "流式响应中",
        StrKey::StatusApproving => "审批 {}",
        StrKey::StatusModeSwitched => "（审批模式：{} → {}）",
        StrKey::StatusInputVetoed => "输入被 hook 否决：{}",
        StrKey::PanelChatTitle => "对话",
        StrKey::PanelTasksTitle => "任务",
        StrKey::PanelInputTitle => "输入",
        StrKey::ModeLabel => "模式:{}",
        StrKey::UserLine => "你：{}",
        StrKey::AssistantLine => "助手：{}",
        StrKey::ToolCallLine => "→ 执行工具 {}：{}",
        StrKey::ToolResultLine => "← 工具结果：{}",
        StrKey::TranscriptToolCall => "→ 执行工具 {}",
        StrKey::TranscriptToolResult => "← 结果：{}",
        StrKey::TranscriptApproval => "⚖ 审批 {} → {}",
        StrKey::GoalCreatedCard => "★ 新目标已创建：{}",
        StrKey::GoalChangedCard => "🎯 goal {}（→ {}）：{}",
        StrKey::CompactionSep => "──── 会话已压缩（完整历史仍在日志中）────",
        StrKey::GoalNotEnabled => "goal 栈未启用（goal.enabled=false 或 headless 模式）",
        StrKey::GoalShowCard => {
            "🎯 目标：{}\n状态：{}（revision {}）· round {}/{} · token {}/{}"
        }
        StrKey::GoalUnlimited => "不限",
        StrKey::GoalNoneHint => "当前没有 goal。用 /goal <objective> 创建一个。",
        StrKey::GoalPausedOk => "已暂停 goal（停止续行驱动）",
        StrKey::GoalResumedOk => "已恢复 goal 并重新启用续行驱动",
        StrKey::GoalClearedOk => "已清除 goal",
        StrKey::GoalEditedOk => "已更新 objective",
        StrKey::GoalCreatedOk => "已创建 goal",
        StrKey::GoalOpFailed => "（goal 操作失败：{}）",
        StrKey::GoalOpFailedDefault => "操作失败",
        StrKey::OkColon => "：",
        StrKey::BadgePausedSuffix => "（已暂停）",
        StrKey::BadgeStoppedSuffix => "（预算耗尽已停）",
        StrKey::BadgeCompleteSuffix => "（已完成）",
        StrKey::SuspendPanelTitle => "╔═ ⛔ 限额挂起 ══════════",
        StrKey::SuspendPanelReason => "║ 原因：{}",
        StrKey::SuspendPanelReset => "║ provider：{} · 预计恢复倒计时 {}",
        StrKey::SuspendPanelProbe => "║ provider：{} · 周期探测，下次 {}",
        StrKey::SuspendPanelKeys => "║ [r] 立即重试 · [c] 取消挂起 · [p] 收起面板（状态栏保留）",
        StrKey::StatusSuspendReset => "⛔ 限额挂起（{}）· 预计恢复 {}",
        StrKey::StatusSuspendProbe => "⛔ 限额挂起（{}）· 周期探测中，下次 {}",
        StrKey::Escalated429Reason => "连续 {} 次 429 限流失败，升级为限额挂起",
        StrKey::Status429Retry => "429 限流，{} 后重试（连续第 {} 次）",
        StrKey::SuspendCancelledErr => {
            "已取消限额挂起：不再自动探测；会话与记录已保留（可 dscode resume 继续）"
        }
        StrKey::HeadlessSuspendModeReset => "到点自动探测恢复",
        StrKey::HeadlessSuspendModeProbe => "周期退避探测恢复",
        StrKey::HeadlessSuspendLine => "⛔ 限额挂起：{}（{}，Ctrl+C 退出）",
        StrKey::AutoResumedNote => "限额已恢复，已自动续跑",
        StrKey::AutoResumedGoalNote => "\n已顺带重新启用 goal：{}",
        StrKey::ApprovalCardTitle => "═══ 审批卡：{}（tier {}）",
        StrKey::ApprovalCardArgs => "参数：{}",
        StrKey::ApprovalCardStep => "升级原因：{}",
        StrKey::ApprovalCardReviewer => "代审意见：{}",
        StrKey::ApprovalCriticalHit => "critical pattern 命中：毁灭性命令，无批准选项",
        StrKey::ApprovalAlwaysProposal => "「永远」档将写入规则：{}",
        StrKey::ApprovalKeysCritical => "n=拒绝 d=拒绝(本会话)",
        StrKey::ApprovalKeysFull => "y=批准 s=批准(本会话) a=永远批准 n=拒绝 d=拒绝(本会话)",
        StrKey::ApprovalKeysHint => "键位：{}（Ctrl+C 退出，超时按拒绝）",
        StrKey::ApprovalTimeoutNotice => "（审批超时，fail-closed 按拒绝处理）",
        StrKey::ApprovedOnce => "（已批准）",
        StrKey::ApprovedSession => "（已批准，本会话记住）",
        StrKey::ApprovedAlways => "（已批准，规则将写入项目配置）",
        StrKey::DeniedOnce => "（已拒绝）",
        StrKey::DeniedSession => "（已拒绝，本会话记住）",
        StrKey::HeadlessToolCall => "→ 工具调用 {}：{}",
        StrKey::HeadlessPromptHint => "请输入提示词（Ctrl+D 结束）：",
        StrKey::ConfigErrorPrefix => "配置错误：",
        StrKey::HooksErrorPrefix => "hooks 配置错误：",
        StrKey::YoloNotice => {
            "[审批] 未配置 modelRoles.approver，auto 有效落 yolo（本提示仅出现一次）。\n配置指引：~/.dscode/config.yaml 中添加 modelRoles.approver: <模型名>"
        }
        StrKey::UsageResume => "用法：dscode resume <session-id>",
        StrKey::UsageFork => "用法：dscode fork <session-id>",
        StrKey::UnknownApprovalMode => "未知审批模式「{}」（合法值：ask/auto/yolo）",
        StrKey::TaskRestoreFailed => "任务状态恢复失败：{}",
        StrKey::MissingApiKey => {
            "未找到 DEEPSEK_API_KEY（凭据四层：env > ~/.dscode/.credentials.yaml > 项目 .env > ~/.dscode/.env）"
        }
        StrKey::SubagentApiKey => "子代理派发需要 DEEPSEEK_API_KEY",
        StrKey::ReviewerApiKey => "代审需要 DEEPSEK_API_KEY",
        StrKey::NoSessions => "（本目录暂无会话）",
        StrKey::SessionsHeader => "会话 id\t创建时间\t标题",
        StrKey::UntitledSession => "（无标题）",
        StrKey::LanguageCurrent => "当前语言：{}（用 /language <zh|en> 切换；zh=中文 en=English）",
        StrKey::LanguageSwitched => "（显示语言已切换为 {}；已写入全局配置 ~/.dscode/config.yaml）",
        StrKey::LanguageWriteFailed => "（显示语言已切换为 {}，但写回全局配置失败：{}）",
        StrKey::LanguageInvalid => "未知的语言「{}」。用法：/language <zh|en>",
    }
}

fn tr_en(key: StrKey) -> &'static str {
    match key {
        StrKey::WelcomeHint => "DSCode — Enter to send, Shift+Tab to cycle the approval mode, Ctrl+C / Ctrl+D to exit",
        StrKey::StatusIdle => "idle",
        StrKey::StatusStreaming => "streaming",
        StrKey::StatusApproving => "approving {}",
        StrKey::StatusModeSwitched => "(approval mode: {} → {})",
        StrKey::StatusInputVetoed => "input vetoed by hook: {}",
        StrKey::PanelChatTitle => "Chat",
        StrKey::PanelTasksTitle => "Tasks",
        StrKey::PanelInputTitle => "Input",
        StrKey::ModeLabel => "mode:{}",
        StrKey::UserLine => "You: {}",
        StrKey::AssistantLine => "Assistant: {}",
        StrKey::ToolCallLine => "→ running tool {}: {}",
        StrKey::ToolResultLine => "← tool result: {}",
        StrKey::TranscriptToolCall => "→ running tool {}",
        StrKey::TranscriptToolResult => "← result: {}",
        StrKey::TranscriptApproval => "⚖ approval {} → {}",
        StrKey::GoalCreatedCard => "★ new goal created: {}",
        StrKey::GoalChangedCard => "🎯 goal {} (→ {}): {}",
        StrKey::CompactionSep => "──── session compacted (the full history remains in the log) ────",
        StrKey::GoalNotEnabled => "goal stack not enabled (goal.enabled=false or headless mode)",
        StrKey::GoalShowCard => {
            "🎯 objective: {}\nstatus: {} (revision {}) · round {}/{} · token {}/{}"
        }
        StrKey::GoalUnlimited => "unlimited",
        StrKey::GoalNoneHint => "No goal yet. Create one with /goal <objective>.",
        StrKey::GoalPausedOk => "goal paused (round driving stopped)",
        StrKey::GoalResumedOk => "goal resumed and round driving re-armed",
        StrKey::GoalClearedOk => "goal cleared",
        StrKey::GoalEditedOk => "objective updated",
        StrKey::GoalCreatedOk => "goal created",
        StrKey::GoalOpFailed => "(goal operation failed: {})",
        StrKey::GoalOpFailedDefault => "operation failed",
        StrKey::OkColon => ": ",
        StrKey::BadgePausedSuffix => " (paused)",
        StrKey::BadgeStoppedSuffix => " (stopped: budget exhausted)",
        StrKey::BadgeCompleteSuffix => " (complete)",
        StrKey::SuspendPanelTitle => "╔═ ⛔ limit suspension ══════════",
        StrKey::SuspendPanelReason => "║ reason: {}",
        StrKey::SuspendPanelReset => "║ provider: {} · estimated recovery in {}",
        StrKey::SuspendPanelProbe => "║ provider: {} · periodic probing, next in {}",
        StrKey::SuspendPanelKeys => {
            "║ [r] retry now · [c] cancel suspension · [p] collapse panel (the status bar keeps the signal)"
        }
        StrKey::StatusSuspendReset => "⛔ suspended ({}) · estimated recovery {}",
        StrKey::StatusSuspendProbe => "⛔ suspended ({}) · periodic probing, next {}",
        StrKey::Escalated429Reason => "{} consecutive 429 rate-limit failures, escalated to limit suspension",
        StrKey::Status429Retry => "429 rate-limited, retrying in {} (failure #{})",
        StrKey::SuspendCancelledErr => {
            "limit suspension cancelled: no further auto-probing; the session and its records are kept (dscode resume to continue)"
        }
        StrKey::HeadlessSuspendModeReset => "auto-probe at the reset time",
        StrKey::HeadlessSuspendModeProbe => "periodic backoff probing",
        StrKey::HeadlessSuspendLine => "⛔ limit suspension: {} ({}, Ctrl+C to exit)",
        StrKey::AutoResumedNote => "limit recovered, auto-continuing",
        StrKey::AutoResumedGoalNote => "\ngoal re-armed as well: {}",
        StrKey::ApprovalCardTitle => "═══ approval card: {} (tier {})",
        StrKey::ApprovalCardArgs => "args: {}",
        StrKey::ApprovalCardStep => "escalation reason: {}",
        StrKey::ApprovalCardReviewer => "auto-review opinion: {}",
        StrKey::ApprovalCriticalHit => "critical pattern hit: destructive command, no approve option",
        StrKey::ApprovalAlwaysProposal => "\"always\" tier will write the rule: {}",
        StrKey::ApprovalKeysCritical => "n=deny d=deny(session)",
        StrKey::ApprovalKeysFull => "y=approve s=approve(session) a=always approve n=deny d=deny(session)",
        StrKey::ApprovalKeysHint => "keys: {} (Ctrl+C exits, timeout denies)",
        StrKey::ApprovalTimeoutNotice => "(approval timeout, fail-closed treated as deny)",
        StrKey::ApprovedOnce => "(approved)",
        StrKey::ApprovedSession => "(approved, remembered for this session)",
        StrKey::ApprovedAlways => "(approved, the rule will be written to project config)",
        StrKey::DeniedOnce => "(denied)",
        StrKey::DeniedSession => "(denied, remembered for this session)",
        StrKey::HeadlessToolCall => "→ tool call {}: {}",
        StrKey::HeadlessPromptHint => "Enter a prompt (Ctrl+D to finish):",
        StrKey::ConfigErrorPrefix => "config error: ",
        StrKey::HooksErrorPrefix => "hooks config error: ",
        StrKey::YoloNotice => {
            "[approval] modelRoles.approver is not configured; auto effectively lands on yolo (this notice appears only once).\nGuidance: add modelRoles.approver: <model> to ~/.dscode/config.yaml"
        }
        StrKey::UsageResume => "usage: dscode resume <session-id>",
        StrKey::UsageFork => "usage: dscode fork <session-id>",
        StrKey::UnknownApprovalMode => "unknown approval mode \"{}\" (valid: ask/auto/yolo)",
        StrKey::TaskRestoreFailed => "task state restore failed: {}",
        StrKey::MissingApiKey => {
            "DEEPSEK_API_KEY not found (four credential tiers: env > ~/.dscode/.credentials.yaml > project .env > ~/.dscode/.env)"
        }
        StrKey::SubagentApiKey => "sub-agent dispatch requires DEEPSEK_API_KEY",
        StrKey::ReviewerApiKey => "auto-review requires DEEPSEK_API_KEY",
        StrKey::NoSessions => "(no sessions in this directory yet)",
        StrKey::SessionsHeader => "session id\tcreated\ttitle",
        StrKey::UntitledSession => "(untitled)",
        StrKey::LanguageCurrent => "current language: {} (switch with /language <zh|en>; zh=中文 en=English)",
        StrKey::LanguageSwitched => "(display language switched to {}; written to the global config ~/.dscode/config.yaml)",
        StrKey::LanguageWriteFailed => "(display language switched to {}, but writing the global config failed: {})",
        StrKey::LanguageInvalid => "unknown language \"{}\". usage: /language <zh|en>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 全部键两语言非空() {
        for key in StrKey::ALL {
            assert!(!tr(Lang::Zh, *key).is_empty(), "zh 缺文案：{key:?}");
            assert!(!tr(Lang::En, *key).is_empty(), "en 缺文案：{key:?}");
        }
    }

    #[test]
    fn parse合法与非法() {
        assert_eq!(Lang::parse("zh").unwrap(), Lang::Zh);
        assert_eq!(Lang::parse("en").unwrap(), Lang::En);
        assert_eq!(Lang::parse(" en ").unwrap(), Lang::En, "容忍首尾空白");
        assert!(Lang::parse("fr").is_err());
        assert!(Lang::parse("").is_err());
        assert!(Lang::parse("english").is_err());
    }

    #[test]
    fn 默认语言为中文() {
        assert_eq!(Lang::default(), Lang::Zh);
        assert_eq!(Lang::Zh.as_str(), "zh");
        assert_eq!(Lang::En.as_str(), "en");
    }

    #[test]
    fn trf按位置填充参数且不二次替换() {
        assert_eq!(trf(Lang::Zh, StrKey::UserLine, &[&"你好"]), "你：你好");
        assert_eq!(trf(Lang::En, StrKey::UserLine, &[&"hi"]), "You: hi");
        // An argument containing "{}" is inserted verbatim, never re-substituted.
        assert_eq!(trf(Lang::En, StrKey::UserLine, &[&"{}"]), "You: {}");
        // No-arg fill round-trips the template itself.
        assert_eq!(trf(Lang::Zh, StrKey::StatusIdle, &[]), "空闲");
        // Multi-placeholder order.
        assert_eq!(
            trf(Lang::En, StrKey::StatusModeSwitched, &[&"ask", &"yolo"]),
            "(approval mode: ask → yolo)"
        );
    }
}
