//! Task management tools (tools.zh.md §3.7): TaskCreate / TaskUpdate / TaskGet / TaskList.
//! Incremental taskId-handle operations replacing TodoWrite-style whole-list rewrites.
//! State is session-resident: an in-memory store plus an event-sourced projection over
//! `task/write` events (session.zh.md) — replaying the log rebuilds the list; no standalone file.
//! Live execution and replay fold the very same `TaskAction`, keeping both paths in lockstep.

use super::{Tier, Tool, ToolOutput};
use crate::session::{Event, SessionLog};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Event key for every task mutation (registered in session/events.rs; DScode-owned, ignorable=true).
pub const TASK_EVENT: &str = "task/write";

/// Task lifecycle (§3.7): pending → in_progress → completed / deleted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Deleted,
}

impl TaskStatus {
    pub fn label(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Completed => "completed",
            TaskStatus::Deleted => "deleted",
        }
    }

    /// TUI panel icon (status glyph + title is the whole panel contract).
    pub fn icon(self) -> &'static str {
        match self {
            TaskStatus::Pending => "○",
            TaskStatus::InProgress => "◐",
            TaskStatus::Completed => "●",
            TaskStatus::Deleted => "✗",
        }
    }
}

/// Spec arrows only: pending → in_progress → completed/deleted (pending may also exit straight to
/// completed/deleted); completed and deleted are terminal. in_progress → pending has no spec arrow → rejected.
fn transition_allowed(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    match from {
        Pending => matches!(to, InProgress | Completed | Deleted),
        InProgress => matches!(to, Completed | Deleted),
        Completed | Deleted => false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub id: u64,
    pub title: String,
    pub status: TaskStatus,
    /// Ids this task blocks (those tasks wait on this one).
    pub blocks: Vec<u64>,
    /// Ids this task is blocked by.
    pub blocked_by: Vec<u64>,
}

/// One recorded mutation, serialized into `task/write` event data. Field names are camelCase
/// to match the model-visible tool schemas; optional lists default to empty.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum TaskAction {
    Create {
        #[serde(rename = "taskId")]
        task_id: u64,
        title: String,
        #[serde(rename = "addBlocks", default, skip_serializing_if = "Vec::is_empty")]
        add_blocks: Vec<u64>,
        #[serde(
            rename = "addBlockedBy",
            default,
            skip_serializing_if = "Vec::is_empty"
        )]
        add_blocked_by: Vec<u64>,
    },
    Update {
        #[serde(rename = "taskId")]
        task_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<TaskStatus>,
        #[serde(rename = "addBlocks", default, skip_serializing_if = "Vec::is_empty")]
        add_blocks: Vec<u64>,
        #[serde(
            rename = "removeBlocks",
            default,
            skip_serializing_if = "Vec::is_empty"
        )]
        remove_blocks: Vec<u64>,
        #[serde(
            rename = "addBlockedBy",
            default,
            skip_serializing_if = "Vec::is_empty"
        )]
        add_blocked_by: Vec<u64>,
        #[serde(
            rename = "removeBlockedBy",
            default,
            skip_serializing_if = "Vec::is_empty"
        )]
        remove_blocked_by: Vec<u64>,
    },
}

struct TaskState {
    /// Task ids are allocated from 1 (#0 reads badly for models and humans).
    next_id: u64,
    tasks: BTreeMap<u64, Task>,
}

/// Session-resident task state, shared by the four tools, the TUI panel and the replay path.
pub struct TaskStore {
    state: Mutex<TaskState>,
    /// Recorded (not yet flushed) `task/write` payloads; drained into the session log by the turn loop.
    pending: Mutex<Vec<Value>>,
}

impl TaskStore {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(TaskState {
                next_id: 1,
                tasks: BTreeMap::new(),
            }),
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Apply one action under the state lock, then record its payload for the next flush.
    /// `build` receives the id the action will allocate (Create), keeping allocation race-free.
    fn mutate(&self, build: impl FnOnce(u64) -> TaskAction) -> Result<Task, String> {
        let mut st = self.state.lock();
        let action = build(st.next_id);
        let touched = apply_inner(&mut st, &action)?;
        self.pending
            .lock()
            .push(serde_json::to_value(&action).expect("task action serializes"));
        Ok(touched)
    }

