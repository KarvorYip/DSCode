//! Sub-agent subsystem (tools.zh.md §3.8/§3.9): AgentDefinition frontmatter subset, discovery
//! (project `.dscode/agents` > user `~/.dscode/agents` > bundled, first-wins on name), lifecycle
//! registry (running/idle/parked/aborted; idle TTL 7min → parked; a message revives), and the
//! shared `AgentHost`: roster + mailbox (cap 100) + jobs + async deliveries + `agent/*` events.
//! Coordination waits are poll-based (25ms tick) — uniform for messages/jobs/processes;
//! ample for a coding-agent coordinator.
//!
//! Locking: std Mutex held only across synchronous critical sections (never across .await);
//! all acquisitions go through the `lock()` helper (dependency freeze keeps us off parking_lot).

pub mod isolation;
pub mod proc;
pub mod runner;
pub mod schema;

use crate::config::Config;
use crate::llm::AnyProvider;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Idle TTL before an agent is auto-parked (tools.zh.md §3.8).
pub const IDLE_TTL: Duration = Duration::from_secs(7 * 60);
/// Mailbox hard cap (tools.zh.md §3.9 messaging).
pub const MAILBOX_CAP: usize = 100;
/// Coordination poll tick for waits.
pub const POLL_TICK: Duration = Duration::from_millis(25);

// ---- Agent definitions ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefSource {
    Bundled,
    User,
    Project,
}

/// AgentDefinition frontmatter subset (tools.zh.md §3.8).
#[derive(Clone, Debug)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    /// None = inherit the full child tool set (minus stripped spawn).
    pub tools: Option<Vec<String>>,
    /// Whether this agent type may dispatch children at all.
    pub spawns: bool,
    /// modelRoles role or concrete model id.
    pub model: Option<String>,
    /// Output instructions (advisor: output never enters the main conversation flow).
    pub output: Option<String>,
    pub source: DefSource,
}

/// Bundled first-release trio (tools.zh.md §3.8): scout (read-only research), task (general
/// worker), advisor (observer; its output stays in `agent://` artifacts, ticket 004 rev 6).
pub fn bundled() -> Vec<AgentDefinition> {
    vec![
        AgentDefinition {
            name: "scout".into(),
            description: "只读调研代理：探查代码库并回答问题，不修改任何文件".into(),
            system_prompt: "你是只读调研代理（scout）。你只做调研：用 read/glob/grep 收集证据，\
绝不创建、修改或删除任何文件，不执行有副作用的命令。产出简明、有出处的调研结论。"
                .into(),
            tools: Some(vec!["read".into(), "glob".into(), "grep".into()]),
            spawns: false,
            model: None,
            output: Some("简明调研结论，附关键文件与行号出处".into()),
            source: DefSource::Bundled,
        },
        AgentDefinition {
            name: "task".into(),
            description: "通用 worker：可使用全部工具完成分配的实现/修改任务".into(),
            system_prompt: "你是通用任务代理（task）。独立完成分配的任务：可以先调研再动手，\
完成后用 yield 提交结果与关键变更说明。"
                .into(),
            tools: None,
            spawns: true,
            model: None,
            output: None,
            source: DefSource::Bundled,
        },
        AgentDefinition {
            name: "advisor".into(),
            description: "观察者代理：审阅并给出意见；输出不进入主对话流，仅供 agent:// 产物查阅"
                .into(),
            system_prompt: "你是观察者代理（advisor）。你审阅上下文并给出专业意见；\
你不修改文件。你的输出不会回流主对话，只落在 agent:// 产物中供事后查阅，因此意见要完整、自成一体。"
                .into(),
            tools: Some(vec!["read".into(), "glob".into(), "grep".into()]),
            spawns: false,
            model: None,
            output: Some("完整、自成一体的审阅意见（不回注主对话）".into()),
            source: DefSource::Bundled,
        },
    ]
}

#[derive(Default, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    spawns: Option<bool>,
    model: Option<String>,
    output: Option<String>,
}

