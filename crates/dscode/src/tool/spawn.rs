//! spawn tool (tools.zh.md §3.8): sub-agent dispatch — batch `{context, tasks[]}` default,
//! flat single-shot `{task}`; per-item `agent` / `outputSchema`+`schemaMode` / `isolated`;
//! async background jobs by default (results auto-delivered into the conversation flow),
//! or sync under the `task.maxConcurrency` semaphore. Self-recursion is intercepted and the
//! depth cap strips `spawn` from children (defense re-checked here).
//! Also hosts the hidden `yield` tool (§3.11): the sub-agent's only legal exit, intercepted
//! by the runner — its `execute` only guards against top-level calls.

use super::{Tier, Tool, ToolCtx, ToolOutput};
use crate::agent::runner::{self, SpawnSpec};
use crate::agent::schema::SchemaMode;
use crate::agent::{discover, truncate_chars};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Semaphore;

fn err(msg: impl Into<String>) -> ToolOutput {
    ToolOutput {
        output: msg.into(),
        exit_code: Some(1),
    }
}

/// One parsed dispatch item (normalized from batch entry or flat form).
struct Item {
    task: String,
    agent: String,
    output_schema: Option<Value>,
    schema_mode: SchemaMode,
    isolated: bool,
}

pub struct SpawnTool;

#[async_trait::async_trait]
impl Tool for SpawnTool {
    fn name(&self) -> &str {
        "spawn"
    }

    fn description(&self) -> &str {
        "派发子代理任务。默认 batch 形态 {context, tasks:[{task, agent?, outputSchema?, \
schemaMode?, isolated?}]}，各项并发；或 flat 单发 {task, ...}。默认 async 后台执行、完成后\
自动投递结果；sync:true 同步等待（受 task.maxConcurrency 限流）。isolated:true 在 git \
worktree 隔离执行并产出可 apply 的 .patch"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "context": { "type": "string", "description": "batch 共享上下文（背景/共享事实），随每项任务下发" },
                "tasks": {
                    "type": "array",
                    "description": "batch 任务项；与 flat 的 task 二选一",
                    "items": {
                        "type": "object",
                        "properties": {
                            "task": { "type": "string", "description": "任务描述（自包含、可独立执行）" },
                            "agent": { "type": "string", "description": "agent 类型：scout（只读调研）/task（通用）/advisor（观察者）或 .dscode/agents 自定义；默认 task" },
                            "outputSchema": { "type": "object", "description": "JSON Schema：yield 产出的结构化契约" },
                            "schemaMode": { "type": "string", "enum": ["strict", "permissive"], "description": "默认 strict：不符合拒绝并重试" },
                            "isolated": { "type": "boolean", "description": "git worktree 隔离执行，产出 changes.patch" }
                        },
                        "required": ["task"]
                    }
                },
                "task": { "type": "string", "description": "flat 单发：一次性任务描述（与 tasks 二选一）" },
                "agent": { "type": "string", "description": "flat 形态的 agent 类型，默认 task" },
                "outputSchema": { "type": "object", "description": "flat 形态的产出契约" },
                "schemaMode": { "type": "string", "enum": ["strict", "permissive"] },
                "isolated": { "type": "boolean", "description": "flat 形态的 worktree 隔离" },
                "sync": { "type": "boolean", "description": "true=同步等待全部完成；默认 false=async 后台 job，结果完成后自动投递" }
            }
        })
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    async fn execute(&self, _arguments: &Value) -> ToolOutput {
        err("spawn 需要执行上下文（子代理派发仅在回合循环内可用）")
    }

    async fn execute_ctx(&self, ctx: &ToolCtx<'_>, arguments: &Value) -> ToolOutput {
        dispatch(ctx, arguments).await
    }
}

/// The hidden sub-agent exit (tools.zh.md §3.11): registered and callable, but advertised only
/// in child tool lists; the runner intercepts calls by name. This shell guards the top level.
pub struct YieldTool;

#[async_trait::async_trait]
impl Tool for YieldTool {
    fn name(&self) -> &str {
        "yield"
    }

    fn description(&self) -> &str {
        "子代理唯一合法出口：提交最终结果 {\"result\": ...}。仅子代理运行时内有效"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "result": { "description": "最终产出：字符串或符合 outputSchema 的 JSON 值" }
            },
            "required": ["result"]
        })
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    fn hidden(&self) -> bool {
        true
    }

    async fn execute(&self, _arguments: &Value) -> ToolOutput {
        err("yield 是子代理出口，由子代理运行时处理；主对话不可调用")
    }
}