    /// TaskCreate path: allocate a fresh id and create the task with optional dependency edges.
    pub fn execute_create(
        &self,
        title: String,
        add_blocks: Vec<u64>,
        add_blocked_by: Vec<u64>,
    ) -> Result<Task, String> {
        self.mutate(|id| TaskAction::Create {
            task_id: id,
            title,
            add_blocks,
            add_blocked_by,
        })
    }

    /// TaskUpdate path: incremental status transition and/or edge changes — never a whole-list rewrite.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_update(
        &self,
        task_id: u64,
        status: Option<TaskStatus>,
        add_blocks: Vec<u64>,
        remove_blocks: Vec<u64>,
        add_blocked_by: Vec<u64>,
        remove_blocked_by: Vec<u64>,
    ) -> Result<Task, String> {
        self.mutate(|_| TaskAction::Update {
            task_id,
            status,
            add_blocks,
            remove_blocks,
            add_blocked_by,
            remove_blocked_by,
        })
    }

    /// Fold `task/write` events from the log back into the store (resume/fork rebuild).
    /// Non-task events are skipped; a malformed payload is a hard error naming the seq.
    pub fn replay(&self, events: &[Event]) -> Result<(), String> {
        let mut st = self.state.lock();
        for ev in events {
            if ev.kind != TASK_EVENT {
                continue;
            }
            let action: TaskAction = serde_json::from_value(ev.data.clone())
                .map_err(|e| format!("task/write 载荷损坏（seq {}）：{e}", ev.seq))?;
            apply_inner(&mut st, &action)?;
        }
        Ok(())
    }

    pub fn replace_from_events(&self, events: &[Event]) -> Result<(), String> {
        let mut next = TaskState {
            next_id: 1,
            tasks: BTreeMap::new(),
        };
        for event in events {
            if event.kind != TASK_EVENT {
                continue;
            }
            let action: TaskAction = serde_json::from_value(event.data.clone())
                .map_err(|error| format!("task/write 载荷损坏（seq {}）：{error}", event.seq))?;
            apply_inner(&mut next, &action)?;
        }
        *self.state.lock() = next;
        self.pending.lock().clear();
        Ok(())
    }

    /// Drain recorded mutations into the session log (called by the turn loop after tool results).
    pub fn flush(&self, log: &mut SessionLog) {
        let drained: Vec<Value> = self.pending.lock().drain(..).collect();
        for data in drained {
            log.log(TASK_EVENT, data);
        }
    }

    /// Visible tasks (deleted filtered out), ordered by id.
    pub fn list(&self) -> Vec<Task> {
        self.state
            .lock()
            .tasks
            .values()
            .filter(|t| t.status != TaskStatus::Deleted)
            .cloned()
            .collect()
    }

    /// Read one task by id (deleted included — the model can see it was deleted).
    pub fn get(&self, id: u64) -> Option<Task> {
        self.state.lock().tasks.get(&id).cloned()
    }
}

/// Pure fold of one action onto the state. Works on a clone and commits only on full success,
/// so a rejected action (bad transition, missing target, cycle) leaves the store untouched.
fn apply_inner(state: &mut TaskState, action: &TaskAction) -> Result<Task, String> {
    let mut tasks = state.tasks.clone();
    let touched = match action {
        TaskAction::Create {
            task_id,
            title,
            add_blocks,
            add_blocked_by,
        } => {
            if *task_id != state.next_id {
                return Err(format!(
                    "任务 id 不连续：#{task_id}（期望 #{}）",
                    state.next_id
                ));
            }
            if title.trim().is_empty() {
                return Err("任务标题不能为空".into());
            }
            tasks.insert(
                *task_id,
                Task {
                    id: *task_id,
                    title: title.trim().to_string(),
                    status: TaskStatus::Pending,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                },
            );
            for &blocked in add_blocks {
                add_edge_checked(&mut tasks, *task_id, blocked)?;
            }
            for &blocker in add_blocked_by {
                add_edge_checked(&mut tasks, blocker, *task_id)?;
            }
            state.next_id += 1;
            tasks.get(task_id).cloned().unwrap()
        }
        TaskAction::Update {
            task_id,
            status,
            add_blocks,
            remove_blocks,
            add_blocked_by,
            remove_blocked_by,
        } => {
            let from = tasks
                .get(task_id)
                .ok_or_else(|| format!("任务 #{task_id} 不存在"))?
                .status;
            // Terminal states reject every update (completed/deleted are absorbing).
            if !transition_would_pass(from, *status) {
                return Err(format!(
                    "任务 #{task_id} 处于终态 {}，拒绝更新",
                    from.label()
                ));
            }
            // Edge removals are idempotent; additions are validated (existence, self, cycle).
            for &blocked in remove_blocks {
                remove_edge(&mut tasks, *task_id, blocked);
            }
            for &blocker in remove_blocked_by {
                remove_edge(&mut tasks, blocker, *task_id);
            }
            for &blocked in add_blocks {
                add_edge_checked(&mut tasks, *task_id, blocked)?;
            }
            for &blocker in add_blocked_by {
                add_edge_checked(&mut tasks, blocker, *task_id)?;
            }
            if let Some(to) = *status {
                if from != to {
                    tasks.get_mut(task_id).unwrap().status = to;
                    if to == TaskStatus::Deleted {
                        clear_edges_of(&mut tasks, *task_id);
                    }
                }
            }
            tasks.get(task_id).cloned().unwrap()
        }
    };
    state.tasks = tasks;
    Ok(touched)
}

