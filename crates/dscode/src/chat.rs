//! Shared conversation-turn logic: reused by TUI and headless, with UI differences abstracted behind UiSink.
//! Phase 1: tool registry dispatch, approval gate (decision chain → auto-review/human card/deny + paired audit events),
//! declarative hooks, compaction threshold auto-track, first-turn title generation.

use crate::approval::provider::{Answer, ApprovalProvider, DecisionCard};
use crate::approval::{
    self, audit_pair, Asked, ChainStep, Decided, Decider, Decision, Mode, Remember, UserDecisions,
};
use crate::config::Config;
use crate::goal::GoalRuntime;
use crate::hooks::{HookEvent, HookOutcome, Hooks};
use crate::i18n::{tr, trf, Lang, StrKey};
use crate::llm::{ChatEvent, LlmProvider, Message, ToolCall};
use crate::session::SessionLog;
use crate::tool::{Tier, ToolCtx, ToolOutput};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

pub trait UiSink: Send {
    fn on_status(&mut self, status: &str);
    fn on_user(&mut self, text: &str);
    fn on_delta(&mut self, text: &str);
    fn on_assistant_done(&mut self, content: &str, tool_calls: &[ToolCall]);
    fn on_tool_call(&mut self, call: &ToolCall);
    fn on_tool_result(&mut self, tool_call_id: &str, output: &str);
    /// Human decision card: render the card and collect the answer. Defaults to Unavailable — headless and other
    /// UI-less frontends handle it as a fail-closed denial (approval.zh.md §2.6).
    fn on_approval_card(&mut self, _card: DecisionCard) -> Answer {
        Answer::Unavailable
    }
    /// One suspension tick (limits.zh.md §TUI): render the suspend panel + status bar and
    /// report the user's wish. Default Wait = no interactivity (headless/test UIs); the
    /// chat loop still auto-probes when autoContinue.enabled.
    fn on_suspend_tick(
        &mut self,
        _info: &crate::limits::SuspendInfo,
    ) -> crate::limits::SuspendAction {
        crate::limits::SuspendAction::Wait
    }
    /// Suspension ended (recovered / reclassified / cancelled): drop the panel signal.
    fn on_suspend_end(&mut self) {}
    /// Auto-continue recovered the session (limits.zh.md): the note may carry the re-armed
    /// goal (shared highlight budget); the chat loop guards one-time-per-session.
    fn on_auto_resumed(&mut self, _note: &str) {}
    fn on_config_reloaded(&mut self, _model: &str, _approver_ready: bool, _lang: Lang) {}
}

/// Turn context: session-level state for approval and hooks, threaded through the turn loop.
pub struct ChatCtx<'a> {
    pub config: Config,
    pub hooks: Hooks,
    /// Session-pinned approval mode; switches are logged as approval/policy and take effect on the next approval.
    pub mode: Mode,
    /// Session-scoped remembered decisions (§2.6 session tier, in-memory).
    pub decisions: UserDecisions,
    /// Auto-review provider for auto mode; None when no approver is configured (the effective mode has already fallen to yolo).
    pub reviewer: Option<&'a dyn ApprovalProvider>,
    /// Session-resident task state, shared by the task tools and the TUI panel (tools.zh.md §3.7).
    pub tasks: std::sync::Arc<crate::tool::task::TaskStore>,
    /// Session-bound read/grep provenance and named edit registers.
    pub edits: Arc<crate::tool::edit::EditSession>,
    /// Goal runtime handle (goal.zh.md): None = goal stack not mounted (headless / goal.enabled=false) —
    /// no goal tools, no continuation driver. Shared with the TUI (badge + /goal commands).
    pub goal: Option<Arc<Mutex<GoalRuntime>>>,
    /// Shared sub-agent host: lifecycle registry + hub + async result deliveries
    /// (tools.zh.md §3.8/§3.9). Constructed once in main; threaded to tools via ToolCtx.
    pub agents: Arc<crate::agent::AgentHost>,
    /// Limit-recovery state (limits.zh.md): autoContinue switch + backoff knobs + the
    /// one-time auto-resume highlight guard.
    pub limits: crate::limits::LimitsCtl,
    /// Display language (tui.language): shared by every user-visible string routed through
    /// this context; the TUI's /language updates it alongside its own copy.
    pub lang: Lang,
    pub(crate) request_header_written: bool,
    pub(crate) last_request_header: Option<String>,
    pub(crate) last_request_route: Option<(String, String)>,
    pub(crate) config_fingerprint: u64,
}

/// Context window estimation base (characters): deepseek's 64k token window conservatively estimated at 1 Chinese char ≈ 1 token.
const CONTEXT_CHARS: f64 = 60_000.0;
const SUMMARY_SYSTEM: &str =
    "你是会话压缩器。把对话压缩为保留任务、决定、未完成事项的摘要，不超过 400 字。仅输出摘要正文。";
const TITLE_SYSTEM: &str =
    "你是标题生成器。为下面的用户消息生成不超过 16 字的会话标题。仅输出标题文本，不带引号。";

async fn reload_config_if_changed<P: LlmProvider>(
    provider: &mut P,
    ui: &mut dyn UiSink,
    ctx: &mut ChatCtx<'_>,
) -> Result<(), String> {
    let fingerprint = crate::config::config_fingerprint();
    if fingerprint == ctx.config_fingerprint {
        return Ok(());
    }
    let config = Config::load()?;
    let hooks = Hooks::load(&config)?;
    apply_reloaded_config(provider, ui, ctx, config, hooks, fingerprint).await
}

async fn apply_reloaded_config<P: LlmProvider>(
    provider: &mut P,
    ui: &mut dyn UiSink,
    ctx: &mut ChatCtx<'_>,
    config: Config,
    hooks: Hooks,
    fingerprint: u64,
) -> Result<(), String> {
    provider.reload_config(&config).await?;
    ctx.lang = config.language;
    ctx.limits.auto_continue = config.auto_continue_enabled;
    ctx.hooks = hooks;
    ctx.config = config;
    ctx.config_fingerprint = fingerprint;
    ui.on_config_reloaded(provider.model_name(), ctx.reviewer.is_some(), ctx.lang);
    Ok(())
}

pub(crate) fn rebuild_messages(events: &[crate::session::Event]) -> Vec<Message> {
    events
        .iter()
        .filter_map(|event| match event.kind.as_str() {
            "user/message" => event
                .data
                .get("content")
                .and_then(Value::as_str)
                .map(|text| Message::User(text.to_string())),
            "assistant/message" => Some(Message::Assistant {
                content: event
                    .data
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                tool_calls: event
                    .data
                    .get("tool_calls")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
                    .unwrap_or_default(),
            }),
            "tool/result" => Some(Message::Tool {
                tool_call_id: event
                    .data
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                content: event
                    .data
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            }),
            _ => None,
        })
        .collect()
}

/// Enqueue the user message + write it to the log, then run one full turn (possibly multiple tool loops).
pub async fn send_user_message<P: LlmProvider>(
    provider: &mut P,
    messages: &mut Vec<Message>,
    log: &mut SessionLog,
    ui: &mut dyn UiSink,
    ctx: &mut ChatCtx<'_>,
    turn: u64,
    text: &str,
) -> Result<(), String> {
    reload_config_if_changed(provider, ui, ctx).await?;
    // UserPromptSubmit hooks: block vetoes this input, rewrite rewrites it before enqueuing.
    let text = match ctx.hooks.dispatch(HookEvent::UserPromptSubmit, None, text) {
        HookOutcome::Blocked { reason } => {
            ui.on_status(&trf(ctx.lang, StrKey::StatusInputVetoed, &[&reason]));
            return Ok(());
        }
        HookOutcome::Rewritten { input } => input,
        HookOutcome::Proceed => text.to_string(),
    };
    // Host proof (goal.zh.md): this turn contains an accepted {kind:'user'} message —
    // the only context where create_goal is granted. Goal rounds and subagent turns never set this.
    if let Some(rt) = &ctx.goal {
        rt.lock().begin_turn(true, false);
    }
    messages.push(Message::User(text.clone()));
    log.log("turn/start", json!({ "turn": turn }));
    log.log(
        "user/message",
        json!({ "content": &text, "source": "user" }),
    );
    ui.on_user(&text);
    run_turn(provider, messages, log, ui, ctx, turn).await?;
    flush_goal_events(log, ctx);
    // Goal-round-driver: after the turn naturally ends, drive <goal_round> continuation
    // turns while the goal is active + armed and neither budget is exhausted (goal.zh.md).
    drive_goal_rounds(provider, messages, log, ui, ctx, turn).await?;
    // Generate a title when this session's log has none yet (session/title; forked sessions get their own titles too).
    // Precise commit-role routing is a phase-3 refinement; use the main provider for now.
    if log.needs_title() {
        if let Ok(title) = provider.complete(TITLE_SYSTEM, &text).await {
            if !title.trim().is_empty() {
                log.log("session/title", json!({ "title": title.trim() }));
            }
        }
    }
    maybe_compact(provider, messages, log, ui, ctx).await
}