/// Parse an agent markdown file: YAML frontmatter between `---` fences, body = system prompt
/// (an explicit frontmatter `systemPrompt` wins over the body).
pub fn parse_frontmatter(text: &str, source: DefSource) -> Result<AgentDefinition, String> {
    let rest = text
        .strip_prefix("---")
        .ok_or("缺少 frontmatter 起始 ---")?;
    let (fm, body) = rest
        .split_once("\n---")
        .ok_or("缺少 frontmatter 结束 ---")?;
    let fm: Frontmatter =
        serde_yaml::from_str(fm).map_err(|e| format!("frontmatter 解析失败：{e}"))?;
    let name = fm.name.ok_or("缺少必填字段 name")?;
    Ok(AgentDefinition {
        description: fm.description.unwrap_or_default(),
        system_prompt: fm
            .system_prompt
            .unwrap_or_else(|| body.trim_start_matches('\n').trim().to_string()),
        tools: fm.tools,
        spawns: fm.spawns.unwrap_or(true),
        model: fm.model,
        output: fm.output,
        name,
        source,
    })
}

/// User-level agents dir: `~/.dscode/agents`.
fn user_agents_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".dscode").join("agents"))
        .unwrap_or_else(|| PathBuf::from(".dscode/agents"))
}

/// Discovery order: project `.dscode/agents` > user `~/.dscode/agents` > bundled;
/// same name — first source wins.
pub fn discover(project_root: &std::path::Path) -> Vec<AgentDefinition> {
    let mut defs: Vec<AgentDefinition> = Vec::new();
    let layers = [
        (
            project_root.join(".dscode").join("agents"),
            DefSource::Project,
        ),
        (user_agents_dir(), DefSource::User),
    ];
    for (dir, source) in layers {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .collect();
        for f in files {
            if let Ok(text) = std::fs::read_to_string(&f) {
                match parse_frontmatter(&text, source) {
                    Ok(def) => {
                        if !defs.iter().any(|d| d.name == def.name) {
                            defs.push(def);
                        }
                    }
                    Err(e) => eprintln!("[agent] 跳过无法解析的定义 {}：{e}", f.display()),
                }
            }
        }
    }
    for b in bundled() {
        if !defs.iter().any(|d| d.name == b.name) {
            defs.push(b);
        }
    }
    defs
}

// ---- Lifecycle registry + hub core ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Running,
    Idle,
    Parked,
    Aborted,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Running => "running",
            AgentState::Idle => "idle",
            AgentState::Parked => "parked",
            AgentState::Aborted => "aborted",
        }
    }
}

#[derive(Clone, Debug)]
pub struct HubMessage {
    pub from: String,
    pub text: String,
    pub time: u64,
}

struct AgentEntry {
    def_name: String,
    state: AgentState,
    last_active: Instant,
    mailbox: VecDeque<HubMessage>,
    transcript: Vec<String>,
}

/// send() receipt (tools.zh.md §3.9): injected (target running) / woken (idle → running) /
/// revived (parked|aborted → running) / failed (unknown target or mailbox full).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendReceipt {
    Injected,
    Woken,
    Revived,
    Failed(String),
}

impl SendReceipt {
    pub fn as_str(&self) -> &'static str {
        match self {
            SendReceipt::Injected => "injected",
            SendReceipt::Woken => "woken",
            SendReceipt::Revived => "revived",
            SendReceipt::Failed(_) => "failed",
        }
    }
}

pub enum WaitOutcome {
    Message(HubMessage),
    Timeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Completed => "completed",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }
}

