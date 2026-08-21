//! TUI: ratatui inline viewport (no alt-screen) + crossterm event-stream + bracketed-paste.
//! Layout: chat stream (top, auto-scroll to bottom) + input box (hand-rolled single-line editor) + status line (model/approval mode/connection state).
//! Phase 1: resume transcript rebuilt via readFrom cursor, approval mode always visible (yolo highlighted), Shift+Tab cycling,
//! human decision card (synchronous keyboard polling: y/s/a/n).

use crate::approval::provider::{Answer, DecisionCard};
use crate::approval::{ChainStep, Mode, Remember};
use crate::chat::{compact_now, send_user_message, ChatCtx, UiSink};
use crate::config::RenderMode;
use crate::goal::GoalRuntime;
use crate::i18n::{tr, trf, Lang, StrKey};
use crate::llm::{LlmProvider, Message, ToolCall};
use crate::session::SessionLog;
use crate::tool::task::{TaskStatus, TaskStore};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use parking_lot::Mutex;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use serde_json::json;
use std::io::stdout;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct Tui {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    transcript: Vec<String>,
    /// Assistant message currently streaming (merged into transcript once complete)
    streaming: String,
    input: String,
    /// Input cursor (byte offset, always on a char boundary)
    cursor: usize,
    status: String,
    model: String,
    /// Approval mode label shown persistently in the status bar (§2.8).
    mode_label: String,
    /// Whether an approver is configured (Shift+Tab cycling skips auto when not).
    approver_ready: bool,
    /// Shared session task state; the panel renders the same projection the tools mutate.
    /// Goal runtime handle, taken from ChatCtx at run() start; None = stack not mounted.
    /// The status-bar badge and the /goal commands read this; chat.rs drives the rounds.
    goal: Option<Arc<Mutex<GoalRuntime>>>,
    tasks: Arc<TaskStore>,
    /// Active suspension view (limits.zh.md §TUI): panel data + open/closed; closing the
    /// panel keeps the status-bar signal (the suspension is not lost).
    suspend: Option<SuspendView>,
    /// Display language (tui.language): taken from config at construction; /language
    /// switches it live and mirrors the switch into ChatCtx.
    lang: Lang,
    render_mode: RenderMode,
    scroll_offset: usize,
    agents: Option<Arc<crate::agent::AgentHost>>,
    agent_panel: bool,
    tools_expanded: bool,
    thinking_expanded: bool,
    events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    event_stop: Arc<AtomicBool>,
    event_thread: Option<std::thread::JoinHandle<()>>,
}

/// Panel-side projection of one suspension (limits.zh.md §TUI 挂起面板).
struct SuspendView {
    info: crate::limits::SuspendInfo,
    panel_open: bool,
}

impl Tui {
    pub fn new(
        model: &str,
        mode: Mode,
        approver_ready: bool,
        tasks: Arc<TaskStore>,
        lang: Lang,
        render_mode: RenderMode,
    ) -> Result<Self, String> {
        enable_raw_mode().map_err(|e| e.to_string())?;
        let _ = execute!(stdout(), EnableBracketedPaste);
        let terminal = make_terminal(&render_mode)?;
        let (event_sender, events) = tokio::sync::mpsc::unbounded_channel();
        let event_stop = Arc::new(AtomicBool::new(false));
        let event_thread = Some(start_event_reader(event_sender, event_stop.clone()));
        Ok(Self {
            terminal,
            transcript: vec![tr(lang, StrKey::WelcomeHint).into()],
            streaming: String::new(),
            input: String::new(),
            cursor: 0,
            status: tr(lang, StrKey::StatusIdle).into(),
            model: model.to_string(),
            mode_label: mode.as_str().to_string(),
            approver_ready,
            goal: None,
            tasks,
            suspend: None,
            lang,
            render_mode,
            scroll_offset: 0,
            agents: None,
            agent_panel: false,
            tools_expanded: false,
            thinking_expanded: false,
            events,
            event_stop,
            event_thread,
        })
    }

