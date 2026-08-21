//! Sub-agent runner: the child agent loop (tools.zh.md §3.8).
//! Approval inside a child is forced yolo (approval.zh.md §2.10): config-layer rules and the
//! session user-decision snapshot stay effective; prompt-class and critical escalations resolve
//! to denial — no reviewer, no human cards. The hidden `yield` tool is the only legal exit;
//! after three reminders the loop forces toolChoice=yield at the request layer.

use super::isolation;
use super::schema::{self, SchemaMode};
use super::{truncate_chars, AgentDefinition, AgentHost};
use crate::approval::{self, Mode, UserDecisions};
use crate::llm::{ChatEvent, LlmProvider, Message, ToolCall};
use crate::tool::{Registry, ToolCtx};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Round cap: a child that cannot finish in 25 model rounds is aborted (no infinite burn).
pub const MAX_ROUNDS: usize = 25;

pub struct SpawnSpec {
    pub def: AgentDefinition,
    pub parent_id: String,
    pub context: String,
    pub task: String,
    pub output_schema: Option<Value>,
    pub schema_mode: SchemaMode,
    pub isolated: bool,
    /// Depth of the child itself (top-level dispatch → 1).
    pub depth: u8,
    /// advisor: the result never enters the main conversation flow (spawn.rs filters delivery).
    pub deliver: bool,
    pub async_mode: bool,
    /// Session-tier remembered-decision snapshot (§2.10: user deny stays effective in children).
    pub decisions: BTreeMap<String, bool>,
}

pub struct AgentOutcome {
    pub agent_id: String,
    pub ok: bool,
    pub text: String,
    pub artifact: PathBuf,
    pub patch: Option<PathBuf>,
}

/// Which tools a child at `depth` sees (pure; unit-tested): `yield` always present (the only
/// exit); `spawn` stripped at the recursion cap; the agent's own `tools` list filters further.
pub fn child_tool_names(def: &AgentDefinition, depth: u8, max_depth: u8) -> Vec<String> {
    let registry = Registry::builtin();
    let allow_spawn = def.spawns && depth < max_depth;
    registry
        .iter()
        .filter(|t| t.name() != "spawn" || allow_spawn)
        .filter(|t| match &def.tools {
            None => true,
            Some(list) => list.iter().any(|n| n == t.name()) || t.name() == "yield",
        })
        .map(|t| t.name().to_string())
        .collect()
}

fn build_system_prompt(def: &AgentDefinition, spec: &SpawnSpec) -> String {
    let mut s = String::new();
    s.push_str(&def.system_prompt);
    if let Some(output) = &def.output {
        s.push_str("\n\n产出要求：");
        s.push_str(output);
    }
    if let Some(schema_value) = &spec.output_schema {
        let mode_text = match spec.schema_mode {
            SchemaMode::Strict => "严格校验：不符合将被拒绝并要求重新提交",
            SchemaMode::Permissive => "宽松校验：不符合将附警告接受",
        };
        s.push_str(&format!(
            "\n\n产出契约：yield 的 result 字段必须符合以下 JSON Schema（{mode_text}）：\n{}",
            serde_json::to_string_pretty(schema_value).unwrap_or_default()
        ));
    }
    s.push_str("\n\n出口契约：完成后必须调用工具 yield 提交最终结果（参数 {\"result\": ...}）。这是子代理唯一合法出口；不要用纯文本结束回合。");
    s
}

/// Production entry: provider comes from the host factory (Mock in --mock, DeepSeek live).
pub async fn run_agent(host: Arc<AgentHost>, spec: SpawnSpec) -> AgentOutcome {
    let provider = host.make_provider(spec.def.model.as_deref());
    run_agent_with(host, spec, provider).await
}