struct JobEntry {
    label: String,
    state: JobState,
    result: Option<String>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

struct HostInner {
    agents: BTreeMap<String, AgentEntry>,
    jobs: BTreeMap<String, JobEntry>,
    /// Completed async sub-agent results awaiting injection into the conversation flow.
    pending: Vec<(String, String)>,
    /// `agent/*` events awaiting pickup by the turn loop (single-writer stays in the main loop).
    events: Vec<(String, Value)>,
    counter: u32,
}

/// Shared host: constructed once in main, threaded to tools via ToolCtx and to runners via Arc.
pub struct AgentHost {
    inner: Mutex<HostInner>,
    /// Hub process registry (tools.zh.md §3.9); separate table from the agent roster.
    procs: Mutex<BTreeMap<String, proc::ProcEntry>>,
    pub(crate) config: Arc<Config>,
    provider_factory: Arc<dyn Fn(Option<&str>) -> AnyProvider + Send + Sync>,
    pub(crate) shared_tools: Arc<crate::tool::SharedTools>,
    mcp_fingerprint: Mutex<Option<u64>>,
    mcp_refresh: tokio::sync::Mutex<()>,
    project_root: PathBuf,
    mcp_enabled: AtomicBool,
}

impl AgentHost {
    pub fn new(
        config: Arc<Config>,
        provider_factory: Arc<dyn Fn(Option<&str>) -> AnyProvider + Send + Sync>,
    ) -> Self {
        Self {
            inner: Mutex::new(HostInner {
                // The main conversation is itself addressable on the hub (id "Main").
                agents: BTreeMap::from([(
                    "Main".to_string(),
                    AgentEntry {
                        def_name: "main".to_string(),
                        state: AgentState::Idle,
                        last_active: Instant::now(),
                        mailbox: VecDeque::new(),
                        transcript: Vec::new(),
                    },
                )]),
                jobs: BTreeMap::new(),
                pending: Vec::new(),
                events: Vec::new(),
                counter: 0,
            }),
            procs: Mutex::new(BTreeMap::new()),
            config,
            provider_factory,
            shared_tools: Arc::new(crate::tool::SharedTools::default()),
            mcp_fingerprint: Mutex::new(None),
            mcp_refresh: tokio::sync::Mutex::new(()),
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            mcp_enabled: AtomicBool::new(false),
        }
    }

    pub fn config(&self) -> Arc<Config> {
        self.config.clone()
    }

    pub fn shared_tools(&self) -> Arc<crate::tool::SharedTools> {
        self.shared_tools.clone()
    }

    pub fn enable_mcp(&self) {
        self.mcp_enabled.store(true, Ordering::Release);
    }

    pub async fn refresh_mcp(&self) -> Result<Option<Vec<String>>, String> {
        if !self.mcp_enabled.load(Ordering::Acquire) {
            return Ok(None);
        }
        let fingerprint = crate::tool::mcp::config_fingerprint(&self.project_root);
        if *self
            .mcp_fingerprint
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            == Some(fingerprint)
        {
            return Ok(None);
        }
        let _refresh = self.mcp_refresh.lock().await;
        let fingerprint = crate::tool::mcp::config_fingerprint(&self.project_root);
        if *self
            .mcp_fingerprint
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            == Some(fingerprint)
        {
            return Ok(None);
        }
        let tools = crate::tool::mcp::discover_tools(&self.project_root).await?;
        let names = self.shared_tools.replace_boxed(tools);
        *self
            .mcp_fingerprint
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(fingerprint);
        Ok(Some(names))
    }

    /// Per-child provider. The hint is the definition's `model` field: a modelRoles role name
    /// expands to its concrete model at dispatch (architecture.zh.md role resolution).
    pub fn make_provider(&self, model_hint: Option<&str>) -> AnyProvider {
        (self.provider_factory)(model_hint)
    }

    pub fn reset_for_session_switch(&self) -> Result<(), String> {
        {
            let inner = self.lock();
            if inner
                .agents
                .iter()
                .any(|(id, agent)| id != "Main" && agent.state == AgentState::Running)
                || inner
                    .jobs
                    .values()
                    .any(|job| job.state == JobState::Running)
            {
                return Err("仍有运行中的 agent/job；请等待完成或取消后再切换会话".into());
            }
        }
        {
            let processes = self.procs.lock().unwrap_or_else(|error| error.into_inner());
            if processes
                .values()
                .any(|process| !matches!(process.status, proc::ProcStatus::Exited(_)))
            {
                return Err("仍有运行中的 Hub process；请 stop 后再切换会话".into());
            }
        }

        *self.lock() = HostInner {
            agents: BTreeMap::from([(
                "Main".to_string(),
                AgentEntry {
                    def_name: "main".to_string(),
                    state: AgentState::Idle,
                    last_active: Instant::now(),
                    mailbox: VecDeque::new(),
                    transcript: Vec::new(),
                },
            )]),
            jobs: BTreeMap::new(),
            pending: Vec::new(),
            events: Vec::new(),
            counter: 0,
        };
        self.procs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        Ok(())
    }

