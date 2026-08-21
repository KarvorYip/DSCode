//! CDP browser tool (tools.zh.md §3.10): named Chromium targets survive tool calls and
//! sub-agents; HTTP discovery/target lifecycle plus direct local WebSocket CDP commands.

use super::{Tier, Tool, ToolCtx, ToolOutput};
use serde::Deserialize;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:9222";
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const CDP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FRAME_BYTES: u64 = 16 * 1024 * 1024;
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
static MASK_COUNTER: AtomicU32 = AtomicU32::new(1);
static BROWSER_STATE: OnceLock<tokio::sync::Mutex<BrowserState>> = OnceLock::new();

#[derive(Default)]
struct BrowserState {
    /// Stable model-facing name → Chromium target id. The target itself lives in Chromium.
    tabs: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
struct Target {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default, rename = "webSocketDebuggerUrl")]
    websocket_url: Option<String>,
}

fn state() -> &'static tokio::sync::Mutex<BrowserState> {
    BROWSER_STATE.get_or_init(|| tokio::sync::Mutex::new(BrowserState::default()))
}

fn ok(value: Value) -> ToolOutput {
    ToolOutput {
        output: serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
        exit_code: Some(0),
    }
}

fn err(message: impl Into<String>) -> ToolOutput {
    ToolOutput {
        output: message.into(),
        exit_code: Some(1),
    }
}

pub struct BrowserTool;

#[async_trait::async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "通过本机 Chromium DevTools Protocol 操作持久命名 tab。action=list 列出目标；\
         open 创建或附加 tab；tab 按 targetId 附加或读取命名 tab；navigate 导航；\
         snapshot/observe 获取无障碍树；evaluate/run 执行 JavaScript；screenshot 截图；close 关闭 tab。\
         需以 --remote-debugging-port 启动 Chromium，或配置 browser.endpoint"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "open", "tab", "navigate", "snapshot", "observe", "evaluate", "run", "screenshot", "close"],
                    "description": "CDP 操作"
                },
                "name": { "type": "string", "description": "持久 tab 名；默认 main" },
                "targetId": { "type": "string", "description": "tab/open 按 Chromium target id 附加" },
                "url": { "type": "string", "description": "open/navigate 的 URL；open 默认 about:blank" },
                "expression": { "type": "string", "description": "evaluate/run 的 JavaScript 表达式" },
                "format": { "type": "string", "enum": ["png", "jpeg", "webp"], "description": "screenshot 图片格式；默认 png" },
                "quality": { "type": "integer", "minimum": 0, "maximum": 100, "description": "screenshot 的 jpeg/webp 质量" }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    async fn execute(&self, _arguments: &Value) -> ToolOutput {
        err("browser 需要执行上下文；请从会话工具注册表调用")
    }

    async fn execute_ctx(&self, ctx: &ToolCtx<'_>, arguments: &Value) -> ToolOutput {
        execute_at(
            ctx.config
                .browser_endpoint
                .as_deref()
                .unwrap_or(DEFAULT_ENDPOINT),
            arguments,
        )
        .await
    }
}