/// Generic loop core; `run_agent_with` is also the test seam for scripted providers.
pub async fn run_agent_with<P: LlmProvider>(
    host: Arc<AgentHost>,
    spec: SpawnSpec,
    mut provider: P,
) -> AgentOutcome {
    let config = host.config();
    let agent_id = host.register_agent(&spec.def.name);
    host.push_event(
        "agent/spawned",
        json!({
            "agentId": agent_id,
            "def": spec.def.name,
            "parent": spec.parent_id,
            "task": truncate_chars(&spec.task, 200),
            "isolated": spec.isolated,
            "async": spec.async_mode,
            "depth": spec.depth,
        }),
    );

    // Worktree isolation (git backend only in the first release).
    let worktree: Option<PathBuf> = if spec.isolated {
        match std::env::current_dir() {
            Ok(repo_root) => match isolation::setup(&repo_root, &agent_id) {
                Ok(wt) => Some(wt),
                Err(e) => {
                    return fail_outcome(&host, &spec, &agent_id, format!("worktree 隔离失败：{e}"))
                }
            },
            Err(e) => return fail_outcome(&host, &spec, &agent_id, format!("定位仓库根失败：{e}")),
        }
    } else {
        None
    };

    // Child tool set: spawn stripped at the cap; def filter; yield always kept.
    let mut registry = Registry::builtin();
    registry.extend_shared(&host.shared_tools().snapshot());
    let allow_spawn = spec.def.spawns && spec.depth < config.task.max_recursion_depth;
    let tools: Vec<Value> = registry
        .child_definitions(allow_spawn)
        .into_iter()
        .filter(|d| {
            let name = d["function"]["name"].as_str().unwrap_or_default();
            match &spec.def.tools {
                None => true,
                Some(list) => list.iter().any(|n| n == name) || name == "yield",
            }
        })
        .collect();

    let user_msg = if spec.context.is_empty() {
        spec.task.clone()
    } else {
        format!("## 上下文\n{}\n\n## 任务\n{}", spec.context, spec.task)
    };
    let mut messages = vec![
        Message::System(build_system_prompt(&spec.def, &spec)),
        Message::User(user_msg.clone()),
    ];
    host.transcript_push(&agent_id, &format!("user: {user_msg}"));

    let mut reminders: u32 = 0;
    let mut forced = false;
    let mut last_content = String::new();

    for _round in 0..MAX_ROUNDS {
        let mut content = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        {
            let mut on_event = |ev: ChatEvent| match ev {
                ChatEvent::Delta(t) => content.push_str(&t),
                ChatEvent::ToolCall {
                    id,
                    name,
                    arguments,
                } => tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments,
                }),
                ChatEvent::Usage { .. } => {}
            };
            let choice = if forced { Some("yield") } else { None };
            if let Err(e) = provider
                .chat_stream_with_choice(&messages, &tools, choice, &mut on_event)
                .await
            {
                return finalize(
                    &host,
                    &spec,
                    &agent_id,
                    worktree.as_deref(),
                    false,
                    format!("子代理请求失败：{e}"),
                    Value::Null,
                );
            }
        }
        last_content = content.clone();
        host.transcript_push(&agent_id, &format!("assistant: {content}"));
        messages.push(Message::Assistant {
            content: content.clone(),
            tool_calls: tool_calls.clone(),
        });

        if tool_calls.is_empty() {
            // No yield yet: remind (three times), then force toolChoice=yield on later requests.
            reminders += 1;
            let reminder = if reminders > 3 {
                forced = true;
                format!("第 {reminders} 次提醒：必须立即调用 yield 工具提交结果（{{\"result\": ...}}），本次请求已强制 yield。")
            } else {
                format!(
                    "提醒 {reminders}/3：请调用 yield 工具提交最终结果；纯文本结束不是合法出口。"
                )
            };
            host.transcript_push(&agent_id, &format!("system: {reminder}"));
            messages.push(Message::User(reminder));
            continue;
        }

        for call in &tool_calls {
            host.transcript_push(
                &agent_id,
                &format!(
                    "tool {}: {}",
                    call.name,
                    truncate_chars(&call.arguments, 200)
                ),
            );
            let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);

            if call.name == "yield" {
                match handle_yield(&args, spec.output_schema.as_ref(), spec.schema_mode) {
                    YieldVerdict::Accept(result, warning) => {
                        let mut text = render_result(&result);
                        if let Some(w) = warning {
                            text = format!("（警告：产出不符合 outputSchema：{w}）\n{text}");
                        }
                        return finalize(
                            &host,
                            &spec,
                            &agent_id,
                            worktree.as_deref(),
                            true,
                            text,
                            result,
                        );
                    }
                    YieldVerdict::Reject(reason) => {
                        let out = format!("yield 被拒绝：{reason}。请修正后重新调用 yield。");
                        host.transcript_push(&agent_id, &format!("result: {out}"));
                        messages.push(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: out,
                        });
                        continue;
                    }
                }
            }

            // Child approval gate: forced yolo (§2.10) — config rules + user-decision snapshot
            // stay effective; deny and every prompt/critical escalation resolve to denial.
            let tier = registry
                .get(&call.name)
                .map(|t| t.tier())
                .unwrap_or(crate::tool::Tier::Exec);
            let user_decisions = UserDecisions::from_map(spec.decisions.clone());
            let outcome = approval::decide(
                &call.name,
                tier,
                &args,
                &config.rules,
                Mode::Yolo,
                &user_decisions,
            );
            use approval::ChainAction;
            let result = match outcome {
                ChainAction::Allow(_) => {
                    let child_ctx = ToolCtx {
                        config: &config,
                        agents: &host,
                        agent_id: &agent_id,
                        def_name: Some(&spec.def.name),
                        depth: spec.depth,
                        is_subagent: true,
                        cwd: worktree.as_deref(),
                        decisions: Some(spec.decisions.clone()),
                    };
                    match registry.get(&call.name) {
                        Some(tool) => tool.execute_ctx(&child_ctx, &args).await.output,
                        None => format!("未知工具：{}", call.name),
                    }
                }
                ChainAction::Deny(step) => {
                    format!("子代理审批拒绝（规则 {}）：deny 规则硬生效", step.as_str())
                }
                ChainAction::EscalateHuman(step) => format!(
                    "子代理审批拒绝（{}）：该操作需人工确认；子代理无 UI，fail-closed 拒绝（approval.zh.md §2.10）",
                    step.as_str()
                ),
                // Unreachable under Mode::Yolo; kept exhaustive and fail-closed.
                ChainAction::AutoReview(_) => "子代理审批拒绝：子代理绝不发起代审请求".to_string(),
            };
            host.transcript_push(
                &agent_id,
                &format!("result: {}", truncate_chars(&result, 300)),
            );
            messages.push(Message::Tool {
                tool_call_id: call.id.clone(),
                content: result,
            });
        }
    }

    finalize(
        &host,
        &spec,
        &agent_id,
        worktree.as_deref(),
        false,
        format!(
            "子代理超过最大轮次（{MAX_ROUNDS}）未调用 yield。最后的输出：\n{}",
            truncate_chars(&last_content, 1000)
        ),
        Value::Null,
    )
}