    /// All lock acquisitions go through here: poisoning is impossible on these paths (no panic
    /// while held); recovered via into_inner instead of unwrap.
    fn lock(&self) -> MutexGuard<'_, HostInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Artifacts root: sibling of the sessions dir (`~/.dscode/artifacts` by default, tools.zh.md §3.8).
    pub fn artifacts_dir(&self) -> PathBuf {
        let sessions = &self.config.sessions_dir;
        sessions
            .parent()
            .map(|p| p.join("artifacts"))
            .unwrap_or_else(|| sessions.join("artifacts"))
    }

    /// Register a new agent (state Running); id = `<Def><n>` (e.g. Scout1).
    pub fn register_agent(&self, def_name: &str) -> String {
        let mut inner = self.lock();
        inner.counter += 1;
        let mut cap = def_name.chars();
        let id = match cap.next() {
            Some(c) => format!("{}{}{}", c.to_uppercase(), cap.as_str(), inner.counter),
            None => format!("Agent{}", inner.counter),
        };
        inner.agents.insert(
            id.clone(),
            AgentEntry {
                def_name: def_name.to_string(),
                state: AgentState::Running,
                last_active: Instant::now(),
                mailbox: VecDeque::new(),
                transcript: Vec::new(),
            },
        );
        id
    }

    /// Transition on completion: ok → idle, failure → aborted.
    pub fn complete_agent(&self, id: &str, ok: bool) {
        let mut inner = self.lock();
        if let Some(a) = inner.agents.get_mut(id) {
            a.state = if ok {
                AgentState::Idle
            } else {
                AgentState::Aborted
            };
            a.last_active = Instant::now();
        }
    }

    /// Mark the agent active (message send/receipt, waits).
    fn touch(inner: &mut HostInner, id: &str) {
        if let Some(a) = inner.agents.get_mut(id) {
            a.last_active = Instant::now();
        }
    }

    // -- events / deliveries --

    pub fn push_event(&self, kind: &str, data: Value) {
        self.lock().events.push((kind.to_string(), data));
    }

    /// Drain queued `agent/*` events (the turn loop writes them into the session log).
    pub fn drain_events(&self) -> Vec<(String, Value)> {
        std::mem::take(&mut self.lock().events)
    }

    /// Queue a completed async result for injection into the conversation flow.
    pub fn push_pending(&self, id: &str, text: String) {
        self.lock().pending.push((id.to_string(), text));
    }

    pub fn take_pending(&self) -> Vec<(String, String)> {
        std::mem::take(&mut self.lock().pending)
    }

    // -- transcript (history://<id> read-only handle) --

    pub fn transcript_push(&self, id: &str, line: &str) {
        let mut inner = self.lock();
        Self::touch(&mut inner, id);
        if let Some(a) = inner.agents.get_mut(id) {
            a.transcript.push(line.to_string());
            // ponytail: unbounded transcript would leak memory on long agents; 2000 lines is plenty for review
            if a.transcript.len() > 2000 {
                let drop = a.transcript.len() - 2000;
                a.transcript.drain(..drop);
            }
        }
    }

    /// Render the in-memory transcript for `history://<id>` (read-only).
    pub fn history_text(&self, id: &str) -> Result<String, String> {
        let inner = self.lock();
        let a = inner
            .agents
            .get(id)
            .ok_or_else(|| format!("未知 agent：{id}（history:// 句柄不存在）"))?;
        Ok(if a.transcript.is_empty() {
            format!("[{id} 转录为空]")
        } else {
            a.transcript.join("\n")
        })
    }