async fn execute_at(endpoint: &str, args: &Value) -> ToolOutput {
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        return err("browser.endpoint 必须是 http:// 或 https:// CDP 地址");
    }
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let result = match action {
        "list" => list_tabs(endpoint).await,
        "open" => open_tab(endpoint, args).await,
        "tab" => attach_or_get_tab(endpoint, args).await,
        "navigate" => cdp_for_named(endpoint, args, "Page.navigate", |args| {
            let url = required_str(args, "url", "navigate 需要 url")?;
            Ok(json!({ "url": url }))
        })
        .await,
        "snapshot" | "observe" => {
            cdp_for_named(endpoint, args, "Accessibility.getFullAXTree", |_| Ok(json!({}))).await
        }
        "evaluate" | "run" => {
            let result = cdp_for_named(endpoint, args, "Runtime.evaluate", |args| {
                let expression = required_str(args, "expression", "evaluate/run 需要 expression")?;
                Ok(json!({
                    "expression": expression,
                    "awaitPromise": true,
                    "returnByValue": true
                }))
            })
            .await;
            match result {
                Ok(value) if value.get("exceptionDetails").is_some() => Err(format!(
                    "JavaScript 执行失败：{}",
                    value["exceptionDetails"]
                )),
                other => other,
            }
        }
        "screenshot" => cdp_for_named(endpoint, args, "Page.captureScreenshot", |args| {
            let format = args.get("format").and_then(Value::as_str).unwrap_or("png");
            if !matches!(format, "png" | "jpeg" | "webp") {
                return Err("screenshot format 必须是 png/jpeg/webp".into());
            }
            let mut params = json!({ "format": format });
            if let Some(value) = args.get("quality") {
                let quality = value
                    .as_u64()
                    .filter(|quality| *quality <= 100)
                    .ok_or_else(|| "screenshot quality 必须是 0..=100 的整数".to_string())?;
                params["quality"] = json!(quality);
            }
            Ok(params)
        })
        .await,
        "close" => close_tab(endpoint, args).await,
        _ => Err(format!(
            "未知 browser action「{action}」；可用 list/open/tab/navigate/snapshot/observe/evaluate/run/screenshot/close"
        )),
    };
    match result {
        Ok(value) => ok(value),
        Err(message) => err(message),
    }
}

fn required_str<'a>(args: &'a Value, key: &str, message: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| message.to_string())
}

async fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| format!("创建 CDP HTTP 客户端失败：{e}"))
}

fn endpoint_url(endpoint: &str, path: &str) -> String {
    format!("{}{path}", endpoint.trim_end_matches('/'))
}

fn connection_error(endpoint: &str, detail: impl std::fmt::Display) -> String {
    format!(
        "无法连接 Chromium CDP（{endpoint}）：{detail}。请用 --remote-debugging-port=9222 启动 Chromium，或设置 browser.endpoint"
    )
}

async fn fetch_targets(endpoint: &str) -> Result<Vec<Target>, String> {
    let client = client().await?;
    let response = client
        .get(endpoint_url(endpoint, "/json/list"))
        .send()
        .await
        .map_err(|e| connection_error(endpoint, e))?;
    let response = response
        .error_for_status()
        .map_err(|e| format!("CDP /json/list 返回错误状态：{e}"))?;
    response
        .json::<Vec<Target>>()
        .await
        .map_err(|e| format!("CDP /json/list 响应不是目标列表：{e}"))
}

fn target_json(target: &Target, name: Option<&str>) -> Value {
    json!({
        "name": name,
        "targetId": target.id,
        "type": target.kind,
        "title": target.title,
        "url": target.url,
        "webSocketDebuggerUrl": target.websocket_url
    })
}

async fn list_tabs(endpoint: &str) -> Result<Value, String> {
    let targets = fetch_targets(endpoint).await?;
    let names = state().lock().await.tabs.clone();
    Ok(Value::Array(
        targets
            .iter()
            .filter(|target| target.kind == "page")
            .map(|target| {
                let name = names
                    .iter()
                    .find_map(|(name, id)| (id == &target.id).then_some(name.as_str()));
                target_json(target, name)
            })
            .collect(),
    ))
}