/// Parse items from batch or flat form; None = neither shape present.
fn parse_items(args: &Value) -> Option<(Vec<Item>, Option<String>)> {
    let context = args
        .get("context")
        .and_then(Value::as_str)
        .map(str::to_string);
    let str_of = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
    if let Some(tasks) = args.get("tasks").and_then(Value::as_array) {
        let mut items = Vec::new();
        for t in tasks {
            let Some(task) = str_of(t.get("task").or_else(|| t.get("prompt"))) else {
                continue;
            };
            items.push(Item {
                task,
                agent: str_of(t.get("agent")).unwrap_or_else(|| "task".into()),
                output_schema: t.get("outputSchema").cloned(),
                schema_mode: SchemaMode::parse(str_of(t.get("schemaMode")).as_deref()),
                isolated: t.get("isolated").and_then(Value::as_bool).unwrap_or(false),
            });
        }
        if items.is_empty() {
            return None;
        }
        return Some((items, context));
    }
    // flat form
    let Some(task) = str_of(args.get("task")) else {
        return None;
    };
    let item = Item {
        task,
        agent: str_of(args.get("agent")).unwrap_or_else(|| "task".into()),
        output_schema: args.get("outputSchema").cloned(),
        schema_mode: SchemaMode::parse(str_of(args.get("schemaMode")).as_deref()),
        isolated: args
            .get("isolated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    let _ = context; // flat form carries no batch context
    Some((vec![item], None))
}

async fn dispatch(ctx: &ToolCtx<'_>, args: &Value) -> ToolOutput {
    let config = ctx.agents.config();
    let max_depth = config.task.max_recursion_depth;

    // Defensive depth cap (the registry normally strips spawn before this can fire).
    if ctx.depth >= max_depth {
        return err(format!(
            "已达子代理递归上限（task.maxRecursionDepth={max_depth}）：depth {} 不可再派发",
            ctx.depth
        ));
    }

    let Some((items, context)) = parse_items(args) else {
        return err("参数不完整：需要 batch 的 tasks[] 或 flat 的 task");
    };
    let sync = args.get("sync").and_then(Value::as_bool).unwrap_or(false);

    // Definition discovery: worktree cwd first (a child dispatching grandchildren sees the
    // worktree's own .dscode/agents), else the process cwd.
    let root = ctx
        .cwd
        .map(|p| p.to_path_buf())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let defs = discover(&root);

    let mut specs = Vec::new();
    for item in items {
        let Some(def) = defs.iter().find(|d| d.name == item.agent) else {
            return err(format!(
                "未知 agent 类型「{}」（可用：{}）",
                item.agent,
                defs.iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join("/")
            ));
        };
        // Self-recursion intercept: an agent dispatching its own type directly.
        if ctx.def_name == Some(def.name.as_str()) {
            return err(format!(
                "自递归拦截：{def_name} 不可直接派发 {def_name}（深度派发走 task.maxRecursionDepth 封顶）",
                def_name = def.name
            ));
        }
        // advisor's output never enters the main conversation flow (ticket 004 rev 6).
        let deliver = def.name != "advisor";
        specs.push(SpawnSpec {
            def: def.clone(),
            parent_id: ctx.agent_id.to_string(),
            context: context.clone().unwrap_or_default(),
            task: item.task,
            output_schema: item.output_schema,
            schema_mode: item.schema_mode,
            isolated: item.isolated,
            depth: ctx.depth + 1,
            deliver,
            async_mode: !sync,
            decisions: ctx.decisions.clone().unwrap_or_default(),
        });
    }

    let host = ctx.agents.clone();
    if sync {
        run_sync(host, specs, config.task.max_concurrency).await
    } else {
        run_async(host, specs)
    }
}

/// Sync execution: all items run under the maxConcurrency semaphore; the tool result carries
/// every outcome (advisor items report only their agent:// artifact).
async fn run_sync(
    host: Arc<crate::agent::AgentHost>,
    specs: Vec<SpawnSpec>,
    max: Option<usize>,
) -> ToolOutput {
    let n = specs.len();
    let sem = Arc::new(Semaphore::new(max.unwrap_or(n.max(1))));
    let mut handles = Vec::new();
    for spec in specs {
        // Acquire before spawning: in-flight agents are capped at maxConcurrency.
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .unwrap_or_else(|_| unreachable!("semaphore never closed"));
        let h = host.clone();
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            runner::run_agent(h, spec).await
        }));
    }
    let mut out = String::new();
    let mut ok_count = 0usize;
    for h in handles {
        match h.await {
            Ok(o) => {
                if o.ok {
                    ok_count += 1;
                }
                out.push_str(&render_outcome(&o));
            }
            Err(e) => out.push_str(&format!("[子代理任务崩溃] {e}\n")),
        }
    }
    ToolOutput {
        output: format!("同步派发完成：{ok_count}/{n} 成功。\n{out}"),
        exit_code: if ok_count == n { Some(0) } else { Some(1) },
    }
}