    pub async fn run<P: LlmProvider>(
        &mut self,
        provider: &mut P,
        log: &mut SessionLog,
        ctx: &mut ChatCtx<'_>,
        messages: &mut Vec<Message>,
    ) -> Result<(), String> {
        // Resume transcript: rebuilt via the readFrom(0) cursor (session.zh.md acceptance 5).
        if let Ok(events) = log.read_from(0) {
            for ev in &events {
                if let Some(line) = transcript_line(&ev.kind, &ev.data, self.lang) {
                    self.transcript.push(line);
                }
            }
        }
        // Take the goal handle for the badge + /goal commands (chat.rs keeps driving the rounds).
        self.goal = ctx.goal.clone();
        let mut turn = messages
            .iter()
            .filter(|m| matches!(m, Message::User(_)))
            .count() as u64;
        self.agents = Some(ctx.agents.clone());
        loop {
            self.draw().map_err(|e| e.to_string())?;
            let Some(ev) = self.events.recv().await else {
                break;
            };
            match ev {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    if ctrl && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('d')) {
                        break;
                    }
                    if ctrl {
                        match k.code {
                            KeyCode::Char('a') => self.agent_panel = !self.agent_panel,
                            KeyCode::Char('t') => self.thinking_expanded = !self.thinking_expanded,
                            KeyCode::Char('o') => self.tools_expanded = !self.tools_expanded,
                            _ => {}
                        }
                        if matches!(k.code, KeyCode::Char('a' | 't' | 'o')) {
                            continue;
                        }
                    }
                    match k.code {
                        // Shift+Tab cycles ask → auto → yolo (skips auto when no approver is configured, §2.8);
                        // the switch is logged as approval/policy and takes effect on the next approval.
                        KeyCode::BackTab => {
                            let from = ctx.mode;
                            let to = next_mode(from, self.approver_ready);
                            if to != from {
                                let (kind, data) = crate::approval::policy_event(
                                    from,
                                    to,
                                    "shift-tab",
                                    self.approver_ready,
                                );
                                log.log(&kind, data);
                                ctx.mode = to;
                                self.mode_label = to.as_str().to_string();
                                self.push(trf(
                                    self.lang,
                                    StrKey::StatusModeSwitched,
                                    &[&from.as_str(), &to.as_str()],
                                ));
                            }
                        }
                        KeyCode::Enter if k.modifiers.contains(KeyModifiers::SHIFT) => {
                            self.insert("\n");
                        }
                        KeyCode::Enter => {
                            let text = self.input.trim().to_string();
                            if text.is_empty() {
                                continue;
                            }
                            self.input.clear();
                            self.cursor = 0;
                            if text == "/wizard" {
                                log.log("command/run", json!({ "command": "wizard", "args": "" }));
                                self.shutdown();
                                let result = crate::config::run_wizard();
                                enable_raw_mode().map_err(|error| error.to_string())?;
                                let _ = execute!(stdout(), EnableBracketedPaste);
                                self.terminal = make_terminal(&self.render_mode)?;
                                self.restart_event_reader();
                                match result {
                                    Ok(()) => match crate::config::Config::load() {
                                        Ok(config) => match self
                                            .apply_runtime_config(provider, ctx, config)
                                            .await
                                        {
                                            Ok(()) => self.push("wizard 配置已重新加载。".into()),
                                            Err(error) => {
                                                self.push(format!("wizard 后配置应用失败：{error}"))
                                            }
                                        },
                                        Err(error) => {
                                            self.push(format!("wizard 后配置加载失败：{error}"))
                                        }
                                    },
                                    Err(error) => self.push(format!("wizard 失败：{error}")),
                                }
                                log.log("command/done", json!({ "command": "wizard" }));
                                continue;
                            }
                            if text == "/settings" {
                                log.log(
                                    "command/run",
                                    json!({ "command": "settings", "args": "" }),
                                );
                                match crate::config::Config::load() {
                                    Ok(config) => match self
                                        .apply_runtime_config(provider, ctx, config)
                                        .await
                                    {
                                        Ok(()) => self.push(format!(
                                            "当前设置（来源优先级 project > global > default）\nprovider: {:?}\nmodelRoles: {:?}\napproval: {}\nautoContinue: {}\ntui: {:?}/{}\n编辑：~/.dscode/config.yaml 或 .dscode/config.yaml",
                                            ctx.config.providers.keys().collect::<Vec<_>>(),
                                            ctx.config.model_roles,
                                            ctx.config.approval_mode.as_str(),
                                            ctx.config.auto_continue_enabled,
                                            ctx.config.render_mode,
                                            ctx.config.language.as_str()
                                        )),
                                        Err(error) => {
                                            self.push(format!("配置热加载失败：{error}"))
                                        }
                                    },
                                    Err(error) => self.push(format!("配置热加载失败：{error}")),
                                }
                                log.log("command/done", json!({ "command": "settings" }));
                                continue;
                            }
                            if text == "/tui" || text.starts_with("/tui ") {
                                let value = text.strip_prefix("/tui").unwrap_or("").trim();
                                let mode = match value {
                                    "fullscreen" => Some(RenderMode::Fullscreen),
                                    "default" | "inline" => Some(RenderMode::Inline),
                                    _ => None,
                                };
                                if let Some(mode) = mode {
                                    self.set_render_mode(mode.clone())?;
                                    ctx.config.render_mode = mode.clone();
                                    let result = crate::config::write_render_mode_global(mode);
                                    self.push(match result {
                                        Ok(()) => "TUI 渲染模式已切换。".into(),
                                        Err(error) => format!("模式已切换，但写回失败：{error}"),
                                    });
                                } else {
                                    self.push("用法：/tui fullscreen|default".into());
                                }
                                continue;
                            }
                            if text == "/export" || text.starts_with("/export ") {
                                let value = text.strip_prefix("/export").unwrap_or("").trim();
                                let dir = if value.is_empty() {
                                    std::env::current_dir()
                                        .unwrap_or_else(|_| Path::new(".").to_path_buf())
                                        .join("dscode-export")
                                } else {
                                    Path::new(value).to_path_buf()
                                };
                                log.log(
                                    "command/run",
                                    json!({ "command": "export", "args": value }),
                                );
                                self.push(match log.export(&dir) {
                                    Ok((markdown, jsonl)) => format!(
                                        "会话已导出：{}；{}",
                                        markdown.display(),
                                        jsonl.display()
                                    ),
                                    Err(error) => format!("导出失败：{error}"),
                                });
                                log.log("command/done", json!({ "command": "export" }));
                                continue;
                            }
                            if let Some(path) = text.strip_prefix("@claude ") {
                                log.log(
                                    "command/run",
                                    json!({ "command": "import-claude", "args": path }),
                                );
                                match crate::session::import_claude(Path::new(path.trim())) {
                                    Ok(imported) => {
                                        let count = imported.len();
                                        for imported in imported {
                                            match imported {
                                                crate::session::ImportedMessage::User(content) => {
                                                    log.log("user/message", json!({ "content": &content, "source": "claude-import" }));
                                                    self.on_user(&content);
                                                    messages.push(Message::User(content));
                                                }
                                                crate::session::ImportedMessage::Assistant(
                                                    content,
                                                ) => {
                                                    log.log("assistant/message", json!({ "content": &content, "tool_calls": [] }));
                                                    self.on_assistant_done(&content, &[]);
                                                    messages.push(Message::Assistant {
                                                        content,
                                                        tool_calls: vec![],
                                                    });
                                                }
                                            }
                                        }
                                        self.push(format!(
                                            "已导入 {count} 条 Claude Code 消息，可继续对话。"
                                        ));
                                    }
                                    Err(error) => self.push(format!("导入失败：{error}")),
                                }
                                log.log("command/done", json!({ "command": "import-claude" }));
                                continue;
                            }
                            if text == "/sessions" || text.starts_with("/sessions ") {
                                let selection = text.strip_prefix("/sessions").unwrap_or("").trim();
                                let cwd = std::env::current_dir()
                                    .map(|path| path.display().to_string())
                                    .unwrap_or_default();
                                match crate::session::index::list_by_cwd(
                                    &ctx.config.sessions_dir,
                                    &cwd,
                                ) {
                                    Ok(entries) if selection.is_empty() => self.push(format!(
                                        "会话选择器（用 /sessions <序号或 id> 恢复）：\n{}",
                                        entries
                                            .iter()
                                            .enumerate()
                                            .map(|(index, entry)| format!(
                                                "{}. {}  {}",
                                                index + 1,
                                                entry.id,
                                                entry.title.as_deref().unwrap_or("（无标题）")
                                            ))
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    )),
                                    Ok(entries) => {
                                        let selected = selection
                                            .parse::<usize>()
                                            .ok()
                                            .and_then(|index| index.checked_sub(1))
                                            .and_then(|index| entries.get(index))
                                            .or_else(|| {
                                                entries.iter().find(|entry| entry.id == selection)
                                            })
                                            .map(|entry| entry.id.clone());
                                        match selected {
                                            Some(id) => match self.switch_session(
                                                &id, log, ctx, messages, &mut turn,
                                            ) {
                                                Ok(()) => {}
                                                Err(error) => {
                                                    self.push(format!("恢复会话失败：{error}"))
                                                }
                                            },
                                            None => self.push(format!(
                                                "未找到会话「{selection}」；先用 /sessions 查看列表。"
                                            )),
                                        }
                                    }
                                    Err(error) => self.push(format!("读取会话失败：{error}")),
                                }
                                continue;
                            }
                            if text == "/agents" {
                                self.agent_panel = !self.agent_panel;
                                continue;
                            }
                            if text == "/hotkeys" {
                                self.push("快捷键：Shift+Enter 换行；PageUp/PageDown 滚动；Ctrl+T thinking；Ctrl+O 工具输出；Ctrl+A Agent Hub；Shift+Tab 审批模式。".into());
                                continue;
                            }
                            if let Some(value) = text.strip_prefix("/approval-mode ") {
                                let to = match value.trim() {
                                    "ask" => Some(Mode::Ask),
                                    "auto" if self.approver_ready => Some(Mode::Auto),
                                    "yolo" => Some(Mode::Yolo),
                                    _ => None,
                                };
                                if let Some(to) = to {
                                    let from = ctx.mode;
                                    let (kind, data) = crate::approval::policy_event(
                                        from,
                                        to,
                                        "command",
                                        self.approver_ready,
                                    );
                                    log.log(&kind, data);
                                    ctx.mode = to;
                                    self.mode_label = to.as_str().into();
                                    self.push(format!("审批模式已切换：{}", to.as_str()));
                                } else {
                                    self.push(
                                        "审批模式不可用；用法：/approval-mode ask|auto|yolo".into(),
                                    );
                                }
                                continue;
                            }
                            // /goal slash command: six forms, user authority (no host proof —
                            // that gate is model-side only); runs while idle, never a model turn.
                            if text == "/goal" || text.starts_with("/goal ") {
                                self.handle_goal_command(&text, log);
                                continue;
                            }
                            // /language slash command: show the current language (no arg) or
                            // switch zh/en live; the switch persists to the global config layer.
                            if text == "/language" || text.starts_with("/language ") {
                                self.handle_language_command(&text, ctx, log);
                                continue;
                            }
                            if text == "/compact" {
                                log.log("command/run", json!({ "command": "compact", "args": "" }));
                                compact_now(provider, messages, log, self, ctx).await?;
                                log.log("command/done", json!({ "command": "compact" }));
                                self.push("会话上下文压缩完成。".into());
                                continue;
                            }
                            turn += 1;
                            send_user_message(provider, messages, log, self, ctx, turn, &text)
                                .await?;
                        }
                        KeyCode::Backspace => {
                            if self.cursor > 0 {
                                let prev = self.input[..self.cursor]
                                    .chars()
                                    .next_back()
                                    .unwrap()
                                    .len_utf8();
                                self.input.drain(self.cursor - prev..self.cursor);
                                self.cursor -= prev;
                            }
                        }
                        KeyCode::Delete => {
                            if self.cursor < self.input.len() {
                                let next =
                                    self.input[self.cursor..].chars().next().unwrap().len_utf8();
                                self.input.drain(self.cursor..self.cursor + next);
                            }
                        }
                        KeyCode::Left => {
                            if self.cursor > 0 {
                                self.cursor -= self.input[..self.cursor]
                                    .chars()
                                    .next_back()
                                    .unwrap()
                                    .len_utf8();
                            }
                        }
                        KeyCode::Right => {
                            if self.cursor < self.input.len() {
                                self.cursor +=
                                    self.input[self.cursor..].chars().next().unwrap().len_utf8();
                            }
                        }
                        KeyCode::PageUp => {
                            self.scroll_offset = self.scroll_offset.saturating_add(5);
                        }
                        KeyCode::PageDown => {
                            self.scroll_offset = self.scroll_offset.saturating_sub(5);
                        }
                        KeyCode::Home => self.cursor = 0,
                        KeyCode::End => self.cursor = self.input.len(),
                        KeyCode::Char(c) => self.insert(&c.to_string()),
                        _ => {}
                    }
                }
                Event::Paste(s) => self.insert(&s.replace('\r', "")),
                _ => {}
            }
        }
        self.shutdown();
        Ok(())
    }

    fn insert(&mut self, s: &str) {
        self.input.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn handle_stream_keys(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    match key.code {
                        KeyCode::Char('t') if ctrl => {
                            self.thinking_expanded = !self.thinking_expanded
                        }
                        KeyCode::Char('o') if ctrl => self.tools_expanded = !self.tools_expanded,
                        KeyCode::Char('a') if ctrl => self.agent_panel = !self.agent_panel,
                        KeyCode::PageUp => {
                            self.scroll_offset = self.scroll_offset.saturating_add(5)
                        }
                        KeyCode::PageDown => {
                            self.scroll_offset = self.scroll_offset.saturating_sub(5)
                        }
                        _ => self.buffer_event(Event::Key(key)),
                    }
                }
                other => self.buffer_event(other),
            }
        }
    }

    fn buffer_event(&mut self, event: Event) {
        apply_buffered_input(&mut self.input, &mut self.cursor, &event);
    }

    fn set_render_mode(&mut self, mode: RenderMode) -> Result<(), String> {
        if self.render_mode == mode {
            return Ok(());
        }
        let _ = self.terminal.clear();
        if self.render_mode == RenderMode::Fullscreen {
            let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        }
        self.terminal = make_terminal(&mode)?;
        self.render_mode = mode;
        self.scroll_offset = 0;
        Ok(())
    }

    async fn apply_runtime_config<P: LlmProvider>(
        &mut self,
        provider: &mut P,
        ctx: &mut ChatCtx<'_>,
        config: crate::config::Config,
    ) -> Result<(), String> {
        provider.reload_config(&config).await?;
        self.set_render_mode(config.render_mode.clone())?;
        self.model = provider.model_name().to_string();
        self.lang = config.language;
        self.approver_ready = ctx.reviewer.is_some();
        ctx.lang = config.language;
        ctx.limits.auto_continue = config.auto_continue_enabled;
        ctx.config_fingerprint = crate::config::config_fingerprint();
        ctx.config = config;
        Ok(())
    }

    fn switch_session(
        &mut self,
        id: &str,
        log: &mut SessionLog,
        ctx: &mut ChatCtx<'_>,
        messages: &mut Vec<Message>,
        turn: &mut u64,
    ) -> Result<(), String> {
        let next = SessionLog::open(id, &ctx.config.sessions_dir)?;
        let events = next.read_from(0)?;
        let context = next.model_context()?;
        let system = messages.iter().find_map(|message| match message {
            Message::System(text) => Some(Message::System(text.clone())),
            _ => None,
        });
        let mut rebuilt: Vec<Message> = system.into_iter().collect();
        rebuilt.extend(crate::chat::rebuild_messages(&context));
        let restored_mode = events
            .iter()
            .filter(|event| event.kind == "approval/policy")
            .filter_map(|event| event.data.get("to").and_then(serde_json::Value::as_str))
            .filter_map(Mode::parse)
            .next_back()
            .unwrap_or_else(|| ctx.config.effective_mode().mode);
        TaskStore::new().replay(&events)?;
        ctx.agents.reset_for_session_switch()?;
        ctx.tasks.replace_from_events(&events)?;
        if let Some(goal) = &ctx.goal {
            *goal.lock() =
                crate::goal::GoalRuntime::replay(&events, ctx.config.goal_default_max_rounds);
        }
        ctx.mode = restored_mode;
        ctx.decisions = Default::default();
        ctx.edits = Arc::new(crate::tool::edit::EditSession::default());
        ctx.limits = crate::limits::LimitsCtl {
            auto_continue: ctx.config.auto_continue_enabled,
            ..Default::default()
        };
        ctx.request_header_written = false;
        ctx.last_request_header = None;
        ctx.last_request_route = None;
        self.mode_label = restored_mode.as_str().to_string();
        self.transcript = vec![tr(self.lang, StrKey::WelcomeHint).into()];
        for event in &events {
            if let Some(line) = transcript_line(&event.kind, &event.data, self.lang) {
                self.transcript.push(line);
            }
        }
        self.streaming.clear();
        self.input.clear();
        self.cursor = 0;
        self.scroll_offset = 0;
        self.status = tr(self.lang, StrKey::StatusIdle).into();
        self.push(format!("已恢复会话 {id}，可继续对话。"));
        *turn = events
            .iter()
            .filter(|event| event.kind == "turn/start")
            .count() as u64;
        *messages = rebuilt;
        *log = next;
        Ok(())
    }

    fn push(&mut self, line: String) {
        self.transcript.push(line);
    }

    /// `/goal` six forms (goal.zh.md §用户命令与 TUI): show (default) / <objective> (create) /
    /// edit / pause / resume / clear. User authority — no host proof (model-side gate only);
    /// `resume` is the single explicit re-arm path. command/run + command/done are logged;
    /// goal/change events flush from the runtime queue after each mutation.
    fn handle_goal_command(&mut self, text: &str, log: &mut SessionLog) {
        let Some(rt) = self.goal.clone() else {
            self.push(tr(self.lang, StrKey::GoalNotEnabled).into());
            let _ = self.draw();
            return;
        };
        let rest = text.strip_prefix("/goal").unwrap_or("").trim();
        log.log("command/run", json!({ "command": "goal", "args": rest }));
        let reply: String = {
            let mut g = rt.lock();
            match rest {
                "" | "show" => match g.state.get() {
                    Some(goal) => trf(
                        self.lang,
                        StrKey::GoalShowCard,
                        &[
                            &goal.objective as &dyn std::fmt::Display,
                            &goal.status.as_str(),
                            &goal.revision,
                            &goal.rounds_used,
                            &goal
                                .max_goal_rounds
                                .map(|m| m.to_string())
                                .unwrap_or_else(|| tr(self.lang, StrKey::GoalUnlimited).into()),
                            &goal.tokens_used,
                            &goal
                                .token_budget
                                .map(|b| b.to_string())
                                .unwrap_or_else(|| tr(self.lang, StrKey::GoalUnlimited).into()),
                        ],
                    ),
                    None => tr(self.lang, StrKey::GoalNoneHint).into(),
                },
                "pause" => goal_reply(
                    &g.user_pause(),
                    tr(self.lang, StrKey::GoalPausedOk),
                    self.lang,
                ),
                "resume" => goal_reply(
                    &g.user_resume(),
                    tr(self.lang, StrKey::GoalResumedOk),
                    self.lang,
                ),
                "clear" => goal_reply(
                    &g.user_clear(),
                    tr(self.lang, StrKey::GoalClearedOk),
                    self.lang,
                ),
                _ if rest.starts_with("edit ") => goal_reply(
                    &g.user_edit(rest["edit ".len()..].trim()),
                    tr(self.lang, StrKey::GoalEditedOk),
                    self.lang,
                ),
                _ => goal_reply(
                    &g.user_create(rest),
                    tr(self.lang, StrKey::GoalCreatedOk),
                    self.lang,
                ),
            }
        };
        // Flush queued goal/change events; the create card gets its one-time highlight
        // via the transcript card renderer (action=create → ★).
        for data in rt.lock().drain_events() {
            log.log("goal/change", data);
        }
        log.log("command/done", json!({ "command": "goal" }));
        self.push(reply);
        let _ = self.draw();
    }

    /// `/language` (config-onboarding.zh.md §TUI 显示语言): no arg → current language +
    /// usage; zh/en → live switch. The switch mirrors into ChatCtx (chat.rs status strings),
    /// re-localizes the idle status so the status bar reflects it immediately, and persists
    /// to the GLOBAL config layer (language is a user preference, not a project attribute).
    /// A write-back failure surfaces loudly; the in-session switch stays.
    fn handle_language_command(&mut self, text: &str, ctx: &mut ChatCtx<'_>, log: &mut SessionLog) {
        let rest = text.strip_prefix("/language").unwrap_or("").trim();
        log.log(
            "command/run",
            json!({ "command": "language", "args": rest }),
        );
        let new_lang = match rest {
            "" => {
                self.push(trf(
                    self.lang,
                    StrKey::LanguageCurrent,
                    &[&self.lang.as_str()],
                ));
                log.log("command/done", json!({ "command": "language" }));
                let _ = self.draw();
                return;
            }
            "zh" => Lang::Zh,
            "en" => Lang::En,
            other => {
                self.push(trf(self.lang, StrKey::LanguageInvalid, &[&other]));
                log.log("command/done", json!({ "command": "language" }));
                let _ = self.draw();
                return;
            }
        };
        self.lang = new_lang;
        ctx.lang = new_lang;
        // Commands only run between turns, so idle is the live status here — re-localize it.
        self.status = tr(new_lang, StrKey::StatusIdle).into();
        let reply = match crate::config::write_language_global(new_lang) {
            Ok(()) => trf(new_lang, StrKey::LanguageSwitched, &[&new_lang.as_str()]),
            Err(e) => trf(
                new_lang,
                StrKey::LanguageWriteFailed,
                &[&new_lang.as_str() as &dyn std::fmt::Display, &e],
            ),
        };
        log.log("command/done", json!({ "command": "language" }));
        self.push(reply);
        let _ = self.draw();
    }

    fn draw(&mut self) -> std::io::Result<()> {
        let lang = self.lang;
        let model = self.model.clone();
        let status = self.status.clone();
        let mode = self.mode_label.clone();
        let input = self.input.clone();
        let cursor = self.cursor;
        let streaming = if self.thinking_expanded {
            self.streaming.clone()
        } else if self.streaming.is_empty() {
            String::new()
        } else {
            "[thinking/streaming，Ctrl+T 展开]".into()
        };
        let transcript: Vec<String> = self
            .transcript
            .iter()
            .map(|line| {
                if !self.tools_expanded && line.starts_with("← ") {
                    let head: String = line.chars().take(80).collect();
                    format!("{head}… [Ctrl+O 展开]")
                } else {
                    line.clone()
                }
            })
            .collect();
        let tasks_snapshot = self.tasks.list();
        let agent_roster = self
            .agents
            .as_ref()
            .filter(|_| self.agent_panel)
            .map(|host| host.roster());
        let goal_badge = self
            .goal
            .as_ref()
            .map(|rt| rt.lock().badge(self.lang))
            .flatten();
        let suspend_panel = self.suspend.as_ref().filter(|s| s.panel_open).map(|s| {
            (
                s.info.reason.clone(),
                crate::limits::fmt_countdown(s.info.next_probe_at, crate::limits::now_ms()),
                s.info.reset_at.is_some(),
            )
        });
        self.terminal.draw(|f| {
            let task_rows: u16 = if tasks_snapshot.is_empty() {
                0
            } else {
                tasks_snapshot.len().min(6) as u16 + 2
            };
            let agent_rows = agent_roster
                .as_ref()
                .map(|roster| {
                    roster["agents"].as_array().map_or(0, Vec::len)
                        + roster["jobs"].as_array().map_or(0, Vec::len)
                })
                .unwrap_or(0)
                .min(6) as u16;
            let input_rows = input.lines().count().clamp(1, 6) as u16 + 2;
            let chunks = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(task_rows),
                Constraint::Length(if agent_rows == 0 { 0 } else { agent_rows + 2 }),
                Constraint::Length(input_rows),
                Constraint::Length(1),
            ])
            .split(f.area());

            // Chat stream: estimate the total wrapped line count and scroll to the bottom
            let area = chunks[0];
            let w = area.width.saturating_sub(2).max(1) as usize;
            let h = area.height.saturating_sub(2).max(1) as usize;
            let mut lines: Vec<Line> = Vec::new();
            // Suspend panel (limits.zh.md §TUI): reason + countdown + shortcuts; closable
            // without losing the signal (the status bar keeps the suspension state).
            if let Some((reason, cd, has_reset)) = &suspend_panel {
                lines.push(Line::from(Span::styled(
                    tr(lang, StrKey::SuspendPanelTitle),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                )));
                lines.push(Line::from(trf(
                    lang,
                    StrKey::SuspendPanelReason,
                    &[&reason],
                )));
                if *has_reset {
                    lines.push(Line::from(trf(
                        lang,
                        StrKey::SuspendPanelReset,
                        &[&&model, &cd],
                    )));
                } else {
                    lines.push(Line::from(trf(
                        lang,
                        StrKey::SuspendPanelProbe,
                        &[&&model, &cd],
                    )));
                }
                lines.push(Line::from(tr(lang, StrKey::SuspendPanelKeys)));
                lines.push(Line::from("╚════════════════════════"));
            }
            for text in &transcript {
                push_text_lines(&mut lines, text, None);
            }
            if !streaming.is_empty() {
                let rendered = trf(lang, StrKey::AssistantLine, &[&streaming]);
                push_text_lines(
                    &mut lines,
                    &rendered,
                    Some(Style::default().fg(Color::Cyan)),
                );
            }
            let para = Paragraph::new(lines).wrap(Wrap { trim: false });
            let total = para.line_count(w as u16);
            let scroll = total.saturating_sub(h).saturating_sub(self.scroll_offset) as u16;
            let para = para
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(tr(lang, StrKey::PanelChatTitle)),
                )
                .scroll((scroll, 0));
            f.render_widget(para, area);

            // Task panel: status icon + title, in_progress highlighted.
            if task_rows > 0 {
                let task_lines: Vec<Line> = tasks_snapshot
                    .iter()
                    .map(|task| {
                        let text = format!("{} #{} {}", task.status.icon(), task.id, task.title);
                        if task.status == TaskStatus::InProgress {
                            Line::from(Span::styled(
                                text,
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(ratatui::style::Modifier::BOLD),
                            ))
                        } else {
                            Line::from(text)
                        }
                    })
                    .collect();
                f.render_widget(
                    Paragraph::new(task_lines).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(tr(lang, StrKey::PanelTasksTitle)),
                    ),
                    chunks[1],
                );
            }

            if let Some(roster) = &agent_roster {
                let mut agent_lines = Vec::new();
                if let Some(agents) = roster["agents"].as_array() {
                    for agent in agents {
                        agent_lines.push(Line::from(format!(
                            "{} [{}] mailbox:{}",
                            agent["id"].as_str().unwrap_or("?"),
                            agent["state"].as_str().unwrap_or("?"),
                            agent["mailbox"].as_u64().unwrap_or(0)
                        )));
                    }
                }
                if let Some(jobs) = roster["jobs"].as_array() {
                    for job in jobs {
                        agent_lines.push(Line::from(format!(
                            "{} [{}] {}",
                            job["id"].as_str().unwrap_or("?"),
                            job["state"].as_str().unwrap_or("?"),
                            job["label"].as_str().unwrap_or("")
                        )));
                    }
                }
                f.render_widget(
                    Paragraph::new(agent_lines)
                        .block(Block::default().borders(Borders::ALL).title("Agent Hub")),
                    chunks[2],
                );
            }

            // Input box: one rendered Line per logical line; the highlighted span is the cursor.
            f.render_widget(
                Paragraph::new(input_lines_with_cursor(&input, cursor)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(tr(lang, StrKey::PanelInputTitle)),
                ),
                chunks[3],
            );

            // Status line: model | approval mode (yolo highlighted) | goal badge | status
            // (goal.zh.md §用户命令与 TUI: objective truncated + round N/M + blocked reason + budget awareness)
            let mode_span = if mode == "yolo" {
                Span::styled(
                    trf(lang, StrKey::ModeLabel, &[&mode]),
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                )
            } else {
                Span::raw(trf(lang, StrKey::ModeLabel, &[&mode]))
            };
            let mut status_spans = vec![Span::raw(format!(" {model} | ")), mode_span];
            if let Some(badge) = &goal_badge {
                status_spans.push(Span::raw(" | "));
                status_spans.push(Span::styled(
                    badge.clone(),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ));
            }
            status_spans.push(Span::raw(format!(" | {status}")));
            let status_line = Line::from(status_spans);
            f.render_widget(Paragraph::new(status_line), chunks[4]);
        })?;
        Ok(())
    }

    fn restart_event_reader(&mut self) {
        let (sender, events) = tokio::sync::mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        self.events = events;
        self.event_stop = stop.clone();
        self.event_thread = Some(start_event_reader(sender, stop));
    }

    fn shutdown(&mut self) {
        self.event_stop.store(true, Ordering::Release);
        if let Some(thread) = self.event_thread.take() {
            let _ = thread.join();
        }
        let _ = disable_raw_mode();
        if self.render_mode == RenderMode::Fullscreen {
            let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        }
        let _ = execute!(self.terminal.backend_mut(), DisableBracketedPaste);
        let _ = self.terminal.show_cursor();
    }
}