enum YieldVerdict {
    Accept(Value, Option<String>),
    Reject(String),
}

fn handle_yield(args: &Value, schema: Option<&Value>, mode: SchemaMode) -> YieldVerdict {
    let Some(result) = args.get("result") else {
        return YieldVerdict::Reject("缺少 result 字段（{\"result\": ...}）".into());
    };
    if result.is_null() {
        return YieldVerdict::Reject("result 不能为空".into());
    }
    if let Some(schema) = schema {
        if let Err(e) = schema::validate(result, schema) {
            return match mode {
                SchemaMode::Strict => YieldVerdict::Reject(e),
                SchemaMode::Permissive => YieldVerdict::Accept(result.clone(), Some(e)),
            };
        }
    }
    YieldVerdict::Accept(result.clone(), None)
}

fn render_result(result: &Value) -> String {
    match result {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// Write artifacts + patch, emit agent/completed, flip the lifecycle state.
fn finalize(
    host: &Arc<AgentHost>,
    spec: &SpawnSpec,
    agent_id: &str,
    worktree: Option<&std::path::Path>,
    ok: bool,
    text: String,
    result: Value,
) -> AgentOutcome {
    let artifact_dir = host.artifacts_dir().join(agent_id);
    let artifact = if let Err(e) = std::fs::create_dir_all(&artifact_dir) {
        Err(format!("创建产物目录失败：{e}"))
    } else {
        match &result {
            Value::String(s) => std::fs::write(artifact_dir.join("output.txt"), s)
                .map(|_| artifact_dir.join("output.txt"))
                .map_err(|e| format!("写产物失败：{e}")),
            other => std::fs::write(
                artifact_dir.join("output.json"),
                serde_json::to_string_pretty(other).unwrap_or_default(),
            )
            .map(|_| artifact_dir.join("output.json"))
            .map_err(|e| format!("写产物失败：{e}")),
        }
    };
    let artifact = artifact.unwrap_or_else(|e| {
        eprintln!("[agent] {agent_id} 产物写入失败：{e}（结果仅在结果文本中保留）");
        artifact_dir.join("output.txt")
    });

    let patch = worktree.and_then(|wt| {
        match isolation::finalize(wt, &artifact_dir.join("changes.patch")) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[agent] {agent_id} patch 生成失败：{e}");
                None
            }
        }
    });

    host.push_event(
        "agent/completed",
        json!({
            "agentId": agent_id,
            "def": spec.def.name,
            "ok": ok,
            "result": truncate_chars(&text, 200),
            "artifact": format!("agent://{agent_id}"),
            "patch": patch.as_ref().map(|p| p.display().to_string()),
        }),
    );
    host.complete_agent(agent_id, ok);
    AgentOutcome {
        agent_id: agent_id.to_string(),
        ok,
        text,
        artifact,
        patch,
    }
}