    // -- messaging (tools.zh.md §3.9) --

    /// Fire-and-forget send with a receipt; a message to an idle/parked/aborted agent revives it.
    pub fn send(&self, from: &str, to: &str, text: &str) -> SendReceipt {
        let receipt = {
            let mut inner = self.lock();
            Self::touch(&mut inner, from);
            let Some(a) = inner.agents.get_mut(to) else {
                return SendReceipt::Failed(format!("目标 agent 不存在：{to}"));
            };
            if a.mailbox.len() >= MAILBOX_CAP {
                return SendReceipt::Failed(format!(
                    "目标 mailbox 已满（上限 {MAILBOX_CAP}）：{to}"
                ));
            }
            let prev = a.state;
            a.state = AgentState::Running; // message = the revive primitive
            a.last_active = Instant::now();
            a.mailbox.push_back(HubMessage {
                from: from.to_string(),
                text: text.to_string(),
                time: now_millis(),
            });
            match prev {
                AgentState::Running => SendReceipt::Injected,
                AgentState::Idle => SendReceipt::Woken,
                AgentState::Parked | AgentState::Aborted => SendReceipt::Revived,
            }
        };
        self.push_event(
            "agent/message",
            json!({ "from": from, "to": to, "text": truncate_chars(text, 200) }),
        );
        receipt
    }

    pub fn broadcast(&self, from: &str, text: &str) -> Vec<(String, SendReceipt)> {
        let targets: Vec<String> = {
            let inner = self.lock();
            inner
                .agents
                .keys()
                .filter(|k| *k != from)
                .cloned()
                .collect()
        };
        targets
            .into_iter()
            .map(|to| {
                let r = self.send(from, &to, text);
                (to, r)
            })
            .collect()
    }