async fn open_tab(endpoint: &str, args: &Value) -> Result<Value, String> {
    let name = args.get("name").and_then(Value::as_str).unwrap_or("main");
    if name.is_empty() {
        return Err("open 的 name 不得为空".into());
    }
    if let Some(target_id) = args.get("targetId").and_then(Value::as_str) {
        return attach_target(endpoint, name, target_id).await;
    }

    if let Some(target_id) = state().lock().await.tabs.get(name).cloned() {
        if let Some(target) = fetch_targets(endpoint)
            .await?
            .into_iter()
            .find(|target| target.id == target_id)
        {
            return Ok(target_json(&target, Some(name)));
        }
        state().lock().await.tabs.remove(name);
    }

    let url = args
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("about:blank");
    let client = client().await?;
    let response = client
        .put(endpoint_url(
            endpoint,
            &format!("/json/new?{}", percent_encode(url)),
        ))
        .send()
        .await
        .map_err(|e| connection_error(endpoint, e))?;
    let target = response
        .error_for_status()
        .map_err(|e| format!("CDP 创建 tab 失败：{e}"))?
        .json::<Target>()
        .await
        .map_err(|e| format!("CDP 创建 tab 的响应无效：{e}"))?;
    state()
        .lock()
        .await
        .tabs
        .insert(name.to_string(), target.id.clone());
    Ok(target_json(&target, Some(name)))
}

async fn attach_or_get_tab(endpoint: &str, args: &Value) -> Result<Value, String> {
    let name = args.get("name").and_then(Value::as_str).unwrap_or("main");
    if let Some(target_id) = args.get("targetId").and_then(Value::as_str) {
        attach_target(endpoint, name, target_id).await
    } else {
        let target = resolve_target(endpoint, args).await?;
        Ok(target_json(&target, Some(name)))
    }
}

async fn attach_target(endpoint: &str, name: &str, target_id: &str) -> Result<Value, String> {
    let target = fetch_targets(endpoint)
        .await?
        .into_iter()
        .find(|target| target.id == target_id)
        .ok_or_else(|| format!("Chromium 中不存在 targetId「{target_id}」"))?;
    if target.kind != "page" {
        return Err(format!(
            "targetId「{target_id}」类型为「{}」，browser 仅附加 page target",
            target.kind
        ));
    }
    state()
        .lock()
        .await
        .tabs
        .insert(name.to_string(), target.id.clone());
    Ok(target_json(&target, Some(name)))
}

async fn resolve_target(endpoint: &str, args: &Value) -> Result<Target, String> {
    let name = args.get("name").and_then(Value::as_str).unwrap_or("main");
    let target_id = if let Some(id) = args.get("targetId").and_then(Value::as_str) {
        id.to_string()
    } else {
        state()
            .lock()
            .await
            .tabs
            .get(name)
            .cloned()
            .ok_or_else(|| format!("命名 tab「{name}」尚未打开；请先调用 browser open"))?
    };
    let target = fetch_targets(endpoint)
        .await?
        .into_iter()
        .find(|target| target.id == target_id)
        .ok_or_else(|| format!("命名 tab「{name}」已不存在；请重新 open 或 tab 附加"))?;
    Ok(target)
}

async fn close_tab(endpoint: &str, args: &Value) -> Result<Value, String> {
    let target = resolve_target(endpoint, args).await?;
    let client = client().await?;
    client
        .get(endpoint_url(
            endpoint,
            &format!("/json/close/{}", percent_encode(&target.id)),
        ))
        .send()
        .await
        .map_err(|e| connection_error(endpoint, e))?
        .error_for_status()
        .map_err(|e| format!("CDP 关闭 tab 失败：{e}"))?;
    state().lock().await.tabs.retain(|_, id| id != &target.id);
    Ok(json!({ "closed": true, "targetId": target.id }))
}

async fn cdp_for_named<F>(
    endpoint: &str,
    args: &Value,
    method: &str,
    params: F,
) -> Result<Value, String>
where
    F: FnOnce(&Value) -> Result<Value, String>,
{
    let target = resolve_target(endpoint, args).await?;
    let ws_url = target.websocket_url.ok_or_else(|| {
        format!(
            "targetId「{}」没有 webSocketDebuggerUrl；请确认它是可调试 page",
            target.id
        )
    })?;
    cdp_call(&ws_url, method, params(args)?).await
}

