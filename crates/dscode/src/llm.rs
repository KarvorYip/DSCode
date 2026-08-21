//! LLM provider seam: three HTTP wire formats plus deterministic mock providers.

use serde_json::Value;

/// Conversation message (in-memory model, one-to-one with DeepSeek API roles).
#[derive(Clone, Debug)]
pub enum Message {
    System(String),
    User(String),
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

fn message_chars(message: &Message) -> usize {
    match message {
        Message::System(content) | Message::User(content) => content.chars().count(),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            content.chars().count()
                + tool_calls
                    .iter()
                    .map(|call| call.name.chars().count() + call.arguments.chars().count())
                    .sum::<usize>()
        }
        Message::Tool { content, .. } => content.chars().count(),
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Streaming callback event: text delta, a complete tool_call (emitted once after arguments are assembled),
/// or the request's final usage (total tokens billed, emitted at most once per request; goal token budget accounting).
pub enum ChatEvent {
    Delta(String),
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    Usage {
        total_tokens: u64,
    },
}

pub trait LlmProvider {
    async fn chat_stream(
        &mut self,
        messages: &[Message],
        // 模型可见的工具 schema 列表（来自工具注册表）。
        tools: &[Value],
        on_event: &mut (dyn FnMut(ChatEvent) + Send),
    ) -> Result<(), String>;

    /// 非流式一次性补全：标题生成、compaction 摘要等廉价副任务用。
    async fn complete(&mut self, system: &str, user: &str) -> Result<String, String>;

    async fn reload_config(&mut self, _config: &crate::config::Config) -> Result<bool, String> {
        Ok(false)
    }

    fn provider_name(&self) -> &str {
        "mock"
    }

    fn model_name(&self) -> &str {
        "mock"
    }

    /// Streaming request with a forced tool choice (tools.zh.md §3.8: after three yield reminders the
    /// sub-agent loop forces toolChoice=yield). None falls back to the plain chat_stream path;
    /// providers without tool-choice support ignore the forced name (the default).
    async fn chat_stream_with_choice(
        &mut self,
        messages: &[Message],
        tools: &[Value],
        forced_tool: Option<&str>,
        on_event: &mut (dyn FnMut(ChatEvent) + Send),
    ) -> Result<(), String> {
        let _ = forced_tool;
        self.chat_stream(messages, tools, on_event).await
    }
}

/// Mock: scripted multi-tool loop (write → read → bash → final answer), for automated acceptance.
pub struct Mock;

impl LlmProvider for Mock {
    async fn chat_stream(
        &mut self,
        messages: &[Message],
        _tools: &[Value],
        on_event: &mut (dyn FnMut(ChatEvent) + Send),
    ) -> Result<(), String> {
        let tool_rounds = messages
            .iter()
            .filter(|m| matches!(m, Message::Tool { .. }))
            .count();
        match tool_rounds {
            0 => {
                on_event(ChatEvent::Delta(
                    "好的，我先用 write 工具写一个演示文件。".into(),
                ));
                on_event(ChatEvent::ToolCall {
                    id: "call_mock_1".into(),
                    name: "write".into(),
                    arguments: r#"{"path":"mock-demo.txt","content":"DSCode mock 写入验证\n"}"#
                        .into(),
                });
            }
            1 => {
                on_event(ChatEvent::Delta("写入完成，现在用 read 读回验证。".into()));
                on_event(ChatEvent::ToolCall {
                    id: "call_mock_2".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"mock-demo.txt"}"#.into(),
                });
            }
            2 => {
                on_event(ChatEvent::Delta("读回一致，最后用 bash 确认环境。".into()));
                on_event(ChatEvent::ToolCall {
                    id: "call_mock_3".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"echo dscode-mock-ok"}"#.into(),
                });
            }
            _ => {
                let outputs: Vec<&str> = messages
                    .iter()
                    .filter_map(|m| match m {
                        Message::Tool { content, .. } => Some(content.as_str()),
                        _ => None,
                    })
                    .collect();
                on_event(ChatEvent::Delta(format!(
                    "三个工具结果：{}\n多工具回路验证通过，DSCode 工具面工作正常。",
                    outputs.join(" | ")
                )));
            }
        }
        // Deterministic estimate (total chars ≈ tokens) so the goal token-budget path is testable under mock.
        let est: usize = messages.iter().map(message_chars).sum();
        on_event(ChatEvent::Usage {
            total_tokens: est as u64,
        });
        Ok(())
    }

