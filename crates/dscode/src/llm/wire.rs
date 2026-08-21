use super::{ChatEvent, LlmProvider, Message, ToolCall};
use futures_util::StreamExt;
use serde_json::{json, Value};

/// First-release provider wire formats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireFormat {
    #[default]
    OpenAiCompletions,
    OpenAiResponses,
    AnthropicMessages,
}

impl WireFormat {
    fn endpoint(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "chat/completions",
            Self::OpenAiResponses => "responses",
            Self::AnthropicMessages => "messages",
        }
    }
}

/// Configuration-driven HTTP provider. The turn loop only sees `LlmProvider`.
pub struct HttpProvider {
    client: reqwest::Client,
    pub provider: String,
    wire: WireFormat,
    base_url: String,
    api_key_ref: Option<String>,
    pub model: String,
}

impl HttpProvider {
    pub fn new(
        provider: String,
        wire: WireFormat,
        base_url: String,
        api_key_ref: Option<String>,
        model: String,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            provider,
            wire,
            base_url,
            api_key_ref,
            model,
        }
    }

    fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = self.wire.endpoint();
        if base.ends_with(path) {
            base.to_string()
        } else {
            format!("{base}/{path}")
        }
    }

    fn request(&self, body: &Value) -> Result<reqwest::RequestBuilder, String> {
        let api_key = self
            .api_key_ref
            .as_deref()
            .map(crate::config::resolve_credential_ref)
            .transpose()?
            .flatten();
        if self.api_key_ref.is_some() && api_key.is_none() {
            return Err(format!("{} 的 API 凭据未找到", self.provider));
        }
        let request = self.client.post(self.endpoint()).json(body);
        Ok(match (self.wire, api_key.as_deref()) {
            (WireFormat::AnthropicMessages, Some(key)) => request
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01"),
            (WireFormat::AnthropicMessages, None) => {
                request.header("anthropic-version", "2023-06-01")
            }
            (_, Some(key)) => request.bearer_auth(key),
            (_, None) => request,
        })
    }

    async fn stream_request(
        &mut self,
        messages: &[Message],
        tools: &[Value],
        forced_tool: Option<&str>,
        on_event: &mut (dyn FnMut(ChatEvent) + Send),
    ) -> Result<(), String> {
        let body = build_request(self.wire, &self.model, messages, tools, forced_tool, true);
        let resp = self
            .request(&body)?
            .send()
            .await
            .map_err(|error| format!("{} 请求失败：{error}", self.provider))?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(format!(
                "{} API {status}：{}",
                self.provider,
                resp.text().await.unwrap_or_default()
            ));
        }

        let mut decoder = SseDecoder::default();
        let mut state = StreamState::default();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| format!("读取流失败：{error}"))?;
            for event in decoder.push(&chunk) {
                if parse_stream_event(self.wire, &event, &mut state, on_event)? {
                    state.finish(on_event);
                    return Ok(());
                }
            }
        }
        for event in decoder.finish() {
            parse_stream_event(self.wire, &event, &mut state, on_event)?;
        }
        state.finish(on_event);
        Ok(())
    }
}

impl LlmProvider for HttpProvider {
    fn provider_name(&self) -> &str {
        &self.provider
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    async fn chat_stream(
        &mut self,
        messages: &[Message],
        tools: &[Value],
        on_event: &mut (dyn FnMut(ChatEvent) + Send),
    ) -> Result<(), String> {
        self.stream_request(messages, tools, None, on_event).await
    }

    async fn chat_stream_with_choice(
        &mut self,
        messages: &[Message],
        tools: &[Value],
        forced_tool: Option<&str>,
        on_event: &mut (dyn FnMut(ChatEvent) + Send),
    ) -> Result<(), String> {
        self.stream_request(messages, tools, forced_tool, on_event)
            .await
    }

    async fn complete(&mut self, system: &str, user: &str) -> Result<String, String> {
        let messages = [Message::System(system.into()), Message::User(user.into())];
        let body = build_request(self.wire, &self.model, &messages, &[], None, false);
        let resp = self
            .request(&body)?
            .send()
            .await
            .map_err(|error| format!("{} 请求失败：{error}", self.provider))?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(format!(
                "{} API {status}：{}",
                self.provider,
                resp.text().await.unwrap_or_default()
            ));
        }
        let value: Value = resp
            .json()
            .await
            .map_err(|error| format!("解析响应失败：{error}"))?;
        extract_completion(self.wire, &value).ok_or_else(|| "响应缺少文本内容".to_string())
    }
}