fn fail_outcome(
    host: &Arc<AgentHost>,
    spec: &SpawnSpec,
    agent_id: &str,
    text: String,
) -> AgentOutcome {
    // Pre-loop failure: still emits the completed event and flips the lifecycle state.
    host.push_event(
        "agent/completed",
        json!({
            "agentId": agent_id,
            "def": spec.def.name,
            "ok": false,
            "result": truncate_chars(&text, 200),
            "artifact": format!("agent://{agent_id}"),
            "patch": null,
        }),
    );
    host.complete_agent(agent_id, false);
    AgentOutcome {
        agent_id: agent_id.to_string(),
        ok: false,
        text,
        artifact: host.artifacts_dir().join(agent_id),
        patch: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::bundled;
    use crate::config::Config;
    use crate::llm::AnyProvider;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn host() -> Arc<AgentHost> {
        Arc::new(AgentHost::new(
            Arc::new(Config::default()),
            Arc::new(|_h: Option<&str>| {
                AnyProvider::MockSubagent(crate::llm::MockSubagent::default())
            }),
        ))
    }

    fn spec(def: AgentDefinition) -> SpawnSpec {
        SpawnSpec {
            def,
            parent_id: "Main".into(),
            context: "测试上下文".into(),
            task: "做点什么".into(),
            output_schema: None,
            schema_mode: SchemaMode::Strict,
            isolated: false,
            depth: 1,
            deliver: true,
            async_mode: false,
            decisions: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn runner_mock子代理经提醒后yield() {
        let host = host();
        let out = run_agent(host.clone(), spec(bundled()[1].clone())).await;
        assert!(out.ok, "mock 子代理应成功：{}", out.text);
        assert!(
            out.text.contains("mock 子代理完成"),
            "结果文本：{}",
            out.text
        );
        // events: spawned + completed
        let evs = host.drain_events();
        let kinds: Vec<&str> = evs.iter().map(|(k, _)| k.as_str()).collect();
        assert!(kinds.contains(&"agent/spawned"));
        assert!(kinds.contains(&"agent/completed"));
        // transcript recorded the reminder round
        let t = host.history_text(&out.agent_id).unwrap();
        assert!(t.contains("提醒 1/3"), "转录应含提醒：{t}");
    }

    /// Scripted provider: emits N plain-text rounds, then yields `payload` on every later round.
    struct ScriptedYield {
        plain_rounds: usize,
        payload: Value,
    }

    impl LlmProvider for ScriptedYield {
        async fn chat_stream(
            &mut self,
            messages: &[Message],
            _tools: &[Value],
            on_event: &mut (dyn FnMut(ChatEvent) + Send),
        ) -> Result<(), String> {
            let rounds = messages
                .iter()
                .filter(|m| matches!(m, Message::Assistant { .. }))
                .count();
            if rounds < self.plain_rounds {
                on_event(ChatEvent::Delta(format!("纯文本第 {rounds} 轮")));
            } else {
                on_event(ChatEvent::ToolCall {
                    id: format!("call_{rounds}"),
                    name: "yield".into(),
                    arguments: json!({ "result": self.payload }).to_string(),
                });
            }
            Ok(())
        }
        async fn complete(&mut self, _s: &str, _u: &str) -> Result<String, String> {
            Ok(String::new())
        }
    }

    /// Wraps ScriptedYield and records forced-tool-choice requests (tools.zh.md §3.8 exit forcing).
    struct ChoiceRecorder {
        inner: ScriptedYield,
        forced_seen: Arc<AtomicUsize>,
    }

    impl LlmProvider for ChoiceRecorder {
        async fn chat_stream(
            &mut self,
            messages: &[Message],
            tools: &[Value],
            on_event: &mut (dyn FnMut(ChatEvent) + Send),
        ) -> Result<(), String> {
            self.inner.chat_stream(messages, tools, on_event).await
        }
        async fn chat_stream_with_choice(
            &mut self,
            messages: &[Message],
            tools: &[Value],
            forced_tool: Option<&str>,
            on_event: &mut (dyn FnMut(ChatEvent) + Send),
        ) -> Result<(), String> {
            if forced_tool == Some("yield") {
                self.forced_seen.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.chat_stream(messages, tools, on_event).await
        }
        async fn complete(&mut self, _s: &str, _u: &str) -> Result<String, String> {
            Ok(String::new())
        }
    }

    #[tokio::test]
    async fn runner_三次提醒后强制tool_choice_yield() {
        // 6 plain rounds: reminders 1..3, forced from the 4th on.
        let forced_seen = Arc::new(AtomicUsize::new(0));
        let provider = ChoiceRecorder {
            inner: ScriptedYield {
                plain_rounds: 6,
                payload: json!("最终结果"),
            },
            forced_seen: forced_seen.clone(),
        };
        let out = run_agent_with(host(), spec(bundled()[1].clone()), provider).await;
        assert!(out.ok);
        assert!(out.text.contains("最终结果"));
        assert!(
            forced_seen.load(Ordering::SeqCst) >= 1,
            "第 4 次提醒后应出现强制 tool_choice=yield 请求"
        );
    }

    #[tokio::test]
    async fn runner_strict_schema拒绝非法产出并重试() {
        // Yields an invalid payload first; after seeing the rejection tool message, yields valid.
        struct RetryValid;
        impl LlmProvider for RetryValid {
            async fn chat_stream(
                &mut self,
                messages: &[Message],
                _tools: &[Value],
                on_event: &mut (dyn FnMut(ChatEvent) + Send),
            ) -> Result<(), String> {
                let rejected = messages.iter().any(|m| {
                    matches!(m, Message::Tool { content, .. } if content.contains("yield 被拒绝"))
                });
                let payload = if rejected {
                    json!({ "files": ["a.rs"], "count": 1 })
                } else {
                    json!({ "count": 1 })
                };
                on_event(ChatEvent::ToolCall {
                    id: "c1".into(),
                    name: "yield".into(),
                    arguments: json!({ "result": payload }).to_string(),
                });
                Ok(())
            }
            async fn complete(&mut self, _s: &str, _u: &str) -> Result<String, String> {
                Ok(String::new())
            }
        }
        let mut sp = spec(bundled()[1].clone());
        sp.output_schema = Some(json!({
            "type": "object",
            "properties": { "files": { "type": "array", "items": { "type": "string" } } },
            "required": ["files"]
        }));
        sp.schema_mode = SchemaMode::Strict;
        let out = run_agent_with(host(), sp, RetryValid).await;
        assert!(out.ok, "重试后应成功：{}", out.text);
        assert!(
            out.text.contains("files"),
            "最终产出应是合法 JSON：{}",
            out.text
        );
    }

    #[tokio::test]
    async fn runner_permissive_schema附警告接受() {
        struct OnceInvalid;
        impl LlmProvider for OnceInvalid {
            async fn chat_stream(
                &mut self,
                _messages: &[Message],
                _tools: &[Value],
                on_event: &mut (dyn FnMut(ChatEvent) + Send),
            ) -> Result<(), String> {
                on_event(ChatEvent::ToolCall {
                    id: "c1".into(),
                    name: "yield".into(),
                    arguments: json!({ "result": { "count": 1 } }).to_string(),
                });
                Ok(())
            }
            async fn complete(&mut self, _s: &str, _u: &str) -> Result<String, String> {
                Ok(String::new())
            }
        }
        let mut sp = spec(bundled()[1].clone());
        sp.output_schema = Some(json!({ "type": "object", "required": ["files"] }));
        sp.schema_mode = SchemaMode::Permissive;
        let out = run_agent_with(host(), sp, OnceInvalid).await;
        assert!(out.ok, "permissive 应接受：{}", out.text);
        assert!(out.text.contains("警告"), "应附警告：{}", out.text);
    }

    #[tokio::test]
    async fn runner_超过轮次上限失败收尾() {
        // Never yields: 25 plain rounds → failure outcome with events + aborted state.
        let provider = ScriptedYield {
            plain_rounds: 100,
            payload: json!("x"),
        };
        let h = host();
        let out = run_agent_with(h.clone(), spec(bundled()[1].clone()), provider).await;
        assert!(!out.ok);
        assert!(out.text.contains("最大轮次"), "失败文本：{}", out.text);
        let evs = h.drain_events();
        assert!(evs
            .iter()
            .any(|(k, d)| k == "agent/completed" && d["ok"] == false));
    }

    #[test]
    fn child_tool_names_深度封顶剥离spawn且保留yield() {
        let task_def = &bundled()[1];
        // depth 1 < max 2 → spawn present
        assert!(child_tool_names(task_def, 1, 2).contains(&"spawn".to_string()));
        // depth 2 = max 2 → spawn stripped, yield kept
        let d2 = child_tool_names(task_def, 2, 2);
        assert!(
            !d2.contains(&"spawn".to_string()),
            "深度 2 的 child 无 spawn"
        );
        assert!(d2.contains(&"yield".to_string()), "yield 必须保留");
        // scout (spawns=false, read-only tool list)
        let scout = &bundled()[0];
        let s = child_tool_names(scout, 1, 2);
        assert!(!s.contains(&"spawn".to_string()));
        assert!(!s.contains(&"write".to_string()));
        assert!(s.contains(&"read".to_string()));
        assert!(s.contains(&"yield".to_string()));
        assert!(
            child_tool_names(&bundled()[1], 1, 2).contains(&"hub".to_string()),
            "task 子代理应有 hub"
        );
    }
}