    async fn complete(&mut self, _system: &str, user: &str) -> Result<String, String> {
        let head: String = user.chars().take(24).collect();
        Ok(format!("（mock 补全）{head}"))
    }
}

/// Scripted limit-error mock (limits.zh.md acceptance): each chat_stream request pops the
/// next scripted error string until the script empties; afterwards requests succeed with
/// a plain final answer. `requests` counts every call (probe-count assertions).
pub struct MockQuota {
    pub script: Vec<String>,
    pub requests: u32,
}

impl LlmProvider for MockQuota {
    async fn chat_stream(
        &mut self,
        messages: &[Message],
        _tools: &[Value],
        on_event: &mut (dyn FnMut(ChatEvent) + Send),
    ) -> Result<(), String> {
        self.requests += 1;
        if !self.script.is_empty() {
            return Err(self.script.remove(0));
        }
        on_event(ChatEvent::Delta("限额已恢复，继续回复。".into()));
        let est: usize = messages.iter().map(message_chars).sum();
        on_event(ChatEvent::Usage {
            total_tokens: est as u64,
        });
        Ok(())
    }

    async fn complete(&mut self, _system: &str, user: &str) -> Result<String, String> {
        Ok(format!("（mock 补全）{user}"))
    }
}

mod wire;

pub use wire::{HttpProvider, WireFormat};

/// Mock sub-agent provider: scripted yield flow for --mock acceptance of the spawn path.
/// Round 1 emits plain text without a tool call (exercising the yield-reminder path);
/// from round 2 on it calls `yield` with a fixed result. `delay_ms` simulates work —
/// semaphore-concurrency tests observe the in-flight peak through `mock_hooks`.
pub struct MockSubagent {
    /// Simulated per-request latency; 0 = immediate.
    pub delay_ms: u64,
}

impl Default for MockSubagent {
    fn default() -> Self {
        Self { delay_ms: 0 }
    }
}

impl LlmProvider for MockSubagent {
    async fn chat_stream(
        &mut self,
        messages: &[Message],
        _tools: &[Value],
        on_event: &mut (dyn FnMut(ChatEvent) + Send),
    ) -> Result<(), String> {
        if self.delay_ms > 0 {
            let _in_flight = mock_hooks::track_in_flight();
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        }
        let rounds = messages
            .iter()
            .filter(|m| matches!(m, Message::Assistant { .. }))
            .count();
        if rounds == 0 {
            on_event(ChatEvent::Delta("我先梳理一下任务要求。".into()));
        } else {
            on_event(ChatEvent::Delta("处理完成，调用 yield 提交结果。".into()));
            on_event(ChatEvent::ToolCall {
                id: format!("call_mock_yield_{rounds}"),
                name: "yield".into(),
                arguments: r#"{"result":"mock 子代理完成：已按指示处理任务"}"#.into(),
            });
        }
        Ok(())
    }

    async fn complete(&mut self, _system: &str, user: &str) -> Result<String, String> {
        let head: String = user.chars().take(24).collect();
        Ok(format!("（mock 子代理补全）{head}"))
    }
}

/// Concurrency observation hooks for MockSubagent (semaphore tests in tool/spawn.rs assert
/// the in-flight peak under task.maxConcurrency). Resides in production code because
/// MockSubagent::chat_stream itself tracks in-flight requests; peak()/reset_peak() are test-only.
pub(crate) mod mock_hooks {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CONCURRENT: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);

    /// Guard tracking one in-flight request; decrements on drop.
    pub struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            CONCURRENT.fetch_sub(1, Ordering::SeqCst);
        }
    }

    pub fn track_in_flight() -> Guard {
        let n = CONCURRENT.fetch_add(1, Ordering::SeqCst) + 1;
        PEAK.fetch_max(n, Ordering::SeqCst);
        Guard
    }

    pub fn peak() -> usize {
        PEAK.load(Ordering::SeqCst)
    }

    pub fn reset_peak() {
        PEAK.store(0, Ordering::SeqCst);
        CONCURRENT.store(0, Ordering::SeqCst);
    }
}

pub struct InvalidProvider {
    pub model: String,
    pub error: String,
}