/// One full turn's observable activity, consumed by the goal driver's progress test.
pub struct TurnStats {
    pub tool_calls: usize,
    pub assistant_text: usize,
}

async fn run_turn<P: LlmProvider>(
    provider: &mut P,
    messages: &mut Vec<Message>,
    log: &mut SessionLog,
    ui: &mut dyn UiSink,
    ctx: &mut ChatCtx<'_>,
    turn: u64,
) -> Result<TurnStats, String> {
    ctx.agents.refresh_mcp().await?;
    let mut registry = crate::tool::Registry::with_tasks(ctx.tasks.clone(), ctx.edits.clone());
    // Goal tools mount only when the runtime handle exists (headless / goal.enabled=false → absent
    // from the model-visible schema list — the spec's conditional gating).
    if let Some(goal) = &ctx.goal {
        registry = registry.and_goal(goal.clone());
    }
    registry.extend_shared(&ctx.agents.shared_tools().snapshot());
    let tools = registry.definitions();
    let provider_name = provider.provider_name().to_string();
    let model_name = provider.model_name().to_string();
    let route = (provider_name.clone(), model_name.clone());
    if ctx.last_request_route.as_ref() != Some(&route) {
        log.log(
            "request/context",
            json!({ "provider": &provider_name, "model": &model_name }),
        );
        ctx.last_request_route = Some(route);
    }
    let mut header = json!({
        "config": {
            "provider": &provider_name,
            "model": &model_name,
        }
    });
    if let Some(system) = messages.iter().find_map(|message| match message {
        Message::System(system) => Some(system.as_str()),
        _ => None,
    }) {
        header["system"] = json!(system);
    }
    if !tools.is_empty() {
        header["tools"] = json!(&tools);
    }
    let fingerprint = header.to_string();
    if !ctx.request_header_written
        || ctx.last_request_header.as_deref() != Some(fingerprint.as_str())
    {
        let reason = if ctx.request_header_written {
            "change"
        } else if log
            .read_all()
            .unwrap_or_default()
            .iter()
            .any(|event| event.kind == "request/header")
        {
            "resume"
        } else {
            "initial"
        };
        log.log(
            "request/header",
            json!({ "header": header, "reason": reason }),
        );
        ctx.request_header_written = true;
        ctx.last_request_header = Some(fingerprint);
    }
    let mut stats = TurnStats {
        tool_calls: 0,
        assistant_text: 0,
    };
    ui.on_status(tr(ctx.lang, StrKey::StatusStreaming));
    // Limit-recovery state (limits.zh.md): rate-class backoff count + quota-window debounce
    // + the auto-probe marker that gates the rearm/highlight side effects.
    let mut rate_fails: u32 = 0;
    let mut window: Option<String> = None;
    let mut ladder: usize = 0;
    let mut auto_probe_fired = false;
    loop {
        // Async sub-agent deliveries: completed background spawn results inject into the
        // conversation flow here (tools.zh.md §3.8 — auto-delivery on completion).
        for (id, text) in ctx.agents.take_pending() {
            let msg = format!("[子代理 {id} 完成]\n{text}");
            log.log(
                "user/message",
                json!({ "content": &msg, "source": "agent" }),
            );
            ui.on_user(&msg);
            messages.push(Message::User(msg));
        }
        // agent/* events (spawned/completed/message) land in the session log;
        // the log stays single-writer — only the turn loop appends.
        for (kind, data) in ctx.agents.drain_events() {
            log.log(&kind, data);
        }
        let mut content = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let attempted = {
            // Clone the Arc out so the closure can charge the goal budget without borrowing ctx.
            let goal_rt = ctx.goal.clone();
            let mut on_event = |ev: ChatEvent| match ev {
                ChatEvent::Delta(t) => {
                    log.log("assistant/chunk", json!({ "turn": turn, "text": t }));
                    ui.on_delta(&t);
                    content.push_str(&t);
                }
                ChatEvent::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
                ChatEvent::Usage { total_tokens } => {
                    // Goal token budget: every model request's usage accumulates over the goal
                    // lifetime; the pause window is excluded inside the state (charge only while active).
                    if let Some(rt) = &goal_rt {
                        rt.lock().charge_tokens(total_tokens);
                    }
                }
            };
            provider.chat_stream(messages, &tools, &mut on_event).await
        };
        match attempted {
            Ok(()) => {
                // Auto-continue recovered the session (limits.zh.md §goal rearm): rearm a disarmed
                // active goal — one `goal/rearm` audit per successful goal-carrying auto recovery —
                // sharing the one-time-per-session highlight with the resume notice.
                if auto_probe_fired {
                    auto_probe_fired = false;
                    let mut note = tr(ctx.lang, StrKey::AutoResumedNote).to_string();
                    if let Some(rt) = &ctx.goal {
                        if let Some(g) = rt.lock().rearm_for_continue() {
                            log.log(
                                "goal/rearm",
                                json!({ "reason": "auto-continue", "objective": g.objective, "revision": g.revision }),
                            );
                            note.push_str(&trf(
                                ctx.lang,
                                StrKey::AutoResumedGoalNote,
                                &[&g.objective],
                            ));
                        }
                    }
                    if !ctx.limits.highlight_shown {
                        ctx.limits.highlight_shown = true;
                        ui.on_auto_resumed(&note);
                    }
                }
            }
            Err(e) => {
                let cfg = ctx.limits.backoff.clone();
                let (reason, reset_at) = match crate::limits::classify_error(&e) {
                    // 无法归类：普通错误呈现，绝不挂起（limits.zh.md §错误分类）。
                    crate::limits::ErrorClass::Unknown => return Err(e),
                    crate::limits::ErrorClass::Rate => {
                        rate_fails += 1;
                        if rate_fails < cfg.rate_escalate_after {
                            // 速率类原位指数退避重试（1s→2s→…→60s）。
                            let d = crate::limits::rate_backoff(rate_fails, &cfg);
                            ui.on_status(&trf(
                                ctx.lang,
                                StrKey::Status429Retry,
                                &[
                                    &crate::limits::fmt_secs(d) as &dyn std::fmt::Display,
                                    &rate_fails,
                                ],
                            ));
                            tokio::time::sleep(d).await;
                            continue;
                        }
                        // 连续 5 次失败：升级为限额类处理（挂起面板接管）。
                        (
                            trf(
                                ctx.lang,
                                StrKey::Escalated429Reason,
                                &[&cfg.rate_escalate_after],
                            ),
                            None,
                        )
                    }
                    crate::limits::ErrorClass::Quota { reset_at } => {
                        (crate::limits::suspend_reason(&e), reset_at)
                    }
                };
                // 防抖（limits.zh.md §退避与防抖）：同一配额窗口内重复限额错误不重置阶梯。
                let key = crate::limits::window_key(&e, reset_at);
                let same = window.as_deref() == Some(key.as_str());
                ladder = crate::limits::advance_ladder(ladder, same);
                window = Some(key);
                // —— 挂起：进程本地暂停态；不开新 turn、不回滚，恢复即重发同一未完成请求 ——
                let auto_on = ctx.limits.auto_continue;
                let mut cancelled = false;
                let now = crate::limits::now_ms();
                let ladder_ms = crate::limits::ladder_wait(ladder, &cfg).as_millis() as u64;
                let next_probe_at = match reset_at {
                    Some(t) => t.saturating_add(cfg.reset_margin_ms).max(now + ladder_ms),
                    None => now + ladder_ms,
                };
                loop {
                    let info = crate::limits::SuspendInfo {
                        reason: reason.clone(),
                        reset_at,
                        next_probe_at,
                    };
                    match ui.on_suspend_tick(&info) {
                        // Manual retry probes now without re-arming a goal.
                        crate::limits::SuspendAction::RetryNow => break,
                        crate::limits::SuspendAction::ReloadProvider => {
                            let config = crate::config::Config::load()?;
                            provider.reload_config(&config).await?;
                            ctx.lang = config.language;
                            ctx.limits.auto_continue = config.auto_continue_enabled;
                            ctx.config = config;
                            break;
                        }
                        crate::limits::SuspendAction::Cancel => {
                            cancelled = true;
                            break;
                        }
                        crate::limits::SuspendAction::Wait => {}
                    }
                    if auto_on && crate::limits::now_ms() >= next_probe_at {
                        auto_probe_fired = true;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(cfg.tick_ms)).await;
                }
                ui.on_suspend_end();
                if cancelled {
                    // 用户取消：永久放弃本次挂起；不再自动探测，会话与记录保留
                    //（挂起期进程退出由普通 resume 路径覆盖，无需新代码）。
                    return Err(tr(ctx.lang, StrKey::SuspendCancelledErr).into());
                }
                // 探测 = 重发同一未完成请求；再失败则回到本循环顶部重新分类（防抖推进阶梯）。
                continue;
            }
        }
        log.log(
            "assistant/message",
            json!({ "content": content, "tool_calls": tool_calls }),
        );
        ui.on_assistant_done(&content, &tool_calls);
        stats.assistant_text += content.chars().count();
        messages.push(Message::Assistant {
            content: content.clone(),
            tool_calls: tool_calls.clone(),
        });
        if tool_calls.is_empty() {
            break;
        }
        stats.tool_calls += tool_calls.len();
        for call in &tool_calls {
            log.log(
                "tool/call",
                json!({ "turn": turn, "callId": call.id, "name": call.name, "arguments": call.arguments }),
            );
            ui.on_status(&trf(ctx.lang, StrKey::StatusApproving, &[&call.name]));
            ui.on_tool_call(call);
            let mut args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
            // PreToolUse hooks: block vetoes, rewrite modifies the arguments (config-onboarding.zh.md §hooks).
            match ctx
                .hooks
                .dispatch(HookEvent::PreToolUse, Some(&call.name), &args.to_string())
            {
                HookOutcome::Blocked { reason } => {
                    finish_tool(log, ui, &call.id, &format!("hook 否决：{reason}"), None);
                    messages.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        content: format!("hook 否决：{reason}"),
                    });
                    continue;
                }
                HookOutcome::Rewritten { input } => {
                    args = serde_json::from_str(&input).unwrap_or(args);
                }
                HookOutcome::Proceed => {}
            }
            // Approval gate: decision chain → allow/deny/auto-review/human card, with paired audit events logged.
            let tier = registry
                .get(&call.name)
                .map(|t| t.tier())
                .unwrap_or(Tier::Exec);
            let (allowed, deny_reason) =
                gate(ctx, ui, log, &call.name, tier, &args, messages).await;
            let result = match (allowed, registry.get(&call.name)) {
                (false, _) => ToolOutput {
                    output: format!(
                        "审批拒绝：{}",
                        deny_reason.unwrap_or_else(|| "未知原因".into())
                    ),
                    exit_code: None,
                },
                (true, Some(tool)) => {
                    // Top-level execution context: Main identity, depth 0, full decision snapshot.
                    let tctx = ToolCtx {
                        config: &ctx.config,
                        agents: &ctx.agents,
                        agent_id: "Main",
                        def_name: None,
                        depth: 0,
                        is_subagent: false,
                        cwd: None,
                        decisions: Some(ctx.decisions.snapshot()),
                    };
                    tool.execute_ctx(&tctx, &args).await
                }
                (true, None) => ToolOutput {
                    output: format!("未知工具：{}", call.name),
                    exit_code: None,
                },
            };
            let output = result.output;
            log.log(
                "tool/result",
                json!({ "turn": turn, "callId": call.id, "output": &output, "exit_code": result.exit_code }),
            );
            // Task mutations recorded by the task tools persist as task/write events (tools.zh.md §3.7).
            ctx.tasks.flush(log);
            // Goal transitions queued by the goal tools persist as goal/change events (goal.zh.md).
            flush_goal_events(log, ctx);
            ui.on_tool_result(&call.id, &output);
            let _ = ctx
                .hooks
                .dispatch(HookEvent::PostToolUse, Some(&call.name), &output);
            messages.push(Message::Tool {
                tool_call_id: call.id.clone(),
                content: output,
            });
        }
        ui.on_status(tr(ctx.lang, StrKey::StatusStreaming));
    }
    log.log("turn/end", json!({ "turn": turn, "reason": "completed" }));
    let _ = ctx.hooks.dispatch(HookEvent::Stop, None, "");
    ui.on_status(tr(ctx.lang, StrKey::StatusIdle));
    Ok(stats)
}