/// Terminal check + requested transition check in one place: any update on a terminal task
/// (even edge-only or no-op) is rejected; a non-terminal task must satisfy the spec arrows.
fn transition_would_pass(from: TaskStatus, to: Option<TaskStatus>) -> bool {
    match from {
        TaskStatus::Completed | TaskStatus::Deleted => false,
        _ => to.map_or(true, |to| to == from || transition_allowed(from, to)),
    }
}

/// Add a `blocker blocks blocked` edge, maintaining both directions. Rejects unknown ids,
/// self-edges and edges that would close a dependency cycle.
fn add_edge_checked(
    tasks: &mut BTreeMap<u64, Task>,
    blocker: u64,
    blocked: u64,
) -> Result<(), String> {
    if !tasks.contains_key(&blocker) || !tasks.contains_key(&blocked) {
        return Err(format!("依赖边引用不存在的任务：#{blocker} → #{blocked}"));
    }
    if blocker == blocked {
        return Err(format!("任务 #{blocker} 不能依赖自身"));
    }
    if would_cycle(tasks, blocker, blocked) {
        return Err(format!("依赖边 #{} → #{} 会形成循环依赖", blocker, blocked));
    }
    let t = tasks.get_mut(&blocker).unwrap();
    if !t.blocks.contains(&blocked) {
        t.blocks.push(blocked);
    }
    let t = tasks.get_mut(&blocked).unwrap();
    if !t.blocked_by.contains(&blocker) {
        t.blocked_by.push(blocker);
    }
    Ok(())
}

/// Adding blocker→blocked closes a cycle iff `blocked` already reaches `blocker` via blocks edges.
// ponytail: O(V+E) DFS per added edge — fine for session-scale task counts
fn would_cycle(tasks: &BTreeMap<u64, Task>, blocker: u64, blocked: u64) -> bool {
    let mut stack = vec![blocked];
    let mut seen = BTreeSet::new();
    while let Some(cur) = stack.pop() {
        if cur == blocker {
            return true;
        }
        if !seen.insert(cur) {
            continue;
        }
        if let Some(t) = tasks.get(&cur) {
            stack.extend(t.blocks.iter().copied());
        }
    }
    false
}

fn remove_edge(tasks: &mut BTreeMap<u64, Task>, blocker: u64, blocked: u64) {
    if let Some(t) = tasks.get_mut(&blocker) {
        t.blocks.retain(|&id| id != blocked);
    }
    if let Some(t) = tasks.get_mut(&blocked) {
        t.blocked_by.retain(|&id| id != blocker);
    }
}

/// Deleted tasks take their edges with them: drop the id from every other task's lists.
fn clear_edges_of(tasks: &mut BTreeMap<u64, Task>, id: u64) {
    for t in tasks.values_mut() {
        t.blocks.retain(|&b| b != id);
        t.blocked_by.retain(|&b| b != id);
    }
    if let Some(t) = tasks.get_mut(&id) {
        t.blocks.clear();
        t.blocked_by.clear();
    }
}

// —— Model-visible formatting ——