fn build_request(
    wire: WireFormat,
    model: &str,
    messages: &[Message],
    tools: &[Value],
    forced_tool: Option<&str>,
    stream: bool,
) -> Value {
    match wire {
        WireFormat::OpenAiCompletions => {
            let mut body = json!({
                "model": model,
                "messages": openai_messages(messages),
                "stream": stream,
            });
            if stream {
                body["stream_options"] = json!({ "include_usage": true });
            }
            if !tools.is_empty() {
                body["tools"] = json!(tools);
            }
            if let Some(name) = forced_tool {
                body["tool_choice"] = json!({ "type": "function", "function": { "name": name } });
            }
            body
        }
        WireFormat::OpenAiResponses => {
            let mut body = json!({
                "model": model,
                "input": responses_input(messages),
                "stream": stream,
            });
            if !tools.is_empty() {
                body["tools"] = Value::Array(to_responses_tools(tools));
            }
            if let Some(name) = forced_tool {
                body["tool_choice"] = json!({ "type": "function", "name": name });
            }
            body
        }
        WireFormat::AnthropicMessages => {
            let (system, messages) = anthropic_input(messages);
            let mut body = json!({
                "model": model,
                "max_tokens": 8192,
                "messages": messages,
                "stream": stream,
            });
            if !system.is_empty() {
                body["system"] = Value::String(system);
            }
            if !tools.is_empty() {
                body["tools"] = Value::Array(to_anthropic_tools(tools));
            }
            if let Some(name) = forced_tool {
                body["tool_choice"] = json!({ "type": "tool", "name": name });
            }
            body
        }
    }
}

fn openai_messages(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| match message {
            Message::System(content) => json!({ "role": "system", "content": content }),
            Message::User(content) => json!({ "role": "user", "content": content }),
            Message::Assistant {
                content,
                tool_calls,
            } if tool_calls.is_empty() => json!({ "role": "assistant", "content": content }),
            Message::Assistant {
                content,
                tool_calls,
            } => json!({
                "role": "assistant",
                "content": content,
                "tool_calls": tool_calls.iter().map(|call| json!({
                    "id": call.id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.arguments },
                })).collect::<Vec<_>>(),
            }),
            Message::Tool {
                tool_call_id,
                content,
            } => json!({ "role": "tool", "tool_call_id": tool_call_id, "content": content }),
        })
        .collect()
}

fn responses_input(messages: &[Message]) -> Vec<Value> {
    let mut input = Vec::new();
    for message in messages {
        match message {
            Message::System(content) => input.push(json!({ "role": "system", "content": content })),
            Message::User(content) => input.push(json!({ "role": "user", "content": content })),
            Message::Assistant {
                content,
                tool_calls,
            } => {
                if !content.is_empty() {
                    input.push(json!({ "role": "assistant", "content": content }));
                }
                input.extend(tool_calls.iter().map(|call| {
                    json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    })
                }));
            }
            Message::Tool {
                tool_call_id,
                content,
            } => input.push(json!({
                "type": "function_call_output",
                "call_id": tool_call_id,
                "output": content,
            })),
        }
    }
    input
}

fn anthropic_input(messages: &[Message]) -> (String, Vec<Value>) {
    let system = messages
        .iter()
        .filter_map(|message| match message {
            Message::System(content) => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut output = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::System(_) => index += 1,
            Message::User(content) => {
                output.push(json!({ "role": "user", "content": content }));
                index += 1;
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks = Vec::new();
                if !content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": content }));
                }
                blocks.extend(tool_calls.iter().map(|call| {
                    json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": serde_json::from_str::<Value>(&call.arguments)
                            .unwrap_or_else(|_| json!({})),
                    })
                }));
                output.push(json!({ "role": "assistant", "content": blocks }));
                index += 1;
            }
            Message::Tool { .. } => {
                let mut blocks = Vec::new();
                while index < messages.len() {
                    let Message::Tool {
                        tool_call_id,
                        content,
                    } = &messages[index]
                    else {
                        break;
                    };
                    blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": content,
                    }));
                    index += 1;
                }
                output.push(json!({ "role": "user", "content": blocks }));
            }
        }
    }
    (system, output)
}