fn finish_tool(
    log: &mut SessionLog,
    ui: &mut dyn UiSink,
    id: &str,
    output: &str,
    exit_code: Option<i32>,
) {
    log.log(
        "tool/result",
        json!({ "tool_call_id": id, "output": output, "exit_code": exit_code }),
    );
    ui.on_tool_result(id, output);
}

/// Write the goal runtime's queued `goal/change` events into the log (append-only; the
/// snapshot rides with the event; arming never does — it is process-local).
fn flush_goal_events(log: &mut SessionLog, ctx: &ChatCtx<'_>) {
    if let Some(rt) = &ctx.goal {
        for data in rt.lock().drain_events() {
            log.log("goal/change", data);
        }
    }
}

/// One driver decision: stop (optionally surfacing the hard-stop reason) or drive one round.
enum DriveStep {
    Stop(Option<&'static str>),
    Drive { prompt: String, round_no: u64 },
}

/// Decide and reserve atomically under one lock: drive only while active + armed; hard stop
/// when either budget leaves no room for a new round (the in-flight round always completes);
/// the soft warning (<20% remaining) rides inside the reserved round's prompt.
fn next_drive_step(rt: &Arc<Mutex<GoalRuntime>>) -> DriveStep {
    let mut g = rt.lock();
    if !g.state.should_drive() {
        return DriveStep::Stop(None);
    }
    if let Some(reason) = g.state.stop_reason() {
        return DriveStep::Stop(Some(reason));
    }
    let Some(round_no) = g.state.get().map(|goal| goal.rounds_used + 1) else {
        return DriveStep::Stop(None);
    };
    if !g.state.charge_round() {
        return DriveStep::Stop(Some("rounds 预算已耗尽"));
    }
    let warning = g.state.soft_warning();
    match g.goal_round_prompt(round_no, warning.as_deref()) {
        Some(prompt) => DriveStep::Drive { prompt, round_no },
        None => DriveStep::Stop(None),
    }
}

/// goal-round-driver (goal.zh.md): after a turn naturally ends, keep driving continuation
/// rounds with the `<goal_round>` prompt while the goal is active + armed and neither budget
/// is exhausted. Human messages are ordinary turns and consume no budget; goal rounds consume
/// both. Three consecutive rounds without progress force `blocked` (dsh hard floor).
async fn drive_goal_rounds<P: LlmProvider>(
    provider: &mut P,
    messages: &mut Vec<Message>,
    log: &mut SessionLog,
    ui: &mut dyn UiSink,
    ctx: &mut ChatCtx<'_>,
    turn: u64,
) -> Result<(), String> {
    let Some(rt) = ctx.goal.clone() else {
        return Ok(());
    };
    loop {
        match next_drive_step(&rt) {
            DriveStep::Stop(Some(reason)) => {
                // Hard stop: the goal stays active-but-stopped; the TUI status line explains why.
                ui.on_status(reason);
                return Ok(());
            }
            DriveStep::Stop(None) => return Ok(()),
            DriveStep::Drive { prompt, round_no } => {
                // A goal round grants no host proof (no create_goal) and marks the round so
                // complete/blocked become legal update actions inside it.
                rt.lock().begin_turn(false, true);
                messages.push(Message::User(prompt.clone()));
                log.log("turn/start", json!({ "turn": turn, "goalRound": round_no }));
                log.log(
                    "user/message",
                    json!({ "content": &prompt, "source": "goal-round" }),
                );
                ui.on_user(&prompt);
                let stats = run_turn(provider, messages, log, ui, ctx, turn).await?;
                flush_goal_events(log, ctx);
                // Progress test (implementation interpretation, testable definition): a round
                // made progress when the model called at least one tool OR produced assistant text.
                let had_progress = stats.tool_calls > 0 || stats.assistant_text > 0;
                if let Some((_goal, event)) = rt.lock().state.record_round_progress(had_progress) {
                    log.log("goal/change", event);
                    ui.on_status("连续三轮无进展，goal 已强制 blocked（consecutive-no-progress）");
                    return Ok(());
                }
            }
        }
    }
}

/// Tool-call approval gate: six-step decision chain (pure function) → by final value: allow / rule-deny / auto-review / human card.
/// Each triggered gate decision emits paired `approval/asked` + `approval/decided` audit events (log-only).
/// Returns (allowed, deny reason); the reason flows back to the model with the tool result (§2.9).
async fn gate(
    ctx: &mut ChatCtx<'_>,
    ui: &mut dyn UiSink,
    log: &mut SessionLog,
    tool_name: &str,
    tier: Tier,
    args: &Value,
    messages: &[Message],
) -> (bool, Option<String>) {
    use approval::ChainAction::*;
    let outcome = approval::decide(
        tool_name,
        tier,
        args,
        &ctx.config.rules,
        ctx.mode,
        &ctx.decisions,
    );
    let summary = session_summary(messages);
    match outcome {
        Allow(_) => (true, None),
        Deny(step) => {
            write_audit(
                log,
                tool_name,
                tier,
                args,
                step.clone(),
                ctx.mode,
                Decider::Rule,
                Decided {
                    decision: Decision::Deny,
                    decider: Decider::Rule,
                    reason: Some(format!("决议链步骤 {}", step.as_str())),
                    remember: None,
                    always_rule: None,
                },
            );
            (false, Some(format!("被规则拒绝（{}）", step.as_str())))
        }
        EscalateHuman(step) => {
            human_card(ctx, ui, log, tool_name, tier, args, step, None, &summary).await
        }
        AutoReview(step) => {
            let Some(reviewer) = ctx.reviewer else {
                // Defensive: without an approver configured, the effective mode should be yolo; reaching this branch means fail-closed deny.
                write_audit(
                    log,
                    tool_name,
                    tier,
                    args,
                    step,
                    ctx.mode,
                    Decider::HeadlessReject,
                    Decided {
                        decision: Decision::RejectHeadless,
                        decider: Decider::HeadlessReject,
                        reason: Some("未配置代审模型".into()),
                        remember: None,
                        always_rule: None,
                    },
                );
                return (false, Some("未配置代审模型，fail-closed 拒绝".into()));
            };
            let card = DecisionCard {
                tool: tool_name.to_string(),
                tier,
                args: args.clone(),
                step: step.clone(),
                reviewer_reason: None,
                always_proposal: approval::propose_always_rule(tool_name, args),
                session_summary: summary.clone(),
                approved_rules: approved_rules_summary(ctx),
            };
            match reviewer.ask(card).await {
                Answer::Approve { .. } => {
                    write_audit(
                        log,
                        tool_name,
                        tier,
                        args,
                        step.clone(),
                        ctx.mode,
                        Decider::Reviewer,
                        Decided {
                            decision: Decision::Approve,
                            decider: Decider::Reviewer,
                            reason: None,
                            remember: None,
                            always_rule: None,
                        },
                    );
                    (true, None)
                }
                Answer::Deny { reason, .. } => {
                    write_audit(
                        log,
                        tool_name,
                        tier,
                        args,
                        step.clone(),
                        ctx.mode,
                        Decider::Reviewer,
                        Decided {
                            decision: Decision::Deny,
                            decider: Decider::Reviewer,
                            reason: Some(reason.clone()),
                            remember: None,
                            always_rule: None,
                        },
                    );
                    // On deny the user is the final authority and may approve against the reviewer's deny (§2.11) —
                    // the card shows the reviewer reason verbatim; the user's ruling wins.
                    let (_, deny) = human_card(
                        ctx,
                        ui,
                        log,
                        tool_name,
                        tier,
                        args,
                        step,
                        Some(reason.clone()),
                        &summary,
                    )
                    .await;
                    match deny {
                        None => (true, None),
                        Some(_) => (false, Some(format!("代审拒绝：{reason}"))),
                    }
                }
                // Auto-review escalation or abnormal fail-closed: TUI degrades to a human decision card; headless denies (§2.4).
                Answer::EscalateToHuman | Answer::Unavailable => {
                    human_card(
                        ctx,
                        ui,
                        log,
                        tool_name,
                        tier,
                        args,
                        step,
                        Some("代审拿不准或不可用，升级人工裁决".into()),
                        &summary,
                    )
                    .await
                }
            }
        }
    }
}

/// Human decision card path: audit asked(decider=Human) → collect answer → audit decided → apply remembered decision.
async fn human_card(
    ctx: &mut ChatCtx<'_>,
    ui: &mut dyn UiSink,
    log: &mut SessionLog,
    tool_name: &str,
    tier: Tier,
    args: &Value,
    step: ChainStep,
    reviewer_reason: Option<String>,
    summary: &str,
) -> (bool, Option<String>) {
    let card = DecisionCard {
        tool: tool_name.to_string(),
        tier,
        args: args.clone(),
        step: step.clone(),
        reviewer_reason,
        always_proposal: approval::propose_always_rule(tool_name, args),
        session_summary: summary.to_string(),
        approved_rules: approved_rules_summary(ctx),
    };
    let answer = ui.on_approval_card(card);
    match answer {
        Answer::Approve { remember } => {
            apply_remember(ctx, tool_name, args, remember, true);
            write_audit(
                log,
                tool_name,
                tier,
                args,
                step,
                ctx.mode,
                Decider::Human,
                Decided {
                    decision: Decision::Approve,
                    decider: Decider::Human,
                    reason: None,
                    remember: Some(remember),
                    always_rule: approval::propose_always_rule(tool_name, args),
                },
            );
            (true, None)
        }
        Answer::Deny { reason, remember } => {
            apply_remember(ctx, tool_name, args, remember, false);
            write_audit(
                log,
                tool_name,
                tier,
                args,
                step,
                ctx.mode,
                Decider::Human,
                Decided {
                    decision: Decision::Deny,
                    decider: Decider::Human,
                    reason: Some(reason.clone()),
                    remember: Some(remember),
                    always_rule: None,
                },
            );
            (false, Some(format!("用户拒绝：{reason}")))
        }
        // headless has no UI: Unavailable is a fail-closed denial; EscalateToHuman means no human card exists, also denied.
        Answer::EscalateToHuman | Answer::Unavailable => {
            write_audit(
                log,
                tool_name,
                tier,
                args,
                step,
                ctx.mode,
                Decider::HeadlessReject,
                Decided {
                    decision: Decision::RejectHeadless,
                    decider: Decider::HeadlessReject,
                    reason: Some("无可用审批 UI".into()),
                    remember: None,
                    always_rule: None,
                },
            );
            (false, Some("无可用审批 UI，fail-closed 拒绝".into()))
        }
    }
}

/// Apply remembered decision: the session tier goes into the session-level map; the always tier writes the confirmed
/// rule into project config (phase 1 writes back structurally without preserving comments; comment-preserving diffs are deferred to the phase-3 config-onboarding work).
fn apply_remember(
    ctx: &mut ChatCtx<'_>,
    tool_name: &str,
    args: &Value,
    remember: Remember,
    allowed: bool,
) {
    let command = args.get("command").and_then(Value::as_str);
    match remember {
        Remember::Once => {}
        Remember::Session => ctx.decisions.remember(tool_name, command, allowed),
        Remember::Always => {
            if allowed {
                if let Some(rule) = approval::propose_always_rule(tool_name, args) {
                    if let Err(e) = crate::config::write_always_rule(&rule, tool_name) {
                        eprintln!("[审批] always 规则写入配置失败：{e}；本会话内继续放行");
                    }
                }
            }
            // Session-level fallback: if the write-back fails or before a restart, similar calls this session no longer re-escalate.
            ctx.decisions.remember(tool_name, command, allowed);
        }
    }
}

fn session_summary(messages: &[Message]) -> String {
    let mut text = String::new();
    for m in messages.iter().rev() {
        let t = match m {
            Message::User(s) | Message::System(s) => s.clone(),
            Message::Assistant { content, .. } => content.clone(),
            Message::Tool { content, .. } => format!("[工具结果] {content}"),
        };
        if t.is_empty() {
            continue;
        }
        text = format!("{t}\n{text}");
        if text.chars().count() > 400 {
            break;
        }
    }
    text.chars().take(400).collect()
}

fn approved_rules_summary(ctx: &ChatCtx<'_>) -> String {
    if ctx.decisions.is_empty() {
        "（空）".to_string()
    } else {
        ctx.decisions.keys().collect::<Vec<_>>().join("、")
    }
}

fn write_audit(
    log: &mut SessionLog,
    tool: &str,
    tier: Tier,
    args: &Value,
    step: ChainStep,
    mode: Mode,
    asked_decider: Decider,
    decided: Decided,
) {
    let asked = Asked {
        tool: tool.to_string(),
        tier,
        args: args.clone(),
        step,
        mode,
        decider: asked_decider,
    };
    for (kind, data) in audit_pair(&asked, &decided) {
        log.log(&kind, data);
    }
}

/// Manual compaction ignores the auto-threshold switch and uses the same event-sourced
/// path as automatic compaction (session.zh.md §compaction).
pub async fn compact_now<P: LlmProvider>(
    provider: &mut P,
    messages: &mut Vec<Message>,
    log: &mut SessionLog,
    ui: &mut dyn UiSink,
    ctx: &ChatCtx<'_>,
) -> Result<(), String> {
    compact_messages(provider, messages, log, ui, ctx, "manual", None).await
}

/// Compaction threshold auto-track: when estimated context usage exceeds `compaction.autoThreshold`,
/// three events + surface replacement — the message list is reset to system + summary (session.zh.md §compaction).
async fn maybe_compact<P: LlmProvider>(
    provider: &mut P,
    messages: &mut Vec<Message>,
    log: &mut SessionLog,
    ui: &mut dyn UiSink,
    ctx: &ChatCtx<'_>,
) -> Result<(), String> {
    let Some(threshold) = ctx.config.compaction_auto_threshold else {
        return Ok(());
    };
    let chars: usize = messages.iter().map(message_len).sum();
    let ratio = chars as f64 / CONTEXT_CHARS;
    if ratio <= threshold {
        return Ok(());
    }
    compact_messages(
        provider,
        messages,
        log,
        ui,
        ctx,
        "auto-threshold",
        Some(ratio),
    )
    .await
}

async fn compact_messages<P: LlmProvider>(
    provider: &mut P,
    messages: &mut Vec<Message>,
    log: &mut SessionLog,
    ui: &mut dyn UiSink,
    ctx: &ChatCtx<'_>,
    trigger: &str,
    ratio: Option<f64>,
) -> Result<(), String> {
    let system = match messages.first() {
        Some(Message::System(s)) => s.clone(),
        _ => String::new(),
    };
    let _ = ctx.hooks.dispatch(HookEvent::PreCompact, None, "");
    log.log(
        "compaction/start",
        json!({ "trigger": trigger, "ratio": ratio }),
    );
    ui.on_status("压缩会话上下文");
    let transcript = messages
        .iter()
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n");
    match provider.complete(SUMMARY_SYSTEM, &transcript).await {
        Ok(summary) if !summary.trim().is_empty() => {
            if let Some(rt) = &ctx.goal {
                let est = (SUMMARY_SYSTEM.chars().count()
                    + transcript.chars().count()
                    + summary.chars().count()) as u64;
                rt.lock().charge_tokens(est);
            }
            log.log("compaction/summary", json!({ "text": summary.trim() }));
            messages.clear();
            if !system.is_empty() {
                messages.push(Message::System(system));
            }
            let surface = format!("（前文摘要）{}", summary.trim());
            log.log(
                "user/message",
                json!({ "content": surface, "source": "compaction" }),
            );
            messages.push(Message::User(surface));
            log.log("compaction/end", json!({ "error": null }));
            ui.on_status("空闲");
            Ok(())
        }
        Ok(_) => {
            log.log("compaction/end", json!({ "error": "空摘要" }));
            Ok(())
        }
        Err(e) => {
            log.log("compaction/end", json!({ "error": e }));
            Ok(())
        }
    }
}

fn message_len(m: &Message) -> usize {
    message_text(m).chars().count()
}

fn message_text(m: &Message) -> String {
    match m {
        Message::System(s) | Message::User(s) => s.clone(),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let mut t = content.clone();
            for tc in tool_calls {
                t.push_str(&format!(" [{}({})]", tc.name, tc.arguments));
            }
            t
        }
        Message::Tool { content, .. } => content.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::GoalRuntime;
    use crate::llm::Mock;
    use crate::tool::task::TaskStore;
    use parking_lot::Mutex;
    use std::sync::Arc;

    struct NoopUi;
    impl UiSink for NoopUi {
        fn on_status(&mut self, _status: &str) {}
        fn on_user(&mut self, _text: &str) {}
        fn on_delta(&mut self, _text: &str) {}
        fn on_assistant_done(&mut self, _content: &str, _tool_calls: &[ToolCall]) {}
        fn on_tool_call(&mut self, _call: &ToolCall) {}
        fn on_tool_result(&mut self, _id: &str, _output: &str) {}
    }

    fn fresh_ctx<'a>(
        cfg: &'a Config,
        hooks: &'a Hooks,
        goal: Option<Arc<Mutex<GoalRuntime>>>,
    ) -> ChatCtx<'a> {
        ChatCtx {
            config: cfg.clone(),
            hooks: hooks.clone(),
            mode: Mode::Yolo,
            decisions: Default::default(),
            reviewer: None,
            tasks: Arc::new(TaskStore::new()),
            edits: Arc::new(crate::tool::edit::EditSession::default()),
            goal,
            agents: Arc::new(crate::agent::AgentHost::new(
                Arc::new(cfg.clone()),
                Arc::new(|_| crate::llm::AnyProvider::Mock(crate::llm::Mock)),
            )),
            request_header_written: false,
            last_request_header: None,
            last_request_route: None,
            config_fingerprint: crate::config::config_fingerprint(),
            limits: Default::default(),
            lang: cfg.language,
        }
    }

    #[tokio::test]
    async fn 配置热加载更新规则hooks且不改变会话审批模式() {
        let base = Config::default();
        let base_hooks = Hooks::load(&base).unwrap();
        let mut ctx = fresh_ctx(&base, &base_hooks, None);
        ctx.mode = Mode::Ask;
        let loaded = Config::from_str_layers(
            "",
            "approval:\n  mode: yolo\ntools:\n  approval:\n    bash: deny\nautoContinue:\n  enabled: false\ntui:\n  language: en\nhooks:\n  UserPromptSubmit:\n    - block: 热加载阻断\n",
        )
        .unwrap();
        let loaded_hooks = Hooks::load(&loaded).unwrap();
        let mut provider = Mock;
        let mut ui = NoopUi;
        apply_reloaded_config(&mut provider, &mut ui, &mut ctx, loaded, loaded_hooks, 42)
            .await
            .unwrap();

        assert_eq!(ctx.mode, Mode::Ask, "审批模式只能由 approval/policy 切换");
        assert_eq!(
            ctx.config.rules.tool_entry("bash"),
            Some(crate::approval::ToolApproval::Deny)
        );
        assert_eq!(
            ctx.hooks
                .dispatch(HookEvent::UserPromptSubmit, None, "原输入"),
            HookOutcome::Blocked {
                reason: "热加载阻断".into()
            }
        );
        assert!(!ctx.limits.auto_continue);
        assert_eq!(ctx.lang, Lang::En);
        assert_eq!(ctx.config_fingerprint, 42);
    }

    fn fresh_log() -> (tempfile::TempDir, SessionLog) {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::create("goal-test", dir.path()).unwrap();
        (dir, log)
    }

    fn armed_runtime(objective: &str, max: Option<u64>) -> Arc<Mutex<GoalRuntime>> {
        let rt = Arc::new(Mutex::new(GoalRuntime::new(None)));
        rt.lock().begin_turn(true, false);
        let _ = rt.lock().tool_create(objective, max, None);
        rt
    }

    /// Table-driven proof of approval orthogonality (goal.zh.md §与审批的关系): identical
    /// gate() invocations under the goal-absent vs goal-active worlds produce byte-identical
    /// decisions and approval audit events — the decision chain takes no goal input at all.
    #[tokio::test]
    async fn goal活跃时审批决议链输出与无goal逐字节一致() {
        let cfg = Config::default();
        let hooks = Hooks::load(&cfg).unwrap();
        let cases: Vec<(&str, Tier, Value, Mode)> = vec![
            (
                "bash",
                Tier::Exec,
                json!({ "command": "echo hi" }),
                Mode::Yolo,
            ),
            (
                "write",
                Tier::Write,
                json!({ "path": "a.txt", "content": "x" }),
                Mode::Ask,
            ),
            (
                "bash",
                Tier::Exec,
                json!({ "command": "rm -rf /" }),
                Mode::Yolo,
            ),
            (
                "read",
                Tier::Read,
                json!({ "path": "Cargo.toml" }),
                Mode::Ask,
            ),
        ];
        for (tool, tier, args, mode) in cases {
            let mut traces: Vec<Vec<String>> = Vec::new();
            let mut outs: Vec<(bool, Option<String>)> = Vec::new();
            for goal in [None, Some(armed_runtime("审批正交性验证", None))] {
                let (_dir, mut log) = fresh_log();
                let mut ctx = fresh_ctx(&cfg, &hooks, goal);
                ctx.mode = mode;
                let mut ui = NoopUi;
                let (ok, deny) = gate(&mut ctx, &mut ui, &mut log, tool, tier, &args, &[]).await;
                let trace: Vec<String> = log
                    .read_all()
                    .unwrap()
                    .into_iter()
                    .filter(|e| e.kind.starts_with("approval/"))
                    .map(|e| format!("{}|{}", e.kind, serde_json::to_string(&e.data).unwrap()))
                    .collect();
                traces.push(trace);
                outs.push((ok, deny));
            }
            assert_eq!(outs[0], outs[1], "{tool}：gate 结果须一致");
            assert_eq!(traces[0], traces[1], "{tool}：审批审计事件须逐字节一致");
        }
    }

    /// Full-loop integration: an armed goal with maxGoalRounds=1 drives exactly one
    /// `<goal_round>` continuation turn, then the rounds budget hard-stops the driver
    /// while the goal stays active-but-stopped (goal.zh.md §阈值行为).
    #[tokio::test]
    async fn goal_armed驱动一轮后rounds预算硬停() {
        let cfg = Config::default();
        let hooks = Hooks::load(&cfg).unwrap();
        let rt = armed_runtime("完成演示任务", Some(1));
        let (_dir, mut log) = fresh_log();
        let mut ctx = fresh_ctx(&cfg, &hooks, Some(rt.clone()));
        let mut provider = Mock;
        let mut messages = vec![Message::System("系统".into())];
        let mut ui = NoopUi;
        send_user_message(
            &mut provider,
            &mut messages,
            &mut log,
            &mut ui,
            &mut ctx,
            1,
            "开始",
        )
        .await
        .unwrap();
        let events = log.read_all().unwrap();
        let rounds = events
            .iter()
            .filter(|e| {
                e.kind == "user/message"
                    && e.data.get("source").and_then(Value::as_str) == Some("goal-round")
            })
            .count();
        assert_eq!(rounds, 1, "max=1：恰好驱动一个 goal round 后硬停");
        let g = rt.lock();
        assert_eq!(g.state.get().unwrap().rounds_used, 1);
        assert_eq!(
            g.state.get().unwrap().status.as_str(),
            "active",
            "硬停后保持 active-but-stopped"
        );
    }

    /// disarm 后不驱动：a replayed goal (resume/fork 后) is disarmed by construction —
    /// visible and editable, but the driver never reserves a round until /goal resume.
    #[tokio::test]
    async fn disarm后不驱动任何round() {
        let cfg = Config::default();
        let hooks = Hooks::load(&cfg).unwrap();
        let rt = armed_runtime("跨进程目标", None);
        let events: Vec<crate::session::Event> = rt
            .lock()
            .drain_events()
            .into_iter()
            .map(|data| crate::session::Event {
                kind: "goal/change".into(),
                seq: 0,
                time: 0,
                data,
                ignorable: false,
            })
            .collect();
        let disarmed = Arc::new(Mutex::new(GoalRuntime::replay(&events, None)));
        let (_dir, mut log) = fresh_log();
        let mut ctx = fresh_ctx(&cfg, &hooks, Some(disarmed.clone()));
        let mut provider = Mock;
        let mut messages = vec![Message::System("系统".into())];
        let mut ui = NoopUi;
        send_user_message(
            &mut provider,
            &mut messages,
            &mut log,
            &mut ui,
            &mut ctx,
            1,
            "开始",
        )
        .await
        .unwrap();
        let events = log.read_all().unwrap();
        assert!(
            !events.iter().any(|e| e.kind == "user/message"
                && e.data.get("source").and_then(Value::as_str) == Some("goal-round")),
            "disarm 后不得驱动任何 goal round"
        );
        assert_eq!(disarmed.lock().state.get().unwrap().rounds_used, 0);
    }

    #[tokio::test]
    async fn 手动compact忽略自动阈值并写完整事件() {
        let mut cfg = Config::default();
        cfg.compaction_auto_threshold = None;
        let hooks = Hooks::load(&cfg).unwrap();
        let (_dir, mut log) = fresh_log();
        let ctx = fresh_ctx(&cfg, &hooks, None);
        let mut provider = Mock;
        let mut messages = vec![
            Message::System("系统".into()),
            Message::User("待压缩内容".into()),
        ];
        let mut ui = NoopUi;

        compact_now(&mut provider, &mut messages, &mut log, &mut ui, &ctx)
            .await
            .unwrap();

        let events = log.read_all().unwrap();
        let kinds: Vec<_> = events.iter().map(|event| event.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "compaction/start",
                "compaction/summary",
                "user/message",
                "compaction/end"
            ]
        );
        assert_eq!(messages.len(), 2, "模型上下文只保留 system 与摘要 surface");
    }

    // —— limit recovery (limits.zh.md) ——

    const QUOTA_ERR_NOW: &str = r#"DeepSeek API 429：{"error":{"type":"rate_limit_error","message":"Limit reached","reset":0}}"#;
    const QUOTA_ERR_SOON: &str = r#"DeepSeek API 429：{"error":{"type":"rate_limit_error","message":"Limit reached","reset":1}}"#;
    const QUOTA_NO_RESET: &str =
        r#"DeepSeek API 402：{"error":{"message":"Insufficient Balance"}}"#;
    const RATE_ERR: &str =
        r#"DeepSeek API 429：{"error":{"message":"Too Many Requests","type":"rate_limit"}}"#;

    /// Test-scale backoff: millisecond schedule so no test ever sleeps a real interval.
    fn fast_limits(auto: bool) -> crate::limits::LimitsCtl {
        crate::limits::LimitsCtl {
            auto_continue: auto,
            backoff: crate::limits::BackoffCfg {
                rate_base_ms: 1,
                rate_cap_ms: 2,
                rate_escalate_after: 5,
                ladder_ms: [3, 4, 5, 6],
                reset_margin_ms: 0,
                tick_ms: 1,
            },
            highlight_shown: false,
        }
    }

    /// Scripted suspension UI: pops one action per tick (empty = Wait forever); counts
    /// ticks (panel shown) and records auto-resume notes (highlight assertions).
    struct LimitUi {
        actions: std::collections::VecDeque<crate::limits::SuspendAction>,
        ticks: u32,
        notes: Vec<String>,
    }

    impl UiSink for LimitUi {
        fn on_status(&mut self, _status: &str) {}
        fn on_user(&mut self, _text: &str) {}
        fn on_delta(&mut self, _text: &str) {}
        fn on_assistant_done(&mut self, _content: &str, _tool_calls: &[ToolCall]) {}
        fn on_tool_call(&mut self, _call: &ToolCall) {}
        fn on_tool_result(&mut self, _id: &str, _output: &str) {}
        fn on_suspend_tick(
            &mut self,
            _info: &crate::limits::SuspendInfo,
        ) -> crate::limits::SuspendAction {
            self.ticks += 1;
            self.actions
                .pop_front()
                .unwrap_or(crate::limits::SuspendAction::Wait)
        }
        fn on_auto_resumed(&mut self, note: &str) {
            self.notes.push(note.to_string());
        }
    }

    /// A disarmed *active* goal (replay lineage — arming is process-local and never persists).
    fn disarmed_active_goal(objective: &str, max: Option<u64>) -> Arc<Mutex<GoalRuntime>> {
        let rt = armed_runtime(objective, max);
        let events: Vec<crate::session::Event> = rt
            .lock()
            .drain_events()
            .into_iter()
            .map(|data| crate::session::Event {
                kind: "goal/change".into(),
                seq: 0,
                time: 0,
                data,
                ignorable: false,
            })
            .collect();
        let disarmed = Arc::new(Mutex::new(GoalRuntime::replay(&events, None)));
        assert!(
            !disarmed.lock().state.is_armed(),
            "replay 构造天然 disarmed"
        );
        disarmed
    }

    #[tokio::test]
    async fn 限额挂起_自动恢复_重发不开新turn() {
        let cfg = Config::default();
        let hooks = Hooks::load(&cfg).unwrap();
        let (_dir, mut log) = fresh_log();
        let mut ctx = fresh_ctx(&cfg, &hooks, None);
        ctx.limits = fast_limits(true);
        let mut provider = crate::llm::MockQuota {
            script: vec![QUOTA_ERR_NOW.into()],
            requests: 0,
        };
        let mut messages = vec![Message::System("系统".into())];
        let mut ui = LimitUi {
            actions: Default::default(),
            ticks: 0,
            notes: vec![],
        };
        send_user_message(
            &mut provider,
            &mut messages,
            &mut log,
            &mut ui,
            &mut ctx,
            1,
            "开始",
        )
        .await
        .unwrap();
        let events = log.read_all().unwrap();
        assert_eq!(
            events.iter().filter(|e| e.kind == "turn/start").count(),
            1,
            "恢复即重发，不开新 turn"
        );
        assert_eq!(events.iter().filter(|e| e.kind == "turn/end").count(), 1);
        assert!(
            provider.requests >= 2,
            "自动探测重发了未完成请求（requests={}）",
            provider.requests
        );
        assert!(ui.ticks >= 1, "挂起面板已出现");
        assert_eq!(ui.notes.len(), 1, "首次自动恢复一次性高亮");
    }

    #[tokio::test]
    async fn 取消挂起_永久放弃不再探测() {
        let cfg = Config::default();
        let hooks = Hooks::load(&cfg).unwrap();
        let (_dir, mut log) = fresh_log();
        let mut ctx = fresh_ctx(&cfg, &hooks, None);
        ctx.limits = fast_limits(true);
        // reset:1 → 1s 后才到点；取消在第 3 tick 前触发，证明取消先于任何自动探测。
        let mut provider = crate::llm::MockQuota {
            script: vec![QUOTA_ERR_SOON.into()],
            requests: 0,
        };
        let mut messages = vec![Message::System("系统".into())];
        let mut ui = LimitUi {
            actions: [
                crate::limits::SuspendAction::Wait,
                crate::limits::SuspendAction::Wait,
                crate::limits::SuspendAction::Cancel,
            ]
            .into(),
            ticks: 0,
            notes: vec![],
        };
        let err = send_user_message(
            &mut provider,
            &mut messages,
            &mut log,
            &mut ui,
            &mut ctx,
            1,
            "开始",
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("已取消限额挂起"),
            "取消错误应说明语义，实际：{err}"
        );
        assert_eq!(provider.requests, 1, "取消后不再自动探测");
        assert!(
            log.read_all()
                .unwrap()
                .iter()
                .any(|e| e.kind == "turn/start"),
            "会话与记录保留"
        );
    }

    #[tokio::test]
    async fn autoContinue关闭_仅手动恢复() {
        let cfg = Config::default();
        let hooks = Hooks::load(&cfg).unwrap();
        // 无手动输入时：不得有任何自动探测（挂起循环永远等待）。
        {
            let (_dir, mut log) = fresh_log();
            let mut ctx = fresh_ctx(&cfg, &hooks, None);
            ctx.limits = fast_limits(false);
            let mut provider = crate::llm::MockQuota {
                script: vec![QUOTA_ERR_NOW.into()],
                requests: 0,
            };
            let mut messages = vec![Message::System("系统".into())];
            let mut ui = LimitUi {
                actions: Default::default(),
                ticks: 0,
                notes: vec![],
            };
            let r = tokio::time::timeout(
                std::time::Duration::from_millis(150),
                send_user_message(
                    &mut provider,
                    &mut messages,
                    &mut log,
                    &mut ui,
                    &mut ctx,
                    1,
                    "开始",
                ),
            )
            .await;
            assert!(r.is_err(), "autoContinue=false 时不得有自动探测");
            assert_eq!(provider.requests, 1, "重发仅可能来自手动");
        }
        // 手动「立即重试」恢复。
        {
            let (_dir, mut log) = fresh_log();
            let mut ctx = fresh_ctx(&cfg, &hooks, None);
            ctx.limits = fast_limits(false);
            let mut provider = crate::llm::MockQuota {
                script: vec![QUOTA_ERR_NOW.into()],
                requests: 0,
            };
            let mut messages = vec![Message::System("系统".into())];
            let mut ui = LimitUi {
                actions: [crate::limits::SuspendAction::RetryNow].into(),
                ticks: 0,
                notes: vec![],
            };
            send_user_message(
                &mut provider,
                &mut messages,
                &mut log,
                &mut ui,
                &mut ctx,
                1,
                "开始",
            )
            .await
            .unwrap();
            assert_eq!(provider.requests, 2, "手动重试恰好探测一次");
            assert!(ui.notes.is_empty(), "手动重试不触发自动恢复高亮");
        }
    }

    #[tokio::test]
    async fn 自动恢复顺带rearm_disarmed活跃goal() {
        let cfg = Config::default();
        let hooks = Hooks::load(&cfg).unwrap();
        let goal = disarmed_active_goal("跨窗口目标", Some(1));
        let (_dir, mut log) = fresh_log();
        let mut ctx = fresh_ctx(&cfg, &hooks, Some(goal.clone()));
        ctx.limits = fast_limits(true);
        let mut provider = crate::llm::MockQuota {
            script: vec![QUOTA_ERR_NOW.into()],
            requests: 0,
        };
        let mut messages = vec![Message::System("系统".into())];
        let mut ui = LimitUi {
            actions: Default::default(),
            ticks: 0,
            notes: vec![],
        };
        send_user_message(
            &mut provider,
            &mut messages,
            &mut log,
            &mut ui,
            &mut ctx,
            1,
            "开始",
        )
        .await
        .unwrap();
        assert!(
            goal.lock().state.is_armed(),
            "自动恢复顺带 rearm disarmed 活跃 goal"
        );
        let events = log.read_all().unwrap();
        let rearm: Vec<_> = events.iter().filter(|e| e.kind == "goal/rearm").collect();
        assert_eq!(rearm.len(), 1, "每次成功的带 goal 自动恢复写一条审计");
        assert_eq!(
            rearm[0].data.get("reason").and_then(Value::as_str),
            Some("auto-continue")
        );
        assert_eq!(
            rearm[0].data.get("objective").and_then(Value::as_str),
            Some("跨窗口目标")
        );
        assert_eq!(ui.notes.len(), 1, "高亮仅一次");
        assert!(
            ui.notes[0].contains("跨窗口目标"),
            "同一张卡同时呈现恢复与 rearm 的 goal"
        );
    }

    #[tokio::test]
    async fn 手动立即重试不rearm() {
        let cfg = Config::default();
        let hooks = Hooks::load(&cfg).unwrap();
        let goal = disarmed_active_goal("手动路径目标", Some(1));
        let (_dir, mut log) = fresh_log();
        let mut ctx = fresh_ctx(&cfg, &hooks, Some(goal.clone()));
        ctx.limits = fast_limits(true);
        let mut provider = crate::llm::MockQuota {
            script: vec![QUOTA_ERR_NOW.into()],
            requests: 0,
        };
        let mut messages = vec![Message::System("系统".into())];
        let mut ui = LimitUi {
            actions: [crate::limits::SuspendAction::RetryNow].into(),
            ticks: 0,
            notes: vec![],
        };
        send_user_message(
            &mut provider,
            &mut messages,
            &mut log,
            &mut ui,
            &mut ctx,
            1,
            "开始",
        )
        .await
        .unwrap();
        assert!(
            !goal.lock().state.is_armed(),
            "手动「立即重试」不 rearm（显式 rearm 走 /goal resume）"
        );
        assert!(
            !log.read_all()
                .unwrap()
                .iter()
                .any(|e| e.kind == "goal/rearm"),
            "手动路径不写 rearm 审计"
        );
    }

    #[tokio::test]
    async fn 首次自动恢复高亮仅一次() {
        let cfg = Config::default();
        let hooks = Hooks::load(&cfg).unwrap();
        let (_dir, mut log) = fresh_log();
        let mut ctx = fresh_ctx(&cfg, &hooks, None);
        ctx.limits = fast_limits(true);
        let mut messages = vec![Message::System("系统".into())];
        let mut ui = LimitUi {
            actions: Default::default(),
            ticks: 0,
            notes: vec![],
        };
        for turn in 1..=2u64 {
            let mut provider = crate::llm::MockQuota {
                script: vec![QUOTA_ERR_NOW.into()],
                requests: 0,
            };
            send_user_message(
                &mut provider,
                &mut messages,
                &mut log,
                &mut ui,
                &mut ctx,
                turn,
                "推进",
            )
            .await
            .unwrap();
        }
        assert_eq!(
            ui.notes.len(),
            1,
            "每会话首次自动恢复显示一次性高亮，后续静默续跑"
        );
    }

    #[tokio::test]
    async fn 速率类五连失败升级挂起面板接管() {
        let cfg = Config::default();
        let hooks = Hooks::load(&cfg).unwrap();
        let (_dir, mut log) = fresh_log();
        let mut ctx = fresh_ctx(&cfg, &hooks, None);
        ctx.limits = fast_limits(true);
        let mut provider = crate::llm::MockQuota {
            script: vec![RATE_ERR.into(); 5],
            requests: 0,
        };
        let mut messages = vec![Message::System("系统".into())];
        let mut ui = LimitUi {
            actions: [crate::limits::SuspendAction::RetryNow].into(),
            ticks: 0,
            notes: vec![],
        };
        send_user_message(
            &mut provider,
            &mut messages,
            &mut log,
            &mut ui,
            &mut ctx,
            1,
            "开始",
        )
        .await
        .unwrap();
        assert!(ui.ticks >= 1, "连续 5 次速率失败后挂起面板接管");
        assert_eq!(provider.requests, 6, "5 次失败 + 1 次挂起中手动探测成功");
        assert!(ui.notes.is_empty(), "手动重试不算自动恢复");
    }

    #[tokio::test]
    async fn 无法归类的错误绝不挂起() {
        let cfg = Config::default();
        let hooks = Hooks::load(&cfg).unwrap();
        let (_dir, mut log) = fresh_log();
        let mut ctx = fresh_ctx(&cfg, &hooks, None);
        ctx.limits = fast_limits(true);
        let mut provider = crate::llm::MockQuota {
            script: vec![r#"DeepSeek API 500：{"error":{"message":"Internal error"}}"#.into()],
            requests: 0,
        };
        let mut messages = vec![Message::System("系统".into())];
        let mut ui = LimitUi {
            actions: Default::default(),
            ticks: 0,
            notes: vec![],
        };
        let err = send_user_message(
            &mut provider,
            &mut messages,
            &mut log,
            &mut ui,
            &mut ctx,
            1,
            "开始",
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("Internal error"),
            "普通错误原样呈现，实际：{err}"
        );
        assert_eq!(ui.ticks, 0, "无法归类绝不挂起");
        assert_eq!(provider.requests, 1, "普通传输重试之外不自动重试");
    }

    #[tokio::test]
    async fn 探测错误重分类为普通错误退出挂起() {
        let cfg = Config::default();
        let hooks = Hooks::load(&cfg).unwrap();
        let (_dir, mut log) = fresh_log();
        let mut ctx = fresh_ctx(&cfg, &hooks, None);
        ctx.limits = fast_limits(true);
        let mut provider = crate::llm::MockQuota {
            script: vec![QUOTA_ERR_NOW.into(), r#"DeepSeek API 500：boom"#.into()],
            requests: 0,
        };
        let mut messages = vec![Message::System("系统".into())];
        let mut ui = LimitUi {
            actions: Default::default(),
            ticks: 0,
            notes: vec![],
        };
        let err = send_user_message(
            &mut provider,
            &mut messages,
            &mut log,
            &mut ui,
            &mut ctx,
            1,
            "开始",
        )
        .await
        .unwrap_err();
        assert!(err.contains("boom"), "重分类后的错误普通呈现，实际：{err}");
        assert_eq!(provider.requests, 2);
        assert!(ui.notes.is_empty(), "自动探测以失败告终不写 rearm、不高亮");
    }

    #[tokio::test]
    async fn 无reset限额_同窗口探测失败阶梯推进() {
        let cfg = Config::default();
        let hooks = Hooks::load(&cfg).unwrap();
        let (_dir, mut log) = fresh_log();
        let mut ctx = fresh_ctx(&cfg, &hooks, None);
        ctx.limits = fast_limits(true);
        // 同一错误体 = 同一窗口：探测失败后阶梯推进（3ms→4ms），不回到首档热循环。
        let mut provider = crate::llm::MockQuota {
            script: vec![QUOTA_NO_RESET.into(), QUOTA_NO_RESET.into()],
            requests: 0,
        };
        let mut messages = vec![Message::System("系统".into())];
        let mut ui = LimitUi {
            actions: Default::default(),
            ticks: 0,
            notes: vec![],
        };
        send_user_message(
            &mut provider,
            &mut messages,
            &mut log,
            &mut ui,
            &mut ctx,
            1,
            "开始",
        )
        .await
        .unwrap();
        assert_eq!(provider.requests, 3, "首错挂起 + 两次阶梯探测后成功");
        assert_eq!(ui.notes.len(), 1, "阶梯探测成功仍是自动恢复");
    }
}