fn start_event_reader(
    sender: tokio::sync::mpsc::UnboundedSender<Event>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            match crossterm::event::poll(std::time::Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(event) => {
                        if sender.send(event).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    })
}

fn make_terminal(mode: &RenderMode) -> Result<Terminal<CrosstermBackend<std::io::Stdout>>, String> {
    let viewport = match mode {
        RenderMode::Fullscreen => {
            execute!(stdout(), EnterAlternateScreen).map_err(|error| error.to_string())?;
            Viewport::Fullscreen
        }
        RenderMode::Inline => {
            let rows = crossterm::terminal::size()
                .map(|(_, height)| height.saturating_sub(1))
                .unwrap_or(20)
                .min(20);
            Viewport::Inline(rows)
        }
    };
    Terminal::with_options(
        CrosstermBackend::new(stdout()),
        TerminalOptions { viewport },
    )
    .map_err(|error| error.to_string())
}

/// User-side goal command reply: success → fixed message + objective; failure → the JSON error message.
fn goal_reply(v: &serde_json::Value, ok_msg: &str, lang: Lang) -> String {
    if v.get("ok") == Some(&serde_json::json!(false)) {
        let msg = v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or(tr(lang, StrKey::GoalOpFailedDefault));
        trf(lang, StrKey::GoalOpFailed, &[&msg])
    } else {
        match v
            .get("objective")
            .and_then(|o| o.as_str())
            .filter(|o| !o.is_empty())
        {
            Some(obj) => format!("{ok_msg}{}{obj}", tr(lang, StrKey::OkColon)),
            None => ok_msg.to_string(),
        }
    }
}