async fn cdp_call(ws_url: &str, method: &str, params: Value) -> Result<Value, String> {
    let mut stream = connect_websocket(ws_url).await?;
    let request = json!({ "id": 1, "method": method, "params": params }).to_string();
    write_frame(&mut stream, 0x1, request.as_bytes()).await?;
    let response = tokio::time::timeout(CDP_TIMEOUT, receive_cdp_response(&mut stream, 1))
        .await
        .map_err(|_| format!("CDP 命令 {method} 等待响应超时（30 秒）"))??;
    if let Some(error) = response.get("error") {
        return Err(format!("CDP 命令 {method} 失败：{error}"));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| format!("CDP 命令 {method} 响应缺少 result"))
}

async fn connect_websocket(url: &str) -> Result<TcpStream, String> {
    if url.starts_with("wss://") {
        return Err("当前首发 browser 仅连接本机 ws:// CDP；wss:// 远端调试不在首发范围".into());
    }
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| format!("CDP WebSocket 地址无效：{url}"))?;
    let (authority, path_tail) = rest.split_once('/').unwrap_or((rest, ""));
    let path = format!("/{path_tail}");
    let (host, port) = parse_authority(authority)?;
    let mut stream = tokio::time::timeout(HTTP_TIMEOUT, TcpStream::connect((host.as_str(), port)))
        .await
        .map_err(|_| format!("连接 CDP WebSocket 超时：{authority}"))?
        .map_err(|e| format!("连接 CDP WebSocket 失败（{authority}）：{e}"))?;

    let key = websocket_key();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    tokio::time::timeout(HTTP_TIMEOUT, stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| "发送 CDP WebSocket 握手超时".to_string())?
        .map_err(|e| format!("发送 CDP WebSocket 握手失败：{e}"))?;

    let mut header = Vec::with_capacity(512);
    loop {
        if header.len() >= 32 * 1024 {
            return Err("CDP WebSocket 握手响应头过大".into());
        }
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(HTTP_TIMEOUT, stream.read(&mut byte))
            .await
            .map_err(|_| "等待 CDP WebSocket 握手超时".to_string())?
            .map_err(|e| format!("读取 CDP WebSocket 握手失败：{e}"))?;
        if read == 0 {
            return Err("CDP 在 WebSocket 握手完成前关闭连接".into());
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    validate_handshake(&header, &key)?;
    Ok(stream)
}

fn parse_authority(authority: &str) -> Result<(String, u16), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .ok_or_else(|| format!("CDP WebSocket IPv6 地址缺少端口：{authority}"))?;
        let port = port
            .parse::<u16>()
            .map_err(|_| format!("CDP WebSocket 端口无效：{authority}"))?;
        return Ok((host.to_string(), port));
    }
    let (host, port) = authority.rsplit_once(':').unwrap_or((authority, "80"));
    if host.is_empty() {
        return Err(format!("CDP WebSocket 主机为空：{authority}"));
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| format!("CDP WebSocket 端口无效：{authority}"))?;
    Ok((host.to_string(), port))
}

fn validate_handshake(header: &[u8], key: &str) -> Result<(), String> {
    let text = String::from_utf8_lossy(header);
    let mut lines = text.split("\r\n");
    let status = lines.next().unwrap_or_default();
    if !status.contains(" 101 ") {
        return Err(format!("CDP WebSocket 握手被拒绝：{status}"));
    }
    let accept = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("Sec-WebSocket-Accept")
            .then(|| value.trim())
    });
    let mut hash = Sha1::new();
    hash.update(key.as_bytes());
    hash.update(WS_GUID.as_bytes());
    let expected = base64_encode(&hash.finalize());
    if accept != Some(expected.as_str()) {
        return Err("CDP WebSocket 握手校验失败（Sec-WebSocket-Accept 不匹配）".into());
    }
    Ok(())
}

fn websocket_key() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut bytes = now.to_le_bytes();
    let counter = MASK_COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes();
    for (dst, src) in bytes.iter_mut().zip(counter.iter().cycle()) {
        *dst ^= src;
    }
    base64_encode(&bytes)
}