fn fmt_ids(ids: &[u64]) -> String {
    ids.iter()
        .map(|i| format!("#{i}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn task_line(t: &Task) -> String {
    format!("#{} [{}] {}", t.id, t.status.label(), t.title)
}

fn task_detail(t: &Task) -> String {
    let mut s = task_line(t);
    if !t.blocks.is_empty() {
        s.push_str(&format!("\n  blocks: {}", fmt_ids(&t.blocks)));
    }
    if !t.blocked_by.is_empty() {
        s.push_str(&format!("\n  blocked by: {}", fmt_ids(&t.blocked_by)));
    }
    s
}

fn ids_arg(args: &Value, key: &str) -> Result<Vec<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.as_u64().ok_or_else(|| format!("参数 {key} 须为整数数组")))
            .collect(),
        Some(_) => Err(format!("参数 {key} 须为整数数组")),
    }
}

fn err(msg: String) -> ToolOutput {
    ToolOutput {
        output: msg,
        exit_code: Some(1),
    }
}

/// The four task tools sharing one store, for registry assembly (tool/mod.rs).
pub fn tools(store: Arc<TaskStore>) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(TaskCreateTool(store.clone())),
        Box::new(TaskUpdateTool(store.clone())),
        Box::new(TaskGetTool(store.clone())),
        Box::new(TaskListTool(store)),
    ]
}

struct TaskCreateTool(Arc<TaskStore>);

#[async_trait::async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        "TaskCreate"
    }

    fn description(&self) -> &str {
        "创建任务并返回新 taskId；可选 addBlocks（本任务阻塞哪些任务）/ addBlockedBy（本任务被哪些任务阻塞）依赖边"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "任务标题" },
                "addBlocks": { "type": "array", "items": { "type": "integer" }, "description": "本任务阻塞的任务 id 列表" },
                "addBlockedBy": { "type": "array", "items": { "type": "integer" }, "description": "阻塞本任务的任务 id 列表" }
            },
            "required": ["title"]
        })
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    async fn execute(&self, arguments: &Value) -> ToolOutput {
        let Some(title) = arguments.get("title").and_then(Value::as_str) else {
            return err("缺少参数 title".into());
        };
        let mut edges = [Vec::new(), Vec::new()];
        for (i, key) in ["addBlocks", "addBlockedBy"].iter().enumerate() {
            match ids_arg(arguments, key) {
                Ok(v) => edges[i] = v,
                Err(e) => return err(e),
            }
        }
        let [add_blocks, add_blocked_by] = edges;
        match self
            .0
            .execute_create(title.to_string(), add_blocks, add_blocked_by)
        {
            Ok(t) => {
                let mut out = format!("已创建任务 {}", task_line(&t));
                if !t.blocks.is_empty() {
                    out.push_str(&format!("；blocks {}", fmt_ids(&t.blocks)));
                }
                if !t.blocked_by.is_empty() {
                    out.push_str(&format!("；blocked by {}", fmt_ids(&t.blocked_by)));
                }
                ToolOutput {
                    output: out,
                    exit_code: Some(0),
                }
            }
            Err(e) => err(e),
        }
    }
}

struct TaskUpdateTool(Arc<TaskStore>);

#[async_trait::async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        "TaskUpdate"
    }

    fn description(&self) -> &str {
        "按 taskId 增量更新：流转状态（pending→in_progress→completed/deleted）与增改依赖边（addBlocks/removeBlocks/addBlockedBy/removeBlockedBy）；终态任务拒绝更新"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "taskId": { "type": "integer", "description": "目标任务 id" },
                "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "deleted"] },
                "addBlocks": { "type": "array", "items": { "type": "integer" } },
                "removeBlocks": { "type": "array", "items": { "type": "integer" } },
                "addBlockedBy": { "type": "array", "items": { "type": "integer" } },
                "removeBlockedBy": { "type": "array", "items": { "type": "integer" } }
            },
            "required": ["taskId"]
        })
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    async fn execute(&self, arguments: &Value) -> ToolOutput {
        let Some(task_id) = arguments.get("taskId").and_then(Value::as_u64) else {
            return err("缺少参数 taskId".into());
        };
        let status = match arguments.get("status").and_then(Value::as_str) {
            None => None,
            Some(s) => match serde_json::from_value::<TaskStatus>(Value::String(s.to_string())) {
                Ok(st) => Some(st),
                Err(_) => {
                    return err(format!(
                        "未知状态：{s}（须为 pending/in_progress/completed/deleted）"
                    ))
                }
            },
        };
        let mut edges = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for (i, key) in [
            "addBlocks",
            "removeBlocks",
            "addBlockedBy",
            "removeBlockedBy",
        ]
        .iter()
        .enumerate()
        {
            match ids_arg(arguments, key) {
                Ok(v) => edges[i] = v,
                Err(e) => return err(e),
            }
        }
        let [add_blocks, remove_blocks, add_blocked_by, remove_blocked_by] = edges;
        match self.0.execute_update(
            task_id,
            status,
            add_blocks,
            remove_blocks,
            add_blocked_by,
            remove_blocked_by,
        ) {
            Ok(t) => ToolOutput {
                output: format!("已更新任务 {}", task_detail(&t)),
                exit_code: Some(0),
            },
            Err(e) => err(e),
        }
    }
}