/// Event → transcript line (resume rebuild; compaction boundaries shown as separators).
fn transcript_line(kind: &str, data: &serde_json::Value, lang: Lang) -> Option<String> {
    match kind {
        "user/message" => data
            .get("content")
            .and_then(|t| t.as_str())
            .map(|t| trf(lang, StrKey::UserLine, &[&t])),
        "assistant/message" => data
            .get("content")
            .and_then(|c| c.as_str())
            .map(|c| trf(lang, StrKey::AssistantLine, &[&c])),
        "goal/change" => {
            // Generic event card for every goal transition; the create action gets the
            // one-time ★ highlight (self-set goals must not slip past the user, goal.zh.md).
            let action = data.get("action").and_then(|a| a.as_str()).unwrap_or("?");
            let objective = data
                .get("objective")
                .and_then(|o| o.as_str())
                .unwrap_or("?");
            let status = data.get("status").and_then(|s| s.as_str()).unwrap_or("?");
            if action == "create" {
                Some(trf(lang, StrKey::GoalCreatedCard, &[&objective]))
            } else {
                Some(trf(
                    lang,
                    StrKey::GoalChangedCard,
                    &[&action, &status, &objective],
                ))
            }
        }
        "tool/call" => {
            let name = data.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            Some(trf(lang, StrKey::TranscriptToolCall, &[&name]))
        }
        "tool/result" => {
            let out = data.get("output").and_then(|o| o.as_str()).unwrap_or("");
            let head: String = out.chars().take(60).collect();
            Some(trf(lang, StrKey::TranscriptToolResult, &[&head]))
        }
        "approval/decided" => {
            let tool = data.get("tool").and_then(|t| t.as_str()).unwrap_or("?");
            let decision = data.get("decision").and_then(|d| d.as_str()).unwrap_or("?");
            Some(trf(lang, StrKey::TranscriptApproval, &[&tool, &decision]))
        }
        "compaction/summary" => Some(tr(lang, StrKey::CompactionSep).into()),
        _ => None,
    }
}
fn next_mode(from: Mode, approver_ready: bool) -> Mode {
    match from {
        Mode::Ask if approver_ready => Mode::Auto,
        Mode::Ask => Mode::Yolo,
        Mode::Auto => Mode::Yolo,
        Mode::Yolo => Mode::Ask,
    }
}