/// Async execution: background jobs; results auto-deliver into the conversation flow on
/// completion (spawn 即返回 job/agent ids)。
fn run_async(host: Arc<crate::agent::AgentHost>, specs: Vec<SpawnSpec>) -> ToolOutput {
    let mut lines = Vec::new();
    for spec in specs {
        let label = format!("spawn:{}", spec.def.name);
        let job_id = host.job_begin(&label);
        let h = host.clone();
        let deliver = spec.deliver;
        let agent_name = spec.def.name.clone();
        let jid = job_id.clone();
        let jh = tokio::spawn(async move {
            let outcome = runner::run_agent(h.clone(), spec).await;
            let summary = if deliver {
                outcome.text.clone()
            } else {
                format!(
                    "advisor（{agent_name}）完成，输出不回注主对话；产物见 agent://{}",
                    outcome.agent_id
                )
            };
            h.job_end(&jid, outcome.ok, truncate_chars(&summary, 400));
            if deliver && outcome.ok {
                h.push_pending(&outcome.agent_id, outcome.text.clone());
            }
        });
        host.job_set_handle(&job_id, jh);
        lines.push(format!("job {job_id}（{label}）已派发，完成后自动投递"));
    }
    ToolOutput {
        output: format!(
            "已派发 {} 个后台子代理任务（async）：\n{}",
            lines.len(),
            lines.join("\n")
        ),
        exit_code: Some(0),
    }
}