struct TaskGetTool(Arc<TaskStore>);

#[async_trait::async_trait]
impl Tool for TaskGetTool {
    fn name(&self) -> &str {
        "TaskGet"
    }

    fn description(&self) -> &str {
        "按 taskId 读取单个任务（含状态与依赖边；已删除任务可查，标记 deleted）"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "taskId": { "type": "integer", "description": "目标任务 id" }
            },
            "required": ["taskId"]
        })
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    async fn execute(&self, arguments: &Value) -> ToolOutput {
        let Some(task_id) = arguments.get("taskId").and_then(Value::as_u64) else {
            return err("缺少参数 taskId".into());
        };
        match self.0.get(task_id) {
            Some(t) => ToolOutput {
                output: task_detail(&t),
                exit_code: Some(0),
            },
            None => err(format!("任务 #{task_id} 不存在")),
        }
    }
}

struct TaskListTool(Arc<TaskStore>);

#[async_trait::async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "TaskList"
    }

    fn description(&self) -> &str {
        "列出全部未删除任务（id、状态、标题）"
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    async fn execute(&self, _arguments: &Value) -> ToolOutput {
        let visible = self.0.list();
        if visible.is_empty() {
            return ToolOutput {
                output: "（无任务）".into(),
                exit_code: Some(0),
            };
        }
        let out = visible.iter().map(task_line).collect::<Vec<_>>().join("\n");
        ToolOutput {
            output: out,
            exit_code: Some(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk() -> (Arc<TaskStore>, Vec<Box<dyn Tool>>) {
        let store = Arc::new(TaskStore::new());
        let tools = tools(store.clone());
        (store, tools)
    }

    async fn create(store: &TaskStore, title: &str) -> Task {
        store.execute_create(title.into(), vec![], vec![]).unwrap()
    }

    #[tokio::test]
    async fn task_创建_流转_依赖边_list_get_往返() {
        let (store, tools) = mk();
        let by_name = |n: &str| tools.iter().find(|t| t.name() == n).unwrap();

        let out = by_name("TaskCreate")
            .execute(&json!({ "title": "设计事件载荷" }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);

        let out = by_name("TaskCreate")
            .execute(&json!({ "title": "实现四工具", "addBlockedBy": [1] }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
        assert!(out.output.contains("blocked by #1"), "{}", out.output);

        // Status flow: pending → in_progress → completed.
        let out = by_name("TaskUpdate")
            .execute(&json!({ "taskId": 1, "status": "in_progress" }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
        assert_eq!(store.get(1).unwrap().status, TaskStatus::InProgress);
        let out = by_name("TaskUpdate")
            .execute(&json!({ "taskId": 1, "status": "completed" }))
            .await;
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(store.get(1).unwrap().status, TaskStatus::Completed);

        // Incremental edge change: task 2 additionally blocks a new task 3.
        let out = by_name("TaskCreate")
            .execute(&json!({ "title": "写测试", "addBlockedBy": [2] }))
            .await;
        assert_eq!(out.exit_code, Some(0));
        let out = by_name("TaskUpdate")
            .execute(&json!({ "taskId": 2, "addBlocks": [3] }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
        assert_eq!(store.get(2).unwrap().blocks, vec![3]);
        assert_eq!(store.get(3).unwrap().blocked_by, vec![2]);

        let out = by_name("TaskGet").execute(&json!({ "taskId": 2 })).await;
        assert_eq!(out.exit_code, Some(0));
        assert!(
            out.output.contains("#2") && out.output.contains("blocks: #3"),
            "{}",
            out.output
        );

        let out = by_name("TaskList").execute(&json!({})).await;
        assert_eq!(out.exit_code, Some(0));
        assert!(
            out.output.contains("#1 [completed] 设计事件载荷"),
            "{}",
            out.output
        );
        assert_eq!(out.output.lines().count(), 3, "{}", out.output);
    }

    #[tokio::test]
    async fn task_非法状态转换拒绝且状态不变() {
        let store = Arc::new(TaskStore::new());
        create(&store, "a").await;
        create(&store, "b").await;

        // completed → pending rejected.
        store
            .execute_update(
                1,
                Some(TaskStatus::Completed),
                vec![],
                vec![],
                vec![],
                vec![],
            )
            .unwrap();
        let e = store
            .execute_update(1, Some(TaskStatus::Pending), vec![], vec![], vec![], vec![])
            .unwrap_err();
        assert!(e.contains("终态"), "{e}");
        assert_eq!(store.get(1).unwrap().status, TaskStatus::Completed);

        // deleted → in_progress rejected.
        store
            .execute_update(2, Some(TaskStatus::Deleted), vec![], vec![], vec![], vec![])
            .unwrap();
        assert!(store
            .execute_update(
                2,
                Some(TaskStatus::InProgress),
                vec![],
                vec![],
                vec![],
                vec![]
            )
            .is_err());

        // in_progress → pending has no spec arrow → rejected.
        create(&store, "c").await;
        store
            .execute_update(
                3,
                Some(TaskStatus::InProgress),
                vec![],
                vec![],
                vec![],
                vec![],
            )
            .unwrap();
        assert!(store
            .execute_update(3, Some(TaskStatus::Pending), vec![], vec![], vec![], vec![])
            .is_err());
        assert_eq!(store.get(3).unwrap().status, TaskStatus::InProgress);

        // pending → in_progress → completed happy path stays valid.
        create(&store, "d").await;
        store
            .execute_update(
                4,
                Some(TaskStatus::InProgress),
                vec![],
                vec![],
                vec![],
                vec![],
            )
            .unwrap();
        store
            .execute_update(
                4,
                Some(TaskStatus::Completed),
                vec![],
                vec![],
                vec![],
                vec![],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn task_终态任务拒绝边更新() {
        let store = Arc::new(TaskStore::new());
        create(&store, "a").await;
        create(&store, "b").await;
        store
            .execute_update(
                1,
                Some(TaskStatus::Completed),
                vec![],
                vec![],
                vec![],
                vec![],
            )
            .unwrap();
        // Edge-only update on a terminal task is still rejected.
        assert!(store
            .execute_update(1, None, vec![2], vec![], vec![], vec![])
            .is_err());
        assert!(store.get(1).unwrap().blocks.is_empty());
    }

    #[tokio::test]
    async fn task_删除任务不再出现在list且清理依赖边() {
        let (store, tools) = mk();
        create(&store, "a").await;
        store.execute_create("b".into(), vec![], vec![1]).unwrap();
        let out = tools
            .iter()
            .find(|t| t.name() == "TaskUpdate")
            .unwrap()
            .execute(&json!({ "taskId": 1, "status": "deleted" }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);

        let visible = store.list();
        assert!(
            visible.iter().all(|t| t.id != 1),
            "deleted must not be listed"
        );
        // Edge cleanup: task 2 was blocked by 1 — the edge disappears with the deletion.
        assert!(store.get(2).unwrap().blocked_by.is_empty());
        // TaskGet still sees it, marked deleted.
        assert_eq!(store.get(1).unwrap().status, TaskStatus::Deleted);

        let out = tools
            .iter()
            .find(|t| t.name() == "TaskList")
            .unwrap()
            .execute(&json!({}))
            .await;
        assert!(!out.output.contains("#1"), "{}", out.output);
    }

    #[tokio::test]
    async fn task_依赖校验_不存在_自环_成环拒绝() {
        let store = Arc::new(TaskStore::new());
        create(&store, "a").await;

        // Unknown target.
        assert!(store.execute_create("x".into(), vec![], vec![9]).is_err());
        assert!(store
            .execute_update(1, None, vec![], vec![], vec![9], vec![])
            .is_err());
        // Self-edge.
        assert!(store
            .execute_update(1, None, vec![1], vec![], vec![], vec![])
            .is_err());
        // Cycle: 1 blocks 2, then 2 blocks 1 must be rejected.
        create(&store, "b").await;
        store
            .execute_update(1, None, vec![2], vec![], vec![], vec![])
            .unwrap();
        assert!(store
            .execute_update(2, None, vec![1], vec![], vec![], vec![])
            .is_err());
        // Transitive cycle: 3 blocked by 2, then 1 blocked by 3 closes 1→2→3→1.
        create(&store, "c").await;
        store
            .execute_update(3, None, vec![], vec![], vec![2], vec![])
            .unwrap();
        assert!(store
            .execute_update(1, None, vec![], vec![], vec![3], vec![])
            .is_err());
    }

    #[tokio::test]
    async fn task_事件回放重建状态一致() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = crate::session::SessionLog::create("task-replay", tmp.path()).unwrap();
        let store = TaskStore::new();

        create(&store, "设计").await;
        create(&store, "实现").await;
        store
            .execute_update(
                1,
                Some(TaskStatus::InProgress),
                vec![],
                vec![],
                vec![],
                vec![],
            )
            .unwrap();
        store
            .execute_update(
                1,
                Some(TaskStatus::Completed),
                vec![],
                vec![],
                vec![],
                vec![],
            )
            .unwrap();
        store
            .execute_create("测试".into(), vec![], vec![2])
            .unwrap();
        store
            .execute_update(2, Some(TaskStatus::Deleted), vec![], vec![], vec![], vec![])
            .unwrap();
        store.flush(&mut log);

        let events = log.read_all().unwrap();
        let task_events: Vec<&Event> = events.iter().filter(|e| e.kind == TASK_EVENT).collect();
        assert_eq!(
            task_events.len(),
            6,
            "every mutation is one task/write event"
        );
        assert!(
            task_events.iter().all(|e| e.ignorable),
            "DScode-owned key must be ignorable"
        );

        let rebuilt = TaskStore::new();
        rebuilt.replay(&events).unwrap();
        assert_eq!(rebuilt.list(), store.list());
        for id in 1..=3u64 {
            assert_eq!(
                rebuilt.get(id),
                store.get(id),
                "task #{id} must match after replay"
            );
        }
        // Id allocation continues after replay (no reuse of deleted ids).
        let t = rebuilt.execute_create("续".into(), vec![], vec![]).unwrap();
        assert_eq!(t.id, 4);

        // Replay into a store with existing state fails loudly (id discontinuity).
        let dirty = TaskStore::new();
        create(&dirty, "已有").await;
        assert!(dirty.replay(&events).is_err());
        dirty.replace_from_events(&events).unwrap();
        assert_eq!(dirty.list(), store.list());
        for id in 1..=3u64 {
            assert_eq!(dirty.get(id), store.get(id));
        }
    }

    #[tokio::test]
    async fn task_回放拒绝损坏载荷() {
        let store = TaskStore::new();
        let ev = Event {
            kind: TASK_EVENT.into(),
            seq: 0,
            time: 0,
            data: serde_json::json!({ "action": "nonsense" }),
            ignorable: true,
        };
        assert!(store.replay(&[ev]).is_err());
    }

    #[tokio::test]
    async fn task_tier声明照规格() {
        let (_, tools) = mk();
        let tier_of = |n: &str| tools.iter().find(|t| t.name() == n).unwrap().tier();
        assert_eq!(tier_of("TaskGet"), Tier::Read);
        assert_eq!(tier_of("TaskList"), Tier::Read);
        assert_eq!(tier_of("TaskCreate"), Tier::Write);
        assert_eq!(tier_of("TaskUpdate"), Tier::Write);
    }

    #[tokio::test]
    async fn task_缺参与坏参报错() {
        let (_, tools) = mk();
        let by_name = |n: &str| tools.iter().find(|t| t.name() == n).unwrap();
        assert_eq!(
            by_name("TaskCreate").execute(&json!({})).await.exit_code,
            Some(1)
        );
        assert_eq!(
            by_name("TaskGet").execute(&json!({})).await.exit_code,
            Some(1)
        );
        assert_eq!(
            by_name("TaskUpdate")
                .execute(&json!({ "taskId": 1, "status": "paused" }))
                .await
                .exit_code,
            Some(1)
        );
        assert_eq!(
            by_name("TaskCreate")
                .execute(&json!({ "title": "x", "addBlocks": ["a"] }))
                .await
                .exit_code,
            Some(1)
        );
        assert_eq!(
            by_name("TaskGet")
                .execute(&json!({ "taskId": 99 }))
                .await
                .exit_code,
            Some(1)
        );
    }
}