fn apply_buffered_input(input: &mut String, cursor: &mut usize, event: &Event) -> bool {
    match event {
        Event::Paste(text) => {
            let text = text.replace('\r', "");
            input.insert_str(*cursor, &text);
            *cursor += text.len();
            true
        }
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    input.insert(*cursor, '\n');
                    *cursor += 1;
                    true
                }
                KeyCode::Backspace if *cursor > 0 => {
                    let width = input[..*cursor]
                        .chars()
                        .next_back()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                    input.drain(*cursor - width..*cursor);
                    *cursor -= width;
                    true
                }
                KeyCode::Delete if *cursor < input.len() => {
                    let width = input[*cursor..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                    input.drain(*cursor..*cursor + width);
                    true
                }
                KeyCode::Left if *cursor > 0 => {
                    *cursor -= input[..*cursor]
                        .chars()
                        .next_back()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                    true
                }
                KeyCode::Right if *cursor < input.len() => {
                    *cursor += input[*cursor..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                    true
                }
                KeyCode::Home => {
                    *cursor = 0;
                    true
                }
                KeyCode::End => {
                    *cursor = input.len();
                    true
                }
                KeyCode::Char(character) if !ctrl => {
                    input.insert(*cursor, character);
                    *cursor += character.len_utf8();
                    true
                }
                _ => false,
            }
        }
        _ => false,
    }
}
fn push_text_lines(lines: &mut Vec<Line<'static>>, text: &str, style: Option<Style>) {
    for part in text.split('\n') {
        let span = match style {
            Some(style) => Span::styled(part.to_string(), style),
            None => Span::raw(part.to_string()),
        };
        lines.push(Line::from(span));
    }
}