impl LlmProvider for InvalidProvider {
    async fn chat_stream(
        &mut self,
        _messages: &[Message],
        _tools: &[Value],
        _on_event: &mut (dyn FnMut(ChatEvent) + Send),
    ) -> Result<(), String> {
        Err(self.error.clone())
    }

    async fn complete(&mut self, _system: &str, _user: &str) -> Result<String, String> {
        Err(self.error.clone())
    }
}

/// Enum dispatch, avoiding async trait objectification.
pub enum AnyProvider {
    Mock(Mock),
    MockSubagent(MockSubagent),
    Http(HttpProvider),
    Invalid(InvalidProvider),
}

impl AnyProvider {
    pub fn model_name(&self) -> &str {
        match self {
            AnyProvider::Mock(_) => "mock",
            AnyProvider::MockSubagent(_) => "mock-agent",
            AnyProvider::Http(p) => &p.model,
            AnyProvider::Invalid(p) => &p.model,
        }
    }

    pub fn configured(config: &crate::config::Config, role: &str, fallback: &str) -> Self {
        match config.resolve_model(role, fallback) {
            Ok(resolved) => Self::Http(HttpProvider::new(
                resolved.provider,
                resolved.config.api,
                resolved.config.base_url,
                resolved.config.api_key,
                resolved.model,
            )),
            Err(error) => Self::Invalid(InvalidProvider {
                model: role.to_string(),
                error,
            }),
        }
    }

    pub fn configured_model(config: &crate::config::Config, model: &str, source: &str) -> Self {
        match config.resolve_model_value(model, source) {
            Ok(resolved) => Self::Http(HttpProvider::new(
                resolved.provider,
                resolved.config.api,
                resolved.config.base_url,
                resolved.config.api_key,
                resolved.model,
            )),
            Err(error) => Self::Invalid(InvalidProvider {
                model: model.to_string(),
                error,
            }),
        }
    }
}

impl LlmProvider for AnyProvider {
    fn provider_name(&self) -> &str {
        match self {
            AnyProvider::Mock(_) => "mock",
            AnyProvider::MockSubagent(_) => "mock",
            AnyProvider::Http(provider) => provider.provider_name(),
            AnyProvider::Invalid(_) => "invalid",
        }
    }

    fn model_name(&self) -> &str {
        AnyProvider::model_name(self)
    }

    async fn chat_stream(
        &mut self,
        messages: &[Message],
        tools: &[Value],
        on_event: &mut (dyn FnMut(ChatEvent) + Send),
    ) -> Result<(), String> {
        match self {
            AnyProvider::Mock(p) => p.chat_stream(messages, tools, on_event).await,
            AnyProvider::MockSubagent(p) => p.chat_stream(messages, tools, on_event).await,
            AnyProvider::Http(p) => p.chat_stream(messages, tools, on_event).await,
            AnyProvider::Invalid(p) => Err(p.error.clone()),
        }
    }

    // Explicit forwarding keeps each wire format's forced tool-choice encoding.
    async fn chat_stream_with_choice(
        &mut self,
        messages: &[Message],
        tools: &[Value],
        forced_tool: Option<&str>,
        on_event: &mut (dyn FnMut(ChatEvent) + Send),
    ) -> Result<(), String> {
        match self {
            AnyProvider::Mock(p) => {
                p.chat_stream_with_choice(messages, tools, forced_tool, on_event)
                    .await
            }
            AnyProvider::MockSubagent(p) => {
                p.chat_stream_with_choice(messages, tools, forced_tool, on_event)
                    .await
            }
            AnyProvider::Http(p) => {
                p.chat_stream_with_choice(messages, tools, forced_tool, on_event)
                    .await
            }
            AnyProvider::Invalid(p) => Err(p.error.clone()),
        }
    }

    async fn complete(&mut self, system: &str, user: &str) -> Result<String, String> {
        match self {
            AnyProvider::Mock(p) => p.complete(system, user).await,
            AnyProvider::MockSubagent(p) => p.complete(system, user).await,
            AnyProvider::Http(p) => p.complete(system, user).await,
            AnyProvider::Invalid(p) => Err(p.error.clone()),
        }
    }

    async fn reload_config(&mut self, config: &crate::config::Config) -> Result<bool, String> {
        if matches!(self, AnyProvider::Mock(_) | AnyProvider::MockSubagent(_)) {
            return Ok(false);
        }
        *self = AnyProvider::configured(config, "default", "deepseek-v4-flash");
        Ok(true)
    }
}