async fn write_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> Result<(), String> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode);
    let len = payload.len() as u64;
    match len {
        0..=125 => frame.push(0x80 | len as u8),
        126..=65535 => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        }
        _ => {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&len.to_be_bytes());
        }
    }
    let mask = next_mask();
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| *b ^ mask[i % 4]));
    stream
        .write_all(&frame)
        .await
        .map_err(|e| format!("发送 CDP WebSocket 帧失败：{e}"))
}

fn next_mask() -> [u8; 4] {
    let counter = MASK_COUNTER.fetch_add(1, Ordering::Relaxed);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (counter ^ time).to_be_bytes()
}

async fn receive_cdp_response(stream: &mut TcpStream, request_id: u64) -> Result<Value, String> {
    let mut fragmented = Vec::new();
    let mut fragmented_text = false;
    loop {
        let mut head = [0u8; 2];
        stream
            .read_exact(&mut head)
            .await
            .map_err(|e| format!("读取 CDP WebSocket 帧头失败：{e}"))?;
        let fin = head[0] & 0x80 != 0;
        let opcode = head[0] & 0x0f;
        if head[1] & 0x80 != 0 {
            return Err("CDP 服务端返回了非法 masked WebSocket 帧".into());
        }
        let mut len = u64::from(head[1] & 0x7f);
        if len == 126 {
            let mut bytes = [0u8; 2];
            stream
                .read_exact(&mut bytes)
                .await
                .map_err(|e| format!("读取 CDP WebSocket 帧长度失败：{e}"))?;
            len = u64::from(u16::from_be_bytes(bytes));
        } else if len == 127 {
            let mut bytes = [0u8; 8];
            stream
                .read_exact(&mut bytes)
                .await
                .map_err(|e| format!("读取 CDP WebSocket 帧长度失败：{e}"))?;
            len = u64::from_be_bytes(bytes);
        }
        if len > MAX_FRAME_BYTES {
            return Err(format!("CDP WebSocket 单帧超过 16 MiB：{len} 字节"));
        }
        if fragmented.len().saturating_add(len as usize) > MAX_FRAME_BYTES as usize {
            return Err("CDP WebSocket 分片消息超过 16 MiB".into());
        }
        let mut payload = vec![0u8; len as usize];
        stream
            .read_exact(&mut payload)
            .await
            .map_err(|e| format!("读取 CDP WebSocket 帧正文失败：{e}"))?;
        match opcode {
            0x8 => return Err("CDP WebSocket 在返回命令结果前关闭".into()),
            0x9 => {
                write_frame(stream, 0xA, &payload).await?;
                continue;
            }
            0xA => continue,
            0x1 => {
                fragmented.clear();
                fragmented.extend_from_slice(&payload);
                fragmented_text = !fin;
            }
            0x0 if fragmented_text => {
                fragmented.extend_from_slice(&payload);
                fragmented_text = !fin;
            }
            _ => continue,
        }
        if !fin {
            continue;
        }
        let value: Value = serde_json::from_slice(&fragmented)
            .map_err(|e| format!("CDP WebSocket 返回非 JSON 文本：{e}"))?;
        if value.get("id").and_then(Value::as_u64) == Some(request_id) {
            return Ok(value);
        }
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(TABLE[((value >> 18) & 63) as usize] as char);
        out.push(TABLE[((value >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn schema注册tier与操作完整() {
        let registry = crate::tool::Registry::builtin();
        let browser = registry.get("browser").expect("registry 应注册 browser");
        assert_eq!(browser.tier(), Tier::Write);
        let schema = browser.parameters_schema();
        let actions = schema["properties"]["action"]["enum"].as_array().unwrap();
        for action in [
            "list",
            "open",
            "tab",
            "navigate",
            "snapshot",
            "observe",
            "evaluate",
            "run",
            "screenshot",
            "close",
        ] {
            assert!(
                actions.iter().any(|value| value.as_str() == Some(action)),
                "缺少 {action}"
            );
        }
    }

    #[tokio::test]
    async fn cdp不可达返回中文可操作错误() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let output = execute_at(&format!("http://{address}"), &json!({ "action": "list" })).await;
        assert_eq!(output.exit_code, Some(1));
        assert!(output.output.contains("无法连接 Chromium CDP"));
        assert!(output.output.contains("--remote-debugging-port=9222"));
    }

    #[tokio::test]
    async fn list使用可替换本地json端点() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 2048];
            let count = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.starts_with("GET /json/list "), "{request}");
            let body = r#"[{"id":"page-1","type":"page","title":"示例","url":"https://example.test/","webSocketDebuggerUrl":"ws://127.0.0.1/devtools/page/page-1"}]"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let output = execute_at(&format!("http://{address}"), &json!({ "action": "list" })).await;
        server.await.unwrap();
        assert_eq!(output.exit_code, Some(0), "{}", output.output);
        assert!(output.output.contains("page-1"));
        assert!(output.output.contains("示例"));
    }
    #[tokio::test]
    async fn websocket执行真实cdp请求响应() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte).await.unwrap();
                header.push(byte[0]);
            }
            let header = String::from_utf8(header).unwrap();
            let key = header
                .split("\r\n")
                .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
                .unwrap();
            let mut hash = Sha1::new();
            hash.update(key.as_bytes());
            hash.update(WS_GUID.as_bytes());
            let accept = base64_encode(&hash.finalize());
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();

            let mut frame_head = [0u8; 2];
            stream.read_exact(&mut frame_head).await.unwrap();
            assert_eq!(frame_head[0], 0x81);
            assert_ne!(frame_head[1] & 0x80, 0, "客户端帧必须 masked");
            let len = usize::from(frame_head[1] & 0x7f);
            assert!(len <= 125, "测试命令应使用短帧");
            let mut mask = [0u8; 4];
            stream.read_exact(&mut mask).await.unwrap();
            let mut payload = vec![0u8; len];
            stream.read_exact(&mut payload).await.unwrap();
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
            let request: Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(request["method"], "Runtime.evaluate");

            let response = br#"{"id":1,"result":{"result":{"type":"number","value":2}}}"#;
            let mut frame = vec![0x81, response.len() as u8];
            frame.extend_from_slice(response);
            stream.write_all(&frame).await.unwrap();
        });
        let result = cdp_call(
            &format!("ws://{address}/devtools/page/page-1"),
            "Runtime.evaluate",
            json!({ "expression": "1 + 1", "returnByValue": true }),
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(result["result"]["value"], 2);
    }

    #[tokio::test]
    #[ignore = "需要本机 Chromium 以 --remote-debugging-port=9222 启动"]
    async fn 本机chromium真实cdp打开读取关闭() {
        let endpoint =
            std::env::var("DSCODE_CDP_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:9222".into());
        let name = "dscode-real-cdp";
        let opened = execute_at(
            &endpoint,
            &json!({
                "action": "open",
                "name": name,
                "url": "data:text/html,<body>dscode-cdp-ok</body>"
            }),
        )
        .await;
        assert_eq!(opened.exit_code, Some(0), "{}", opened.output);

        let mut observed = None;
        for _ in 0..20 {
            let output = execute_at(
                &endpoint,
                &json!({
                    "action": "evaluate",
                    "name": name,
                    "expression": "document.body && document.body.innerText"
                }),
            )
            .await;
            if output.exit_code == Some(0) && output.output.contains("dscode-cdp-ok") {
                observed = Some(output.output);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(observed.is_some(), "真实 CDP 未读回页面正文");

        let closed = execute_at(&endpoint, &json!({ "action": "close", "name": name })).await;
        assert_eq!(closed.exit_code, Some(0), "{}", closed.output);
    }

    #[test]
    fn websocket握手辅助值符合rfc6455样例() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let header = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
        assert!(validate_handshake(header, key).is_ok());
    }
}