fn input_lines_with_cursor(input: &str, cursor: usize) -> Vec<Line<'static>> {
    let (before, at, after) = split_at_cursor(input, cursor);
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    append_plain_input(&mut rows, &before);
    let cursor_style = Style::default().bg(Color::Yellow).fg(Color::Black);
    if at == "\n" {
        rows.last_mut()
            .expect("输入至少有一行")
            .push(Span::styled(" ", cursor_style));
        rows.push(Vec::new());
    } else {
        rows.last_mut()
            .expect("输入至少有一行")
            .push(Span::styled(at, cursor_style));
    }
    append_plain_input(&mut rows, &after);
    rows.into_iter().map(Line::from).collect()
}

fn append_plain_input(rows: &mut Vec<Vec<Span<'static>>>, text: &str) {
    for (index, part) in text.split('\n').enumerate() {
        if index > 0 {
            rows.push(Vec::new());
        }
        if !part.is_empty() {
            rows.last_mut()
                .expect("输入至少有一行")
                .push(Span::raw(part.to_string()));
        }
    }
}

fn split_at_cursor(input: &str, cursor: usize) -> (String, String, String) {
    let before = input[..cursor].to_string();
    let mut rest = input[cursor..].chars();
    match rest.next() {
        Some(c) => {
            let at = c.to_string();
            let after = rest.collect();
            (before, at, after)
        }
        None => (before, " ".into(), String::new()),
    }
}

