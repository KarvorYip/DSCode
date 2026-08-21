//! DSCode main entry: CLI parsing and assembly (config fail-loud / provider / approval / hooks / session).
//! Phase 1 subcommands: sessions (list sessions filtered by cwd), resume <id> (crash recovery + transcript/context rebuild),
//! fork <id> (fork via seed-prefix replay).

mod agent;
mod approval;
mod chat;
mod config;
mod goal;
mod headless;
mod hooks;
mod i18n;
mod limits;
mod llm;
mod session;
mod shell;
mod tool;
mod tui;

use approval::provider::{ApprovalProvider, AutoReviewer};
use approval::Mode;
use chat::ChatCtx;
use i18n::{tr, trf, Lang, StrKey};
use llm::{AnyProvider, Message, Mock};
use std::path::Path;

/// Goal policy appended to the system prompt when the goal stack is mounted (goal.zh.md
/// §长任务约束: purely prompt-level — the three codex points; no runtime self-checks).
const GOAL_POLICY_PROMPT: &str = "\n\ngoal 策略：goal 是跨 turn 的完成承诺，只有真正需要多轮推进的长任务才可用 create_goal 设置；例行多步骤工作请走 plan / 任务清单工具，不要为短任务建 goal；update_goal complete 必须指原始 objective，绝不把 objective 缩水成子集来冒充完成。";
const SYSTEM_PROMPT: &str = "你是 DSCode，一个终端里的 AI 编程助手。你可以调用 bash / read / write / edit / glob / grep 工具，并用 TaskCreate / TaskUpdate / TaskGet / TaskList 管理任务清单。请用中文回答。";

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("错误：{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(());
    }
    let mock = args.iter().any(|a| a == "--mock");
    let headless = args.iter().any(|a| a == "--headless");
    if !mock && !headless && config::needs_wizard() {
        config::run_wizard()?;
    }

    // Config fail-loud (config-onboarding.zh.md §报错两级): syntax/field errors exit with file:line.
    let cfg = config::Config::load()
        .map_err(|e| format!("{}{e}", tr(Lang::default(), StrKey::ConfigErrorPrefix)))?;
    let hooks = hooks::Hooks::load(&cfg)
        .map_err(|e| format!("{}{e}", tr(cfg.language, StrKey::HooksErrorPrefix)))?;
    let lang = cfg.language;

    let mut prompt = flag_value(&args, "--prompt");
    if prompt.is_none() {
        prompt = positional_prompt(&args);
    }

    // Approval mode: CLI override > config; auto without an approver configured effectively falls to yolo + one-time guidance (§2.1).
    let mut mode = cfg.approval_mode;
    if let Some(m) = flag_value(&args, "--approval-mode") {
        mode = parse_mode(&m, lang)?;
    }
    if args
        .iter()
        .any(|a| a == "--yolo" || a == "--dangerously-skip-permissions")
    {
        mode = Mode::Yolo;
    }
    let approver_ready = cfg.approver_configured();
    if mode == Mode::Auto && !approver_ready {
        mode = Mode::Yolo;
        eprintln!("{}", tr(lang, StrKey::YoloNotice));
    }

    let root = cfg.sessions_dir.clone();

    // Session source: subcommand (sessions/resume/fork) or create new.
    let mut messages: Vec<Message>;
    let mut log: session::SessionLog;
    // Subcommands may appear before or after flags: match by token; the argument is the token immediately following it.
    let cmd = args
        .iter()
        .find(|a| matches!(a.as_str(), "resume" | "fork" | "sessions"))
        .cloned();
    match cmd.as_deref() {
        Some("sessions") => {
            list_sessions(&root, lang)?;
            return Ok(());
        }
        Some("resume") => {
            let id = cmd_arg(&args, "resume")
                .ok_or_else(|| tr(lang, StrKey::UsageResume).to_string())?;
            log = session::SessionLog::open(&id, &root)?;
            // Restore the session-pinned approval mode: fold the last approval/policy event's `to` from the log (§2.8).
            if let Ok(events) = log.read_from(0) {
                if let Some(ev) = events.iter().rev().find(|e| e.kind == "approval/policy") {
                    if let Some(to) = ev.data.get("to").and_then(|v| v.as_str()) {
                        if let Ok(m) = parse_mode(to, lang) {
                            mode = m;
                        }
                    }
                }
            }
            messages = vec![Message::System(SYSTEM_PROMPT.into())];
            messages.extend(chat::rebuild_messages(&log.model_context()?));
        }
        Some("fork") => {
            let source =
                cmd_arg(&args, "fork").ok_or_else(|| tr(lang, StrKey::UsageFork).to_string())?;
            let new_id = chrono::Local::now().format("%Y%m%d-%H%M%S-%f").to_string();
            log = session::SessionLog::fork(&source, &new_id, &root)?;
            let (kind, data) = approval::policy_event(mode, mode, "session-fork", approver_ready);
            log.log(&kind, data);
            messages = vec![Message::System(SYSTEM_PROMPT.into())];
            messages.extend(chat::rebuild_messages(&log.model_context()?));
        }
        _ => {
            let session_id = chrono::Local::now().format("%Y%m%d-%H%M%S-%f").to_string();
            log = session::SessionLog::create(&session_id, &root)?;
            let (kind, data) = approval::policy_event(mode, mode, "session-create", approver_ready);
            log.log(&kind, data);
            messages = vec![Message::System(SYSTEM_PROMPT.into())];
        }
    }

    let skills = tool::skill::SkillCatalog::discover(Path::new("."));
    if !skills.is_empty() {
        if let Some(Message::System(system)) = messages.first_mut() {
            system.push_str(&skills.prompt_suffix());
        }
    }

    // Task state projection: fold task/write events back into the shared store
    // (resume/fork rebuild, session.zh.md replay; new sessions fold an empty log).
    let tasks = std::sync::Arc::new(tool::task::TaskStore::new());
    if let Ok(events) = log.read_all() {
        tasks
            .replay(&events)
            .map_err(|e| trf(lang, StrKey::TaskRestoreFailed, &[&e]))?;
    }

    // Goal stack mounting (goal.zh.md §启用默认): the interactive TUI mounts the full stack
    // (tools + driver) when goal.enabled; headless -p never mounts it in the first release.
    // resume/fork replay the latest goal/change snapshot — the rebuilt goal is disarmed by
    // construction (arming is process-local; only /goal resume re-arms).
    let goal_runtime: Option<std::sync::Arc<parking_lot::Mutex<goal::GoalRuntime>>> =
        if !headless && cfg.goal_enabled {
            let events = log.read_all().unwrap_or_default();
            Some(std::sync::Arc::new(parking_lot::Mutex::new(
                goal::GoalRuntime::replay(&events, cfg.goal_default_max_rounds),
            )))
        } else {
            None
        };
    // Pure-prompt long-task policy rides in the system prompt only when the stack is mounted.
    if goal_runtime.is_some() {
        if let Some(Message::System(s)) = messages.first_mut() {
            s.push_str(GOAL_POLICY_PROMPT);
        }
    }

    // Provider: routed by modelRoles.default; fixed to Mock in mock mode.
    let mut provider = if mock {
        AnyProvider::Mock(Mock)
    } else {
        AnyProvider::configured(&cfg, "default", "deepseek-v4-flash")
    };

    // Sub-agent host (tools.zh.md §3.8/§3.9): shared lifecycle registry + hub + async deliveries.
    // The factory gives each child its own provider; the hint is the definition's `model`
    // field, where a modelRoles role name expands to its concrete model (architecture.zh.md).
    let agent_host = {
        let provider_config = cfg.clone();
        let factory: std::sync::Arc<dyn Fn(Option<&str>) -> AnyProvider + Send + Sync> = if mock {
            std::sync::Arc::new(|_hint: Option<&str>| {
                AnyProvider::MockSubagent(llm::MockSubagent::default())
            })
        } else {
            std::sync::Arc::new(move |hint: Option<&str>| match hint {
                Some(role) if config::MODEL_ROLES.contains(&role) => {
                    AnyProvider::configured(&provider_config, role, "deepseek-v4-flash")
                }
                Some(model) => {
                    AnyProvider::configured_model(&provider_config, model, "agent model")
                }
                None => AnyProvider::configured(&provider_config, "default", "deepseek-v4-flash"),
            })
        };
        let host = std::sync::Arc::new(agent::AgentHost::new(
            std::sync::Arc::new(cfg.clone()),
            factory,
        ));
        if !mock {
            host.enable_mcp();
        }
        host.clone().start_sweeper();
        host
    };

    // Standalone approver instance (§2.3 stateless single request; explicitly configuring it same as default is allowed).
    let reviewer: Option<Box<dyn ApprovalProvider>> = match (approver_ready, mock) {
        (true, false) => Some(Box::new(AutoReviewer::new(AnyProvider::configured(
            &cfg,
            "approver",
            "deepseek-v4-flash",
        )))),
        (true, true) => Some(Box::new(AutoReviewer::new(Mock))),
        (false, _) => None,
    };

    let _ = hooks.dispatch(hooks::HookEvent::SessionStart, None, "");
    let result = {
        let mut ctx = ChatCtx {
            config: cfg.clone(),
            hooks: hooks.clone(),
            mode,
            decisions: Default::default(),
            reviewer: reviewer.as_deref(),
            tasks: tasks.clone(),
            edits: std::sync::Arc::new(tool::edit::EditSession::default()),
            goal: goal_runtime.clone(),
            agents: agent_host.clone(),
            limits: limits::LimitsCtl {
                auto_continue: cfg.auto_continue_enabled,
                ..Default::default()
            },
            lang,
            request_header_written: false,
            last_request_header: None,
            last_request_route: None,
            config_fingerprint: config::config_fingerprint(),
        };
        if headless {
            headless::run(&mut provider, &mut log, &mut ctx, &mut messages, prompt).await
        } else {
            let mut ui = tui::Tui::new(
                provider.model_name(),
                mode,
                approver_ready,
                tasks.clone(),
                lang,
                cfg.render_mode.clone(),
            )?;
            ui.run(&mut provider, &mut log, &mut ctx, &mut messages)
                .await
        }
    };
    let _ = hooks.dispatch(hooks::HookEvent::SessionEnd, None, "");
    result
}