fn tool_function(tool: &Value) -> &Value {
    tool.get("function").unwrap_or(tool)
}

fn to_responses_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let function = tool_function(tool);
            json!({
                "type": "function",
                "name": function["name"],
                "description": function["description"],
                "parameters": function["parameters"],
            })
        })
        .collect()
}

fn to_anthropic_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let function = tool_function(tool);
            json!({
                "name": function["name"],
                "description": function["description"],
                "input_schema": function["parameters"],
            })
        })
        .collect()
}

fn extract_completion(wire: WireFormat, value: &Value) -> Option<String> {
    match wire {
        WireFormat::OpenAiCompletions => value["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string),
        WireFormat::OpenAiResponses => value["output"]
            .as_array()?
            .iter()
            .flat_map(|item| item["content"].as_array().into_iter().flatten())
            .find_map(|content| content["text"].as_str().map(str::to_string)),
        WireFormat::AnthropicMessages => value["content"]
            .as_array()?
            .iter()
            .find_map(|content| content["text"].as_str().map(str::to_string)),
    }
}

#[derive(Debug)]
struct SseEvent {
    event: Option<String>,
    data: String,
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(String::from_utf8_lossy(&line).as_ref(), &mut events);
        }
        events
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.line(String::from_utf8_lossy(&line).as_ref(), &mut events);
        }
        self.dispatch(&mut events);
        events
    }

    fn line(&mut self, line: &str, events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(events);
        } else if let Some(value) = line.strip_prefix("event:") {
            self.event = Some(value.trim_start().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            self.data.push(value.trim_start().to_string());
        }
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        if !self.data.is_empty() {
            events.push(SseEvent {
                event: self.event.take(),
                data: std::mem::take(&mut self.data).join("\n"),
            });
        } else {
            self.event = None;
        }
    }
}

#[derive(Default)]
struct StreamState {
    pending: Vec<ToolCall>,
    usage: Option<u64>,
    anthropic_input_tokens: u64,
    finished: bool,
}