impl UiSink for Tui {
    fn on_status(&mut self, status: &str) {
        self.status = status.to_string();
        let _ = self.draw();
    }

    /// One suspension tick (limits.zh.md §TUI): render the panel + sync the status bar,
    /// then poll keys (approval-card precedent: synchronous polling during an await).
    fn on_suspend_tick(
        &mut self,
        info: &crate::limits::SuspendInfo,
    ) -> crate::limits::SuspendAction {
        let panel_open = self.suspend.as_ref().map_or(true, |s| s.panel_open);
        self.suspend = Some(SuspendView {
            info: info.clone(),
            panel_open,
        });
        let cd = crate::limits::fmt_countdown(info.next_probe_at, crate::limits::now_ms());
        self.status = if info.reset_at.is_some() {
            trf(self.lang, StrKey::StatusSuspendReset, &[&self.model, &cd])
        } else {
            trf(self.lang, StrKey::StatusSuspendProbe, &[&self.model, &cd])
        };
        let _ = self.draw();
        if let Ok(event) = self.events.try_recv() {
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        return crate::limits::SuspendAction::RetryNow
                    }
                    KeyCode::Char('s') | KeyCode::Char('S') => {
                        return crate::limits::SuspendAction::ReloadProvider
                    }
                    KeyCode::Char('c') | KeyCode::Char('C') => {
                        return crate::limits::SuspendAction::Cancel
                    }
                    KeyCode::Char('p') | KeyCode::Char('P') | KeyCode::Esc => {
                        if let Some(suspend) = &mut self.suspend {
                            suspend.panel_open = !suspend.panel_open;
                        }
                    }
                    _ => self.buffer_event(Event::Key(key)),
                },
                other => self.buffer_event(other),
            }
        }
        crate::limits::SuspendAction::Wait
    }

    fn on_suspend_end(&mut self) {
        self.suspend = None;
        let _ = self.draw();
    }

    /// Auto-continue resume notice (limits.zh.md): highlight card; the note may carry the
    /// re-armed goal — one card presents both.
    fn on_auto_resumed(&mut self, note: &str) {
        self.push(format!("★ {note}"));
        let _ = self.draw();
    }

    fn on_config_reloaded(&mut self, model: &str, approver_ready: bool, lang: Lang) {
        self.model = model.to_string();
        self.approver_ready = approver_ready;
        self.lang = lang;
        self.push("检测到配置文件变更，已自动热加载。".into());
        let _ = self.draw();
    }

    fn on_user(&mut self, text: &str) {
        self.push(trf(self.lang, StrKey::UserLine, &[&text]));
        let _ = self.draw();
    }

    fn on_delta(&mut self, text: &str) {
        self.streaming.push_str(text);
        self.handle_stream_keys();
        let _ = self.draw();
    }

    fn on_assistant_done(&mut self, content: &str, tool_calls: &[ToolCall]) {
        self.streaming.clear();
        self.push(trf(self.lang, StrKey::AssistantLine, &[&content]));
        for tc in tool_calls {
            self.push(format!("  [tool_call] {}({})", tc.name, tc.arguments));
        }
        let _ = self.draw();
    }

    fn on_tool_call(&mut self, call: &ToolCall) {
        self.push(trf(
            self.lang,
            StrKey::ToolCallLine,
            &[&call.name, &call.arguments],
        ));
        let _ = self.draw();
    }

    fn on_tool_result(&mut self, _id: &str, output: &str) {
        self.push(trf(self.lang, StrKey::ToolResultLine, &[&output]));
        let _ = self.draw();
    }

    /// Human decision card (§2.11): the card renders the escalation step, reviewer reason, and always-tier proposal;
    /// keyboard-first answer: y=approve(once) s=approve(this session) a=always approve n=deny(once) d=deny(this session).
    /// critical pattern is final — no approve option exists on the card (§2.4/§2.11);
    /// timeout returns Unavailable (fail-closed deny, §2.6).
    fn on_approval_card(&mut self, card: DecisionCard) -> Answer {
        let critical = card.step == ChainStep::CriticalPattern;
        self.push(trf(
            self.lang,
            StrKey::ApprovalCardTitle,
            &[&card.tool, &format!("{:?}", card.tier)],
        ));
        self.push(trf(self.lang, StrKey::ApprovalCardArgs, &[&card.args]));
        self.push(trf(
            self.lang,
            StrKey::ApprovalCardStep,
            &[&card.step.as_str()],
        ));
        if let Some(r) = &card.reviewer_reason {
            self.push(trf(self.lang, StrKey::ApprovalCardReviewer, &[&r]));
        }
        if critical {
            self.push(tr(self.lang, StrKey::ApprovalCriticalHit).into());
        } else if let Some(p) = &card.always_proposal {
            self.push(trf(self.lang, StrKey::ApprovalAlwaysProposal, &[&p]));
        }
        let keys = if critical {
            tr(self.lang, StrKey::ApprovalKeysCritical)
        } else {
            tr(self.lang, StrKey::ApprovalKeysFull)
        };
        self.push(trf(self.lang, StrKey::ApprovalKeysHint, &[&keys]));
        let _ = self.draw();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
        loop {
            if std::time::Instant::now() >= deadline {
                self.push(tr(self.lang, StrKey::ApprovalTimeoutNotice).into());
                return Answer::Unavailable;
            }
            let event = match self.events.try_recv() {
                Ok(event) => event,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    continue;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return Answer::Unavailable;
                }
            };
            let Event::Key(key) = event else {
                self.buffer_event(event);
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')) {
                self.shutdown();
                std::process::exit(0);
            }
            match key.code {
                KeyCode::Char('y') if !critical => {
                    self.push(tr(self.lang, StrKey::ApprovedOnce).into());
                    return Answer::Approve {
                        remember: Remember::Once,
                    };
                }
                KeyCode::Char('s') if !critical => {
                    self.push(tr(self.lang, StrKey::ApprovedSession).into());
                    return Answer::Approve {
                        remember: Remember::Session,
                    };
                }
                KeyCode::Char('a') if !critical => {
                    self.push(tr(self.lang, StrKey::ApprovedAlways).into());
                    return Answer::Approve {
                        remember: Remember::Always,
                    };
                }
                KeyCode::Char('n') => {
                    self.push(tr(self.lang, StrKey::DeniedOnce).into());
                    return Answer::Deny {
                        reason: "用户在决定卡上拒绝".into(),
                        remember: Remember::Once,
                    };
                }
                KeyCode::Char('d') => {
                    self.push(tr(self.lang, StrKey::DeniedSession).into());
                    return Answer::Deny {
                        reason: "用户在决定卡上拒绝（本会话记住）".into(),
                        remember: Remember::Session,
                    };
                }
                _ => self.buffer_event(Event::Key(key)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn 流式期间键入与粘贴保留在输入缓冲() {
        let mut input = "前".to_string();
        let mut cursor = input.len();
        assert!(apply_buffered_input(
            &mut input,
            &mut cursor,
            &Event::Key(KeyEvent::new(KeyCode::Char('中'), KeyModifiers::NONE))
        ));
        assert!(apply_buffered_input(
            &mut input,
            &mut cursor,
            &Event::Paste("文\r\n第二行".into())
        ));
        assert_eq!(input, "前中文\n第二行");
        assert_eq!(cursor, input.len());
    }
    #[test]
    fn 多行聊天与cjk输入渲染保持逻辑行() {
        let mut chat = Vec::new();
        push_text_lines(&mut chat, "首行\n第二行", None);
        assert_eq!(chat.len(), 2);
        assert_eq!(chat[0].spans[0].content.as_ref(), "首行");
        assert_eq!(chat[1].spans[0].content.as_ref(), "第二行");

        let content = |line: &Line<'_>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let cursor_after_newline = "前\n".len();
        let rows = input_lines_with_cursor("前\n中文", cursor_after_newline);
        assert_eq!(rows.len(), 2);
        assert_eq!(content(&rows[0]), "前");
        assert_eq!(content(&rows[1]), "中文");
        assert_eq!(rows[1].spans[0].style.bg, Some(Color::Yellow));

        let rows = input_lines_with_cursor("前\n中文", "前".len());
        assert_eq!(rows.len(), 2);
        assert_eq!(content(&rows[0]), "前 ");
        assert_eq!(content(&rows[1]), "中文");
        assert_eq!(rows[0].spans[1].style.bg, Some(Color::Yellow));
    }
}