fn render_outcome(o: &runner::AgentOutcome) -> String {
    let patch = o
        .patch
        .as_ref()
        .map(|p| format!("\n  patch: {}", p.display()))
        .unwrap_or_default();
    if o.ok {
        format!(
            "[{}] 完成（agent://{}，产物 {}）{patch}\n{}\n",
            o.agent_id,
            o.agent_id,
            o.artifact.display(),
            o.text
        )
    } else {
        format!(
            "[{}] 失败（agent://{}）：{}\n",
            o.agent_id, o.agent_id, o.text
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentHost;
    use crate::config::Config;
    use crate::llm::{AnyProvider, MockSubagent};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn host() -> Arc<AgentHost> {
        Arc::new(AgentHost::new(
            Arc::new(Config::default()),
            Arc::new(|_h: Option<&str>| AnyProvider::MockSubagent(MockSubagent::default())),
        ))
    }

    fn ctx_for<'a>(host: &'a Arc<AgentHost>, def_name: Option<&'a str>, depth: u8) -> ToolCtx<'a> {
        ToolCtx {
            config: &host.config,
            agents: host,
            agent_id: "Main",
            def_name,
            depth,
            is_subagent: def_name.is_some(),
            cwd: None,
            decisions: Some(BTreeMap::new()),
        }
    }

    #[tokio::test]
    async fn spawn_flat单发sync走通并产出结果() {
        let h = host();
        let ctx = ctx_for(&h, None, 0);
        let out = SpawnTool
            .execute_ctx(
                &ctx,
                &json!({ "task": "调研仓库结构", "agent": "scout", "sync": true }),
            )
            .await;
        assert!(
            out.output.contains("1/1 成功"),
            "同步单发应成功：{}",
            out.output
        );
        assert!(
            out.output.contains("mock 子代理完成"),
            "结果文本：{}",
            out.output
        );
        assert!(
            out.output.contains("agent://"),
            "应给出 agent:// 句柄：{}",
            out.output
        );
    }

    #[tokio::test]
    async fn spawn_自递归拦截() {
        let h = host();
        // A task-type sub-agent dispatching another task agent directly → intercepted.
        let ctx = ctx_for(&h, Some("task"), 1);
        let out = SpawnTool
            .execute_ctx(&ctx, &json!({ "task": "x", "agent": "task", "sync": true }))
            .await;
        assert!(
            out.output.contains("自递归拦截"),
            "应拦截自递归：{}",
            out.output
        );
        assert_eq!(out.exit_code, Some(1));
        // dispatching a *different* type is fine
        let out2 = SpawnTool
            .execute_ctx(
                &ctx,
                &json!({ "task": "x", "agent": "scout", "sync": true }),
            )
            .await;
        assert!(
            out2.output.contains("1/1 成功"),
            "异类型派发应放行：{}",
            out2.output
        );
    }

    #[tokio::test]
    async fn spawn_深度防御拒绝() {
        let h = host();
        let ctx = ctx_for(&h, Some("task"), 2); // max_recursion_depth = 2
        let out = SpawnTool
            .execute_ctx(&ctx, &json!({ "task": "x", "sync": true }))
            .await;
        assert!(
            out.output.contains("递归上限"),
            "深度 2 应拒绝：{}",
            out.output
        );
    }

    #[tokio::test]
    async fn spawn_未知agent类型与缺参数() {
        let h = host();
        let ctx = ctx_for(&h, None, 0);
        let out = SpawnTool
            .execute_ctx(&ctx, &json!({ "task": "x", "agent": "nope" }))
            .await;
        assert!(
            out.output.contains("未知 agent 类型"),
            "应报未知类型：{}",
            out.output
        );
        let out2 = SpawnTool
            .execute_ctx(&ctx, &json!({ "context": "无任务" }))
            .await;
        assert!(
            out2.output.contains("参数不完整"),
            "缺 task/tasks 应报错：{}",
            out2.output
        );
    }

    #[tokio::test]
    async fn spawn_async派发后结果自动投递() {
        let h = host();
        let ctx = ctx_for(&h, None, 0);
        let out = SpawnTool
            .execute_ctx(&ctx, &json!({ "task": "后台做点事", "agent": "scout" }))
            .await;
        assert!(
            out.output.contains("已派发 1 个后台子代理任务"),
            "应立即返回派发回执：{}",
            out.output
        );
        let mut pending: Vec<(String, String)> = Vec::new();
        for _ in 0..100 {
            pending = h.take_pending();
            if !pending.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            pending[0].1.contains("mock 子代理完成"),
            "投递内容：{}",
            pending[0].1
        );
    }

    #[tokio::test]
    async fn spawn_batch解析与batch上下文下发() {
        let h = host();
        let ctx = ctx_for(&h, None, 0);
        let out = SpawnTool
            .execute_ctx(
                &ctx,
                &json!({
                    "context": "共享背景：仓库在当前目录",
                    "tasks": [
                        { "task": "项A", "agent": "scout" },
                        { "task": "项B", "agent": "scout" }
                    ],
                    "sync": true
                }),
            )
            .await;
        assert!(
            out.output.contains("2/2 成功"),
            "batch 两项应都成功：{}",
            out.output
        );
        // the shared context reached the transcripts
        let roster = h.roster();
        assert_eq!(
            roster["agents"].as_array().unwrap().len(),
            3,
            "Main + 2 子代理"
        );
    }

    #[tokio::test]
    async fn spawn_sync信号量限流峰值不超上限() {
        // Delayed mock providers expose the in-flight peak; maxConcurrency=1 must cap it at 1.
        crate::llm::mock_hooks::reset_peak();
        let cfg = {
            let mut c = Config::default();
            c.task.max_concurrency = Some(1);
            c
        };
        let h = Arc::new(AgentHost::new(
            Arc::new(cfg),
            Arc::new(|_h: Option<&str>| AnyProvider::MockSubagent(MockSubagent { delay_ms: 120 })),
        ));
        let ctx = ctx_for(&h, None, 0);
        let out = SpawnTool
            .execute_ctx(
                &ctx,
                &json!({
                    "tasks": [
                        { "task": "慢任务1", "agent": "scout" },
                        { "task": "慢任务2", "agent": "scout" },
                        { "task": "慢任务3", "agent": "scout" }
                    ],
                    "sync": true
                }),
            )
            .await;
        assert!(
            out.output.contains("3/3 成功"),
            "限流下三项应都完成：{}",
            out.output
        );
        assert!(
            crate::llm::mock_hooks::peak() <= 1,
            "maxConcurrency=1 时并发峰值应为 1，实际 {}",
            crate::llm::mock_hooks::peak()
        );
    }

    #[tokio::test]
    async fn yield_顶层调用被拒绝() {
        let out = YieldTool.execute(&json!({ "result": "x" })).await;
        assert!(
            out.output.contains("子代理出口"),
            "顶层 yield 应被拒：{}",
            out.output
        );
    }

    #[test]
    fn parse_items_形态解析() {
        let (items, ctx) = parse_items(&json!({ "task": "t", "agent": "scout" })).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].agent, "scout");
        assert!(ctx.is_none(), "flat 无 batch 上下文");
        let (items2, ctx2) = parse_items(&json!({
            "context": "c",
            "tasks": [ { "task": "a" }, { "task": "b", "isolated": true } ]
        }))
        .unwrap();
        assert_eq!(items2.len(), 2);
        assert_eq!(items2[0].agent, "task", "默认 task");
        assert!(items2[1].isolated);
        assert_eq!(ctx2.as_deref(), Some("c"));
        assert!(parse_items(&json!({ "sync": true })).is_none());
    }
}