impl StreamState {
    fn tool(&mut self, index: usize) -> &mut ToolCall {
        while self.pending.len() <= index {
            self.pending.push(ToolCall {
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
        }
        &mut self.pending[index]
    }

    fn finish(&mut self, on_event: &mut (dyn FnMut(ChatEvent) + Send)) {
        if self.finished {
            return;
        }
        for call in self.pending.drain(..) {
            if !call.name.is_empty() {
                on_event(ChatEvent::ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                });
            }
        }
        if let Some(total_tokens) = self.usage.take() {
            on_event(ChatEvent::Usage { total_tokens });
        }
        self.finished = true;
    }
}

fn parse_stream_event(
    wire: WireFormat,
    event: &SseEvent,
    state: &mut StreamState,
    on_event: &mut (dyn FnMut(ChatEvent) + Send),
) -> Result<bool, String> {
    if event.data == "[DONE]" {
        return Ok(true);
    }
    let value: Value =
        serde_json::from_str(&event.data).map_err(|error| format!("解析 SSE 事件失败：{error}"))?;
    match wire {
        WireFormat::OpenAiCompletions => parse_completions_event(&value, state, on_event),
        WireFormat::OpenAiResponses => {
            parse_responses_event(event.event.as_deref(), &value, state, on_event)
        }
        WireFormat::AnthropicMessages => {
            parse_anthropic_event(event.event.as_deref(), &value, state, on_event)
        }
    }
}

fn parse_completions_event(
    value: &Value,
    state: &mut StreamState,
    on_event: &mut (dyn FnMut(ChatEvent) + Send),
) -> Result<bool, String> {
    if let Some(total) = value["usage"]["total_tokens"].as_u64() {
        state.usage = Some(total);
    }
    let delta = &value["choices"][0]["delta"];
    if let Some(text) = delta["content"].as_str().filter(|text| !text.is_empty()) {
        on_event(ChatEvent::Delta(text.to_string()));
    }
    if let Some(calls) = delta["tool_calls"].as_array() {
        for call in calls {
            let pending = state.tool(call["index"].as_u64().unwrap_or(0) as usize);
            if let Some(id) = call["id"].as_str() {
                pending.id = id.to_string();
            }
            if let Some(name) = call["function"]["name"].as_str() {
                pending.name = name.to_string();
            }
            if let Some(arguments) = call["function"]["arguments"].as_str() {
                pending.arguments.push_str(arguments);
            }
        }
    }
    Ok(false)
}

fn parse_responses_event(
    event: Option<&str>,
    value: &Value,
    state: &mut StreamState,
    on_event: &mut (dyn FnMut(ChatEvent) + Send),
) -> Result<bool, String> {
    match event.or_else(|| value["type"].as_str()) {
        Some("response.output_text.delta") => {
            if let Some(delta) = value["delta"].as_str() {
                on_event(ChatEvent::Delta(delta.to_string()));
            }
        }
        Some("response.output_item.added") | Some("response.output_item.done")
            if value["item"]["type"] == "function_call" =>
        {
            let pending = state.tool(value["output_index"].as_u64().unwrap_or(0) as usize);
            let item = &value["item"];
            if let Some(id) = item["call_id"].as_str().or_else(|| item["id"].as_str()) {
                pending.id = id.to_string();
            }
            if let Some(name) = item["name"].as_str() {
                pending.name = name.to_string();
            }
            if let Some(arguments) = item["arguments"].as_str().filter(|text| !text.is_empty()) {
                pending.arguments = arguments.to_string();
            }
        }
        Some("response.function_call_arguments.delta") => {
            let pending = state.tool(value["output_index"].as_u64().unwrap_or(0) as usize);
            if let Some(delta) = value["delta"].as_str() {
                pending.arguments.push_str(delta);
            }
        }
        Some("response.completed") => {
            state.usage = response_usage(&value["response"]["usage"]);
            return Ok(true);
        }
        Some("error") => {
            return Err(value["message"]
                .as_str()
                .unwrap_or("OpenAI Responses 流返回错误")
                .to_string())
        }
        _ => {}
    }
    Ok(false)
}

fn parse_anthropic_event(
    event: Option<&str>,
    value: &Value,
    state: &mut StreamState,
    on_event: &mut (dyn FnMut(ChatEvent) + Send),
) -> Result<bool, String> {
    match event.or_else(|| value["type"].as_str()) {
        Some("message_start") => {
            state.anthropic_input_tokens = anthropic_usage(&value["message"]["usage"]);
        }
        Some("content_block_start") if value["content_block"]["type"] == "tool_use" => {
            let pending = state.tool(value["index"].as_u64().unwrap_or(0) as usize);
            let block = &value["content_block"];
            pending.id = block["id"].as_str().unwrap_or_default().to_string();
            pending.name = block["name"].as_str().unwrap_or_default().to_string();
            if block["input"]
                .as_object()
                .is_some_and(|input| !input.is_empty())
            {
                pending.arguments = block["input"].to_string();
            }
        }
        Some("content_block_delta") if value["delta"]["type"] == "text_delta" => {
            if let Some(text) = value["delta"]["text"].as_str() {
                on_event(ChatEvent::Delta(text.to_string()));
            }
        }
        Some("content_block_delta") if value["delta"]["type"] == "input_json_delta" => {
            let pending = state.tool(value["index"].as_u64().unwrap_or(0) as usize);
            if let Some(delta) = value["delta"]["partial_json"].as_str() {
                pending.arguments.push_str(delta);
            }
        }
        Some("message_delta") => {
            state.usage = Some(state.anthropic_input_tokens + anthropic_usage(&value["usage"]));
        }
        Some("message_stop") => return Ok(true),
        Some("error") => {
            return Err(value["error"]["message"]
                .as_str()
                .unwrap_or("Anthropic 流返回错误")
                .to_string())
        }
        _ => {}
    }
    Ok(false)
}

fn response_usage(usage: &Value) -> Option<u64> {
    usage["total_tokens"]
        .as_u64()
        .or_else(|| Some(usage["input_tokens"].as_u64()? + usage["output_tokens"].as_u64()?))
}

fn anthropic_usage(usage: &Value) -> u64 {
    [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .filter_map(|key| usage[*key].as_u64())
    .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_messages() -> Vec<Message> {
        vec![
            Message::System("system".into()),
            Message::User("hello".into()),
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"a"}"#.into(),
                }],
            },
            Message::Tool {
                tool_call_id: "call_1".into(),
                content: "ok".into(),
            },
        ]
    }

    fn fixture_tools() -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "read",
                "description": "read file",
                "parameters": { "type": "object" },
            },
        })]
    }

    fn parse_fixture(wire: WireFormat, fixture: &str) -> Vec<ChatEvent> {
        let mut decoder = SseDecoder::default();
        let mut state = StreamState::default();
        let mut output = Vec::new();
        let mut emit = |event| output.push(event);
        let split = fixture.len() / 2;
        for chunk in [&fixture.as_bytes()[..split], &fixture.as_bytes()[split..]] {
            for event in decoder.push(chunk) {
                if parse_stream_event(wire, &event, &mut state, &mut emit).unwrap() {
                    state.finish(&mut emit);
                }
            }
        }
        for event in decoder.finish() {
            if parse_stream_event(wire, &event, &mut state, &mut emit).unwrap() {
                state.finish(&mut emit);
            }
        }
        state.finish(&mut emit);
        output
    }

    #[test]
    fn openai_completions_request_and_sse_fixture_round_trip() {
        let body = build_request(
            WireFormat::OpenAiCompletions,
            "deepseek-chat",
            &fixture_messages(),
            &fixture_tools(),
            Some("read"),
            true,
        );
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["name"],
            "read"
        );
        assert_eq!(body["tool_choice"]["function"]["name"], "read");
        let fixture = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\",\"tool_calls\":[{\"index\":0,\"id\":\"call_2\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"b\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"total_tokens\":12}}\n\n",
            "data: [DONE]\n\n",
        );
        let events = parse_fixture(WireFormat::OpenAiCompletions, fixture);
        assert!(matches!(&events[0], ChatEvent::Delta(text) if text == "hi"));
        assert!(
            matches!(&events[1], ChatEvent::ToolCall { id, name, arguments } if id == "call_2" && name == "read" && arguments == r#"{"path":"b"}"#)
        );
        assert!(matches!(events[2], ChatEvent::Usage { total_tokens: 12 }));
    }

    #[test]
    fn openai_responses_request_and_sse_fixture_round_trip() {
        let body = build_request(
            WireFormat::OpenAiResponses,
            "gpt-test",
            &fixture_messages(),
            &fixture_tools(),
            Some("read"),
            true,
        );
        assert_eq!(body["input"][2]["type"], "function_call");
        assert_eq!(body["input"][3]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["name"], "read");
        let fixture = concat!(
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"read\",\"arguments\":\"\"}}\n\n",
            "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"path\\\":\\\"b\\\"}\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":5}}}\n\n",
        );
        let events = parse_fixture(WireFormat::OpenAiResponses, fixture);
        assert!(matches!(&events[0], ChatEvent::Delta(text) if text == "hi"));
        assert!(
            matches!(&events[1], ChatEvent::ToolCall { id, name, arguments } if id == "call_2" && name == "read" && arguments == r#"{"path":"b"}"#)
        );
        assert!(matches!(events[2], ChatEvent::Usage { total_tokens: 12 }));
    }

    #[test]
    fn anthropic_messages_request_and_sse_fixture_round_trip() {
        let body = build_request(
            WireFormat::AnthropicMessages,
            "claude-test",
            &fixture_messages(),
            &fixture_tools(),
            Some("read"),
            true,
        );
        assert_eq!(body["system"], "system");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        let fixture = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_2\",\"name\":\"read\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"b\\\"}\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let events = parse_fixture(WireFormat::AnthropicMessages, fixture);
        assert!(matches!(&events[0], ChatEvent::Delta(text) if text == "hi"));
        assert!(
            matches!(&events[1], ChatEvent::ToolCall { id, name, arguments } if id == "call_2" && name == "read" && arguments == r#"{"path":"b"}"#)
        );
        assert!(matches!(events[2], ChatEvent::Usage { total_tokens: 12 }));
    }
    #[test]
    fn api_key每次请求重新解析() {
        let name = "DSCODE_WIRE_ROTATING_KEY_AB12";
        let provider = HttpProvider::new(
            "fixture".into(),
            WireFormat::OpenAiCompletions,
            "https://example.test".into(),
            Some(format!("env:{name}")),
            "model".into(),
        );
        unsafe { std::env::set_var(name, "first") };
        let first = provider.request(&json!({})).unwrap().build().unwrap();
        unsafe { std::env::set_var(name, "second") };
        let second = provider.request(&json!({})).unwrap().build().unwrap();
        unsafe { std::env::remove_var(name) };
        assert_eq!(first.headers()["authorization"], "Bearer first");
        assert_eq!(second.headers()["authorization"], "Bearer second");
    }
}