fn parse_mode(s: &str, lang: Lang) -> Result<Mode, String> {
    match s {
        "ask" => Ok(Mode::Ask),
        "auto" => Ok(Mode::Auto),
        "yolo" => Ok(Mode::Yolo),
        _ => Err(trf(lang, StrKey::UnknownApprovalMode, &[&s])),
    }
}

/// List sessions for the current cwd (index filters by exact directory; session.zh.md §索引).
fn list_sessions(root: &Path, lang: Lang) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let cwd = cwd.display().to_string();
    let entries = session::index::list_by_cwd(root, &cwd)?;
    if entries.is_empty() {
        println!("{}", tr(lang, StrKey::NoSessions));
        return Ok(());
    }
    println!("{}", tr(lang, StrKey::SessionsHeader));
    for e in entries {
        let untitled = tr(lang, StrKey::UntitledSession);
        println!(
            "{}\t{}\t{}",
            e.id,
            e.created_at,
            e.title.unwrap_or_else(|| untitled.into())
        );
    }
    Ok(())
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn print_usage() {
    println!(
        "DSCode — 终端编码 Agent\n\
         \n用法：\n  \
         dscode [--mock] [--headless] [--prompt <文本>] [--approval-mode <ask|auto|yolo>|--yolo]\n  \
         dscode sessions                       列出当前目录的会话\n  \
         dscode resume <session-id>            恢复会话（crash recovery + 转录重建）\n  \
         dscode fork <session-id>              分叉会话（seed 前缀 replay）\n\
         \n选项：\n  \
         --mock        使用 Mock 提供方（脚本化多工具回路，自动验收用）\n  \
         --headless    无 TUI，stdout 输出\n  \
         --prompt      headless 模式的提示词；省略则从 stdin 读一行\n  \
         --approval-mode / --yolo    覆盖审批模式\n\
         \n按键（TUI）：Enter 发送 / Shift+Tab 循环审批模式 / Ctrl+C、Ctrl+D 退出；\n\
         审批卡键位：y=批准 s=批准(本会话) a=永远批准 n=拒绝。\n\
         配置：~/.dscode/config.yaml 与 .dscode/config.yaml（双层）；\n\
         凭据四层：env > ~/.dscode/.credentials.yaml > 项目 .env > ~/.dscode/.env"
    );
}

/// Subcommand argument: the first non-flag token immediately following the subcommand token.
fn cmd_arg(args: &[String], cmd: &str) -> Option<String> {
    args.iter()
        .position(|a| a == cmd)
        .and_then(|i| args.get(i + 1))
        .filter(|v| !v.starts_with("--"))
        .cloned()
}

/// Positional prompt: skip the values of valued flags (--prompt/--approval-mode), subcommands (resume/fork) and their arguments,
/// then take the first remaining non-flag token; falls back to stdin when absent.
fn positional_prompt(args: &[String]) -> Option<String> {
    let valued = ["--prompt", "--approval-mode"];
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if valued.contains(&a.as_str()) {
            skip_next = true;
            continue;
        }
        if matches!(a.as_str(), "resume" | "fork") {
            skip_next = true;
            continue;
        }
        if a == "sessions" || a.starts_with("--") {
            continue;
        }
        return Some(a.clone());
    }
    None
}