    /// Wait for the next inbox message, optionally filtered by sender; timeout is a normal outcome.
    pub async fn wait_inbox(
        &self,
        who: &str,
        from: Option<&str>,
        timeout: Duration,
    ) -> WaitOutcome {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut inner = self.lock();
                Self::touch(&mut inner, who);
                if let Some(a) = inner.agents.get_mut(who) {
                    if let Some(pos) = a
                        .mailbox
                        .iter()
                        .position(|m| from.is_none_or(|f| &m.from == f))
                    {
                        let msg = a.mailbox.remove(pos).unwrap();
                        return WaitOutcome::Message(msg);
                    }
                }
            }
            if Instant::now() >= deadline {
                return WaitOutcome::Timeout;
            }
            tokio::time::sleep(POLL_TICK).await;
        }
    }

    /// One-question-one-answer: send, then wait for the next reply from the target.
    pub async fn await_reply(
        &self,
        from: &str,
        to: &str,
        text: &str,
        timeout: Duration,
    ) -> Result<Value, String> {
        match self.send(from, to, text) {
            SendReceipt::Failed(e) => Err(e),
            receipt => match self.wait_inbox(from, Some(to), timeout).await {
                WaitOutcome::Message(m) => Ok(json!({
                    "receipt": receipt.as_str(),
                    "from": m.from,
                    "text": m.text,
                })),
                WaitOutcome::Timeout => Ok(json!({
                    "receipt": receipt.as_str(),
                    "timeout": true,
                })),
            },
        }
    }

    /// Peek the inbox without consuming.
    pub fn inbox_peek(&self, who: &str) -> Vec<HubMessage> {
        let mut inner = self.lock();
        Self::touch(&mut inner, who);
        inner
            .agents
            .get(who)
            .map(|a| a.mailbox.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn roster(&self) -> Value {
        let inner = self.lock();
        let agents: Vec<Value> = inner
            .agents
            .iter()
            .map(|(id, a)| {
                json!({
                    "id": id,
                    "agent": a.def_name,
                    "state": a.state.as_str(),
                    "idleSecs": a.last_active.elapsed().as_secs(),
                    "mailbox": a.mailbox.len(),
                })
            })
            .collect();
        let jobs: Vec<Value> = inner
            .jobs
            .iter()
            .map(|(id, j)| {
                json!({
                    "id": id,
                    "label": j.label,
                    "state": j.state.as_str(),
                    "result": j.result,
                })
            })
            .collect();
        json!({ "agents": agents, "jobs": jobs })
    }

    // -- jobs --

    /// Reserve a job id (state Running, no handle yet).
    pub fn job_begin(&self, label: &str) -> String {
        let mut inner = self.lock();
        inner.counter += 1;
        let id = format!("job-{}", inner.counter);
        inner.jobs.insert(
            id.clone(),
            JobEntry {
                label: label.to_string(),
                state: JobState::Running,
                result: None,
                handle: None,
            },
        );
        id
    }

    pub fn job_set_handle(&self, id: &str, handle: tokio::task::JoinHandle<()>) {
        if let Some(j) = self.lock().jobs.get_mut(id) {
            j.handle = Some(handle);
        }
    }

    pub fn job_end(&self, id: &str, ok: bool, result: String) {
        if let Some(j) = self.lock().jobs.get_mut(id) {
            if j.state == JobState::Running {
                j.state = if ok {
                    JobState::Completed
                } else {
                    JobState::Failed
                };
                j.result = Some(result);
                j.handle = None;
            }
        }
    }

    /// Cancel by ids: abort the task (if running) and mark cancelled.
    pub fn job_cancel(&self, ids: &[String]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut inner = self.lock();
        for id in ids {
            match inner.jobs.get_mut(id) {
                None => out.push((id.clone(), "未知 job".into())),
                Some(j) => {
                    if j.state == JobState::Running {
                        if let Some(h) = j.handle.take() {
                            h.abort();
                        }
                        j.state = JobState::Cancelled;
                        out.push((id.clone(), "已取消".into()));
                    } else {
                        out.push((
                            id.clone(),
                            format!("已结束（{}），无需取消", j.state.as_str()),
                        ));
                    }
                }
            }
        }
        out
    }

    /// Unified four-way wait (tools.zh.md §3.9 jobs): first of {watched job settled, new message,
    /// window elapsed} wins; an abort is observed as the watched job settling into `cancelled`.
    /// Timeout is a normal outcome, not an error.
    pub async fn job_wait(&self, who: &str, ids: &[String], timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut inner = self.lock();
                Self::touch(&mut inner, who);
                if !ids.is_empty() {
                    if ids.iter().any(|id| !inner.jobs.contains_key(id)) {
                        let unknown: Vec<&String> = ids
                            .iter()
                            .filter(|id| !inner.jobs.contains_key(*id))
                            .collect();
                        return json!({
                            "reason": "unknown-job",
                            "unknown": unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                        });
                    }
                    let settled: Vec<Value> = ids
                        .iter()
                        .filter_map(|id| inner.jobs.get(id))
                        .filter(|j| j.state != JobState::Running)
                        .map(|j| {
                            json!({
                                "label": j.label,
                                "state": j.state.as_str(),
                                "result": j.result,
                            })
                        })
                        .collect();
                    if settled.len() == ids.len() {
                        return json!({ "reason": "jobs-done", "jobs": settled });
                    }
                }
                if let Some(a) = inner.agents.get_mut(who) {
                    if let Some(msg) = a.mailbox.pop_front() {
                        return json!({ "reason": "message", "from": msg.from, "text": msg.text });
                    }
                }
            }
            if Instant::now() >= deadline {
                return json!({ "reason": "timeout" });
            }
            tokio::time::sleep(POLL_TICK).await;
        }
    }

    // -- lifecycle sweeper --

    /// Park agents idle beyond the TTL; returns the number parked.
    pub fn sweep_idle(&self) -> usize {
        let mut inner = self.lock();
        let mut parked = 0;
        for a in inner.agents.values_mut() {
            if matches!(a.state, AgentState::Idle | AgentState::Running)
                && a.last_active.elapsed() > IDLE_TTL
                && a.mailbox.is_empty()
            {
                a.state = AgentState::Parked;
                parked += 1;
            }
        }
        parked
    }

    /// Background sweeper: parks idle agents every 60s (call once from main).
    pub fn start_sweeper(self: &Arc<Self>) {
        let host = self.clone();
        tokio::spawn(async move {
            // ponytail: fixed 60s sweep; thousands of agents would want an ordered deadline heap
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                host.sweep_idle();
            }
        });
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Truncate to a char budget for event payloads and summaries.
pub fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> AgentHost {
        AgentHost::new(
            Arc::new(Config::default()),
            Arc::new(|_h: Option<&str>| {
                AnyProvider::MockSubagent(crate::llm::MockSubagent::default())
            }),
        )
    }

    #[test]
    fn bundled三定义齐全且scout只读() {
        let defs = bundled();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["scout", "task", "advisor"]);
        let scout = &defs[0];
        assert_eq!(
            scout.tools,
            Some(vec![
                "read".to_string(),
                "glob".to_string(),
                "grep".to_string()
            ])
        );
        assert!(!scout.spawns);
        assert!(defs[1].spawns, "task 可派发");
    }

    #[test]
    fn frontmatter解析含正文系统提示() {
        let md =
            "---\nname: demo\ndescription: 演示\ntools: [read]\nspawns: false\n---\n你是演示代理。";
        let def = parse_frontmatter(md, DefSource::Project).unwrap();
        assert_eq!(def.name, "demo");
        assert_eq!(def.system_prompt, "你是演示代理。");
        assert_eq!(def.tools, Some(vec!["read".to_string()]));
        assert!(!def.spawns);
    }

    #[test]
    fn frontmatter缺name报错() {
        assert!(parse_frontmatter("---\ndescription: x\n---\nbody", DefSource::User).is_err());
    }

    #[test]
    fn 发现顺序项目优先同名首个胜() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        let agents = proj.join(".dscode").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        // Project defines a custom agent and shadows bundled `scout`.
        std::fs::write(
            agents.join("scout.md"),
            "---\nname: scout\ndescription: 项目版 scout\n---\n项目定制。",
        )
        .unwrap();
        std::fs::write(
            agents.join("extra.md"),
            "---\nname: extra\ndescription: 项目自有\n---\n自有代理。",
        )
        .unwrap();
        let defs = discover(proj);
        let scout = defs.iter().find(|d| d.name == "scout").unwrap();
        assert_eq!(scout.source, DefSource::Project);
        assert_eq!(scout.system_prompt, "项目定制。");
        assert!(defs.iter().any(|d| d.name == "extra"));
        // bundled task/advisor still present
        assert!(defs
            .iter()
            .any(|d| d.name == "task" && d.source == DefSource::Bundled));
    }

    #[tokio::test]
    async fn send_wait往返与receipt() {
        let h = host();
        let a1 = h.register_agent("scout");
        let a2 = h.register_agent("task");
        let r = h.send(&a1, &a2, "在吗？");
        assert_eq!(r, SendReceipt::Injected); // target is running
        let msg = match h.wait_inbox(&a2, Some(&a1), Duration::from_secs(1)).await {
            WaitOutcome::Message(m) => m,
            WaitOutcome::Timeout => panic!("应有消息"),
        };
        assert_eq!(msg.text, "在吗？");
        assert_eq!(msg.from, a1);
        // consumed: peek now empty
        assert!(h.inbox_peek(&a2).is_empty());
    }

    #[tokio::test]
    async fn wait超时是正常结果() {
        let h = host();
        let a = h.register_agent("task");
        assert!(matches!(
            h.wait_inbox(&a, None, Duration::from_millis(60)).await,
            WaitOutcome::Timeout
        ));
    }

    #[test]
    fn mailbox上限100() {
        let h = host();
        let a = h.register_agent("task");
        for i in 0..100 {
            assert_eq!(
                h.send("Main", &a, &format!("m{i}")),
                SendReceipt::Injected,
                "第 {i} 条应成功"
            );
        }
        match h.send("Main", &a, "overflow") {
            SendReceipt::Failed(e) => assert!(e.contains("mailbox"), "应报 mailbox 满：{e}"),
            other => panic!("第 101 条应失败，实际 {other:?}"),
        }
    }

    #[test]
    fn parked代理被消息复活() {
        let h = host();
        let a = h.register_agent("scout");
        h.lock().agents.get_mut(&a).unwrap().state = AgentState::Parked;
        assert_eq!(h.send("Main", &a, "醒醒"), SendReceipt::Revived);
        assert_eq!(h.lock().agents.get(&a).unwrap().state, AgentState::Running);
    }

    #[test]
    fn idle超TTL自动parked() {
        let h = host();
        let a = h.register_agent("scout");
        h.complete_agent(&a, true); // → idle
        h.lock().agents.get_mut(&a).unwrap().last_active =
            Instant::now() - IDLE_TTL - Duration::from_secs(1);
        let parked = h.sweep_idle();
        assert_eq!(parked, 1);
        assert_eq!(h.lock().agents.get(&a).unwrap().state, AgentState::Parked);
    }

    #[tokio::test]
    async fn jobs_wait先到先返回() {
        let h = Arc::new(host());
        let a = h.register_agent("task");
        // job settles mid-wait → jobs-done
        let j = h.job_begin("real");
        let h2 = h.clone();
        let jid = j.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            h2.job_end(&jid, true, "done!".into());
        });
        let out = h.job_wait(&a, &[j], Duration::from_secs(2)).await;
        assert_eq!(out["reason"], "jobs-done");
        assert_eq!(out["jobs"][0]["state"], "completed");
        // timeout is a normal outcome
        let j2 = h.job_begin("slow");
        let out2 = h.job_wait(&a, &[j2], Duration::from_millis(60)).await;
        assert_eq!(out2["reason"], "timeout");
    }

    #[test]
    fn job_cancel取消运行中任务() {
        let h = host();
        let j = h.job_begin("x");
        let res = h.job_cancel(&[j.clone()]);
        assert_eq!(res[0].1, "已取消");
        // ending after cancel does not resurrect
        h.job_end(&j, true, "late".into());
        assert_eq!(h.lock().jobs.get(&j).unwrap().state.as_str(), "cancelled");
    }

    #[test]
    fn transcript历史句柄与artifacts目录() {
        let h = host();
        let a = h.register_agent("task");
        h.transcript_push(&a, "user: 做事");
        h.transcript_push(&a, "assistant: 完成");
        let text = h.history_text(&a).unwrap();
        assert!(text.contains("user: 做事"));
        assert!(h.history_text("nope").is_err());
        // artifacts dir is a sibling of sessions dir
        assert_eq!(
            h.artifacts_dir(),
            Config::default()
                .sessions_dir
                .parent()
                .unwrap()
                .join("artifacts")
        );
    }

    #[test]
    fn events队列drain后清空() {
        let h = host();
        h.push_event("agent/spawned", serde_json::json!({ "x": 1 }));
        let evs = h.drain_events();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].0, "agent/spawned");
        assert!(h.drain_events().is_empty());
    }
    #[test]
    fn 会话切换拒绝活动任务并清空旧会话投递() {
        let h = host();
        let agent = h.register_agent("task");
        h.push_pending(&agent, "旧结果".into());
        h.push_event("agent/message", serde_json::json!({ "agent": agent }));
        assert!(h.reset_for_session_switch().is_err());

        h.complete_agent(&agent, true);
        let job = h.job_begin("旧 job");
        assert!(h.reset_for_session_switch().is_err());
        h.job_end(&job, true, "完成".into());
        h.reset_for_session_switch().unwrap();

        let roster = h.roster();
        assert_eq!(roster["agents"].as_array().unwrap().len(), 1);
        assert_eq!(roster["agents"][0]["id"], "Main");
        assert!(roster["jobs"].as_array().unwrap().is_empty());
        assert!(h.take_pending().is_empty());
        assert!(h.drain_events().is_empty());
        assert!(h.history_text(&agent).is_err());
    }
}
