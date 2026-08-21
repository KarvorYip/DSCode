//! Tool registry: `Tool` trait + `Tier` declaration + startup assembly (tools.zh.md §2/§4).
//! The registry is the model's only path to side effects; there is no bypass.

pub mod bash;
pub mod browser;
pub mod edit;
pub mod glob;
pub mod goal;
pub mod grep;
pub mod hub;
pub mod mcp;
pub mod read;
pub mod skill;
pub mod spawn;
pub mod task;
pub mod write;

use serde_json::Value;

/// Approval tier: side-effect weight classification, consumed by the decision chain (approval.zh.md §2.2).
/// Tools without a declared tier are treated as `Exec`; unannotated MCP tools as `Write`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// No side effects; never triggers approval.
    Read,
    /// Modifies files, session state, or reaches the outside through bounded channels.
    Write,
    /// Arbitrary command execution, or grants an approval-free subtree.
    Exec,
}

/// Tool execution result: output text (flowing back to the model) and an exit code, if any.
pub struct ToolOutput {
    pub output: String,
    pub exit_code: Option<i32>,
}

/// Tool execution context: sub-agent scope (approval flag, recursion depth, definition name),
/// the shared agent host, worktree cwd, and the session user-decision snapshot.
/// File/exec tools resolve relative paths against `cwd`; spawn/hub consume `agents` and the
/// depth/def guards (tools.zh.md §3.8/§3.9, approval.zh.md §2.10).
pub struct ToolCtx<'a> {
    pub config: &'a crate::config::Config,
    pub agents: &'a std::sync::Arc<crate::agent::AgentHost>,
    /// "Main" at the top level; the agent id inside a sub-agent.
    pub agent_id: &'a str,
    /// The executing agent's definition name (self-recursion guard); None at the top level.
    pub def_name: Option<&'a str>,
    pub depth: u8,
    pub is_subagent: bool,
    /// Worktree isolation: relative paths resolve here.
    pub cwd: Option<&'a std::path::Path>,
    /// Session-tier remembered decisions snapshot (approval.zh.md §2.10: user deny stays effective in children).
    pub decisions: Option<std::collections::BTreeMap<String, bool>>,
}

impl ToolCtx<'_> {
    /// Resolve a possibly-relative path against the execution cwd (worktree isolation).
    pub fn resolve(&self, path: &str) -> std::path::PathBuf {
        let p = std::path::Path::new(path);
        match (self.cwd, p.is_absolute()) {
            (Some(base), false) => base.join(p),
            _ => p.to_path_buf(),
        }
    }
}

/// Model-visible tool contract (tools.zh.md §2).
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Stable string name; session events reference the tool by this.
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// Input JSON schema (model-visible).
    fn parameters_schema(&self) -> Value;
    fn tier(&self) -> Tier;
    async fn execute(&self, arguments: &Value) -> ToolOutput;

    /// Hidden tools are callable but not advertised in the top-level model-visible list
    /// (tools.zh.md §3.11: think / yield / goal tools); `yield` still appears in child lists.
    fn hidden(&self) -> bool {
        false
    }

    /// Context-aware execution: sub-agent scope, worktree cwd, agent host. Default ignores the
    /// context (plain tools); spawn/hub/file tools override this.
    async fn execute_ctx(&self, _ctx: &ToolCtx<'_>, arguments: &Value) -> ToolOutput {
        self.execute(arguments).await
    }
}

#[derive(Default)]
pub struct SharedTools {
    tools: parking_lot::RwLock<Vec<std::sync::Arc<dyn Tool>>>,
}

impl SharedTools {
    pub fn snapshot(&self) -> Vec<std::sync::Arc<dyn Tool>> {
        self.tools.read().clone()
    }

    pub fn replace_boxed(&self, tools: Vec<Box<dyn Tool>>) -> Vec<String> {
        let tools: Vec<std::sync::Arc<dyn Tool>> =
            tools.into_iter().map(std::sync::Arc::from).collect();
        let names = tools.iter().map(|tool| tool.name().to_string()).collect();
        *self.tools.write() = tools;
        names
    }
}

/// A registry assembled once at startup.
pub struct Registry {
    tools: Vec<std::sync::Arc<dyn Tool>>,
}

impl Registry {
    /// Static builtins with a fresh child/session edit state.
    pub fn builtin() -> Self {
        Self::with_edit_session(std::sync::Arc::new(edit::EditSession::default()))
    }

    fn with_edit_session(edits: std::sync::Arc<edit::EditSession>) -> Self {
        Self {
            tools: vec![
                std::sync::Arc::new(bash::BashTool),
                std::sync::Arc::new(browser::BrowserTool),
                std::sync::Arc::new(read::ReadTool(edits.clone())),
                std::sync::Arc::new(write::WriteTool),
                std::sync::Arc::new(edit::EditTool(edits.clone())),
                std::sync::Arc::new(glob::GlobTool),
                std::sync::Arc::new(grep::GrepTool(edits)),
                std::sync::Arc::new(spawn::SpawnTool),
                std::sync::Arc::new(hub::HubTool),
                std::sync::Arc::new(spawn::YieldTool),
            ],
        }
    }

    /// Builtins plus task tools, sharing the top-level session's edit provenance/register state.
    pub fn with_tasks(
        store: std::sync::Arc<task::TaskStore>,
        edits: std::sync::Arc<edit::EditSession>,
    ) -> Self {
        let mut registry = Self::with_edit_session(edits);
        registry
            .tools
            .extend(task::tools(store).into_iter().map(std::sync::Arc::from));
        registry
    }

    /// Append the three goal tools sharing one GoalRuntime handle (chainable after with_tasks).
    /// Mounted only when the goal stack is enabled (TUI + goal.enabled; headless never mounts it),
    /// so the gate is effective at the schema level — disabled means absent from the model-visible list.
    pub fn and_goal(
        mut self,
        runtime: std::sync::Arc<parking_lot::Mutex<crate::goal::GoalRuntime>>,
    ) -> Self {
        self.tools
            .push(std::sync::Arc::new(goal::GetGoalTool(runtime.clone())));
        self.tools
            .push(std::sync::Arc::new(goal::CreateGoalTool(runtime.clone())));
        self.tools
            .push(std::sync::Arc::new(goal::UpdateGoalTool(runtime)));
        self
    }

    pub fn extend_shared(&mut self, tools: &[std::sync::Arc<dyn Tool>]) {
        self.tools.extend(tools.iter().cloned());
    }
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    /// The model-visible list of tool schemas (the tools field of LLM requests).
    /// Hidden tools (yield) are excluded at the top level (tools.zh.md §3.11).
    pub fn definitions(&self) -> Vec<Value> {
        self.tools
            .iter()
            .filter(|t| !t.hidden())
            .map(|t| definition_json(t.as_ref()))
            .collect()
    }

    /// Sub-agent visible list: `yield` included (the child's only legal exit); `spawn` stripped
    /// at the recursion cap (tools.zh.md §3.8). Goal tools are never present here — sub-agents
    /// are built from the builtin set only, so `create_goal` is schema-absent in children.
    pub fn child_definitions(&self, allow_spawn: bool) -> Vec<Value> {
        self.tools
            .iter()
            .filter(|t| t.name() != "spawn" || allow_spawn)
            .map(|t| definition_json(t.as_ref()))
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools.iter().map(|t| t.as_ref())
    }
}

fn definition_json(t: &dyn Tool) -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": t.name(),
            "description": t.description(),
            "parameters": t.parameters_schema(),
        }
    })
}
