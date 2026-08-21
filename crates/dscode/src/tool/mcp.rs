//! MCP client integration backed by the official Rust SDK (`rmcp`).

use super::{Tier, Tool, ToolOutput};
use futures_util::StreamExt;
use http::{HeaderName, HeaderValue};
use rmcp::{
    model::{CallToolRequestParams, PaginatedRequestParams},
    service::{RunningService, RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{
        async_rw::AsyncRwTransport, streamable_http_client::StreamableHttpClientTransportConfig,
        StreamableHttpClientTransport, Transport,
    },
    RoleClient, ServiceExt,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
type McpService = RunningService<RoleClient, ()>;

#[derive(Clone, Debug, Deserialize)]
struct ServerConfig {
    #[serde(default, rename = "type")]
    transport: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default, rename = "oauthToken", alias = "accessToken")]
    oauth_token: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Default, Deserialize)]
struct ClaudeConfig {
    #[serde(default, rename = "mcpServers")]
    servers: BTreeMap<String, ServerConfig>,
    #[serde(default, rename = "disabledServers", alias = "disabledMcpServers")]
    disabled: BTreeSet<String>,
    #[serde(default, rename = "enabledServers", alias = "enabledMcpServers")]
    enabled: BTreeSet<String>,
}

struct Client {
    server: String,
    service: McpService,
}

pub struct McpTool {
    exposed_name: String,
    remote_name: String,
    description: String,
    input_schema: Value,
    tier: Tier,
    client: Arc<Client>,
}

/// Import effective Claude MCP servers and expose their discovered tools.
pub async fn discover_tools(project_root: &Path) -> Result<Vec<Box<dyn Tool>>, String> {
    let user = dirs::home_dir()
        .map(|home| home.join(".claude.json"))
        .unwrap_or_else(|| PathBuf::from(".claude.json"));
    discover_tools_in(&user, &project_root.join(".claude.json"), project_root).await
}

pub fn config_fingerprint(project_root: &Path) -> u64 {
    let user = dirs::home_dir()
        .map(|home| home.join(".claude.json"))
        .unwrap_or_else(|| PathBuf::from(".claude.json"));
    let project = project_root.join(".claude.json");
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for path in [user, project] {
        path.hash(&mut hasher);
        match std::fs::read(&path) {
            Ok(bytes) => bytes.hash(&mut hasher),
            Err(error) => error.kind().hash(&mut hasher),
        }
    }
    hasher.finish()
}

async fn discover_tools_in(
    user_file: &Path,
    project_file: &Path,
    project_root: &Path,
) -> Result<Vec<Box<dyn Tool>>, String> {
    let servers = effective_servers(user_file, project_file)?;
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    let mut exposed = BTreeSet::new();
    for (server_name, config) in servers {
        let client = Arc::new(Client {
            server: server_name.clone(),
            service: start_service(&server_name, &config, project_root).await?,
        });
        let mut cursor = None;
        let mut seen_cursors = BTreeSet::new();
        loop {
            let result = tokio::time::timeout(
                REQUEST_TIMEOUT,
                client.service.list_tools(
                    cursor
                        .clone()
                        .map(|value| PaginatedRequestParams::default().with_cursor(Some(value))),
                ),
            )
            .await
            .map_err(|_| format!("MCP server「{server_name}」tools/list 超时"))?
            .map_err(|error| format!("MCP server「{server_name}」tools/list 失败：{error}"))?;
            for tool in result.tools {
                let exposed_name = format!("mcp__{server_name}_{}", tool.name);
                if !exposed.insert(exposed_name.clone()) {
                    return Err(format!("MCP 工具名冲突：{exposed_name}"));
                }
                let tier = if tool
                    .annotations
                    .as_ref()
                    .and_then(|annotations| annotations.read_only_hint)
                    == Some(true)
                {
                    Tier::Read
                } else {
                    Tier::Write
                };
                tools.push(Box::new(McpTool {
                    exposed_name,
                    remote_name: tool.name.into_owned(),
                    description: tool
                        .description
                        .map(|description| description.into_owned())
                        .unwrap_or_else(|| "MCP 工具".into()),
                    input_schema: Value::Object((*tool.input_schema).clone()),
                    tier,
                    client: client.clone(),
                }));
            }
            let Some(next) = result.next_cursor else {
                break;
            };
            if !seen_cursors.insert(next.clone()) {
                return Err(format!(
                    "MCP server「{server_name}」tools/list 游标循环：{next}"
                ));
            }
            cursor = Some(next);
        }
    }
    Ok(tools)
}

struct StdioTransport {
    inner: AsyncRwTransport<RoleClient, ChildStdout, ChildStdin>,
    child: Child,
}

impl Transport<RoleClient> for StdioTransport {
    type Error = std::io::Error;

    fn name() -> Cow<'static, str> {
        "stdio-child".into()
    }

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.inner.receive()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.inner.close().await?;
        if self.child.try_wait()?.is_none() {
            self.child.kill().await?;
        }
        Ok(())
    }
}
struct LegacySseTransport {
    client: reqwest::Client,
    post_url: String,
    auth: Option<String>,
    headers: HashMap<HeaderName, HeaderValue>,
    receiver: mpsc::Receiver<RxJsonRpcMessage<RoleClient>>,
    task: Option<JoinHandle<()>>,
}

impl LegacySseTransport {
    async fn connect(
        url: &str,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<Self, std::io::Error> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(io_error)?;
        let mut request = client.get(url).header("accept", "text/event-stream");
        if let Some(token) = &auth {
            request = request.bearer_auth(token);
        }
        for (name, value) in &headers {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(io_error)?;
        if !response.status().is_success() {
            return Err(std::io::Error::other(format!(
                "legacy SSE GET 返回 {}",
                response.status()
            )));
        }
        let base = reqwest::Url::parse(url).map_err(io_error)?;
        let (endpoint_tx, endpoint_rx) = oneshot::channel::<Result<String, String>>();
        let (message_tx, receiver) = mpsc::channel(64);
        let task = tokio::spawn(async move {
            let mut endpoint_tx = Some(endpoint_tx);
            let mut decoder = LegacySseDecoder::default();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else { break };
                for (event, data) in decoder.push(&chunk) {
                    if event == "endpoint" {
                        if let Some(tx) = endpoint_tx.take() {
                            let endpoint = base
                                .join(data.trim())
                                .map(|url| url.to_string())
                                .map_err(|error| error.to_string());
                            let _ = tx.send(endpoint);
                        }
                    } else if event.is_empty() || event == "message" {
                        let Ok(message) = serde_json::from_str(&data) else {
                            continue;
                        };
                        if message_tx.send(message).await.is_err() {
                            return;
                        }
                    }
                }
            }
            if let Some(tx) = endpoint_tx {
                let _ = tx.send(Err("SSE 流关闭前未返回 endpoint 事件".into()));
            }
        });
        let post_url = tokio::time::timeout(REQUEST_TIMEOUT, endpoint_rx)
            .await
            .map_err(|_| std::io::Error::other("等待 SSE endpoint 超时"))?
            .map_err(|_| std::io::Error::other("SSE endpoint 任务提前结束"))?
            .map_err(std::io::Error::other)?;
        Ok(Self {
            client,
            post_url,
            auth,
            headers,
            receiver,
            task: Some(task),
        })
    }
}

impl Transport<RoleClient> for LegacySseTransport {
    type Error = std::io::Error;

    fn name() -> Cow<'static, str> {
        "legacy-sse".into()
    }

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let client = self.client.clone();
        let post_url = self.post_url.clone();
        let auth = self.auth.clone();
        let headers = self.headers.clone();
        async move {
            let mut request = client
                .post(post_url)
                .timeout(REQUEST_TIMEOUT)
                .header("content-type", "application/json")
                .json(&item);
            if let Some(token) = auth {
                request = request.bearer_auth(token);
            }
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let response = request.send().await.map_err(io_error)?;
            if response.status().is_success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "legacy SSE POST 返回 {}",
                    response.status()
                )))
            }
        }
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.receiver.recv()
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        async { Ok(()) }
    }
}

#[derive(Default)]
struct LegacySseDecoder {
    buffer: Vec<u8>,
}

impl LegacySseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<(String, String)> {
        const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() > MAX_EVENT_BYTES {
            self.buffer.clear();
            return vec![];
        }
        let mut events = Vec::new();
        while let Some((end, separator)) = event_boundary(&self.buffer) {
            let block = self.buffer.drain(..end).collect::<Vec<_>>();
            self.buffer.drain(..separator);
            let text = String::from_utf8_lossy(&block);
            let mut event = String::new();
            let mut data = Vec::new();
            for line in text.lines() {
                let line = line.trim_end_matches('\r');
                if let Some(value) = line.strip_prefix("event:") {
                    event = value.trim_start().to_string();
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.trim_start());
                }
            }
            if !data.is_empty() {
                events.push((event, data.join("\n")));
            }
        }
        events
    }
}

fn event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes.windows(2).position(|window| window == b"\n\n");
    let crlf = bytes.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(left), Some(right)) if left <= right => Some((left, 2)),
        (Some(_), Some(right)) => Some((right, 4)),
        (Some(left), None) => Some((left, 2)),
        (None, Some(right)) => Some((right, 4)),
        (None, None) => None,
    }
}

fn io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

async fn start_service(
    name: &str,
    config: &ServerConfig,
    project_root: &Path,
) -> Result<McpService, String> {
    let transport = config.transport.as_deref().unwrap_or_else(|| {
        if config.url.is_some() {
            "http"
        } else {
            "stdio"
        }
    });
    match transport {
        "stdio" => {
            let command = config
                .command
                .as_deref()
                .filter(|command| !command.trim().is_empty())
                .ok_or_else(|| format!("MCP server「{name}」缺少 stdio command"))?;
            let mut process = Command::new(command);
            process
                .args(&config.args)
                .envs(&config.env)
                .current_dir(match &config.cwd {
                    Some(cwd) if cwd.is_absolute() => cwd.clone(),
                    Some(cwd) => project_root.join(cwd),
                    None => project_root.to_path_buf(),
                })
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true);
            let mut child = process
                .spawn()
                .map_err(|error| format!("启动 MCP server「{name}」失败：{error}"))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| format!("MCP server「{name}」未提供 stdout"))?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| format!("MCP server「{name}」未提供 stdin"))?;
            let transport = StdioTransport {
                inner: AsyncRwTransport::new_client(stdout, stdin),
                child,
            };
            tokio::time::timeout(REQUEST_TIMEOUT, ().serve(transport))
                .await
                .map_err(|_| format!("MCP server「{name}」initialize 超时"))?
                .map_err(|error| format!("MCP server「{name}」initialize 失败：{error}"))
        }
        "http" | "streamable-http" => {
            let url = config
                .url
                .as_deref()
                .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
                .ok_or_else(|| format!("MCP server「{name}」缺少合法 http(s) url"))?;
            let (auth_header, custom_headers) = http_headers(config)?;
            let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url)
                .custom_headers(custom_headers)
                .reinit_on_expired_session(true);
            if let Some(token) = auth_header {
                transport_config = transport_config.auth_header(token);
            }
            let transport = StreamableHttpClientTransport::from_config(transport_config);
            let initialized = tokio::time::timeout(REQUEST_TIMEOUT, ().serve(transport))
                .await
                .map_err(|_| format!("MCP server「{name}」initialize 超时"))?;
            initialized.map_err(|error| {
                if error.is_authorization_required() {
                    format!(
                        "MCP server「{name}」需要 OAuth 授权；请在 .claude.json 的 oauthToken/Authorization 中引用已登录凭据后重试：{error}"
                    )
                } else {
                    format!("MCP server「{name}」initialize 失败：{error}")
                }
            })
        }
        "sse" => {
            let url = config
                .url
                .as_deref()
                .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
                .ok_or_else(|| format!("MCP server「{name}」缺少合法 http(s) SSE url"))?;
            let (auth_header, custom_headers) = http_headers(config)?;
            let transport = LegacySseTransport::connect(url, auth_header, custom_headers)
                .await
                .map_err(|error| {
                    format!(
                        "MCP server「{name}」legacy SSE 连接失败：{error}；如需 OAuth，请配置 oauthToken/Authorization 凭据引用"
                    )
                })?;
            tokio::time::timeout(REQUEST_TIMEOUT, ().serve(transport))
                .await
                .map_err(|_| format!("MCP server「{name}」initialize 超时"))?
                .map_err(|error| format!("MCP server「{name}」initialize 失败：{error}"))
        }
        other => Err(format!(
            "MCP server「{name}」transport「{other}」未知；支持 stdio/http/sse"
        )),
    }
}

fn http_headers(
    config: &ServerConfig,
) -> Result<(Option<String>, HashMap<HeaderName, HeaderValue>), String> {
    let mut auth = config
        .oauth_token
        .as_deref()
        .map(resolve_config_value)
        .transpose()?;
    let mut headers = HashMap::new();
    for (name, value) in &config.headers {
        let value = resolve_config_value(value)?;
        if name.eq_ignore_ascii_case("authorization") {
            auth = Some(value.strip_prefix("Bearer ").unwrap_or(&value).to_string());
            continue;
        }
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| format!("MCP HTTP header 名无效「{name}」：{error}"))?;
        let value = HeaderValue::from_str(&value)
            .map_err(|error| format!("MCP HTTP header 值无效：{error}"))?;
        headers.insert(name, value);
    }
    Ok((auth, headers))
}

fn resolve_config_value(value: &str) -> Result<String, String> {
    if value.starts_with("env:") {
        crate::config::resolve_credential_ref(value)?
            .ok_or_else(|| format!("MCP 凭据引用「{value}」无法解析"))
    } else {
        Ok(value.to_string())
    }
}

fn effective_servers(
    user_file: &Path,
    project_file: &Path,
) -> Result<BTreeMap<String, ServerConfig>, String> {
    let user = parse_config(user_file)?;
    let project = parse_config(project_file)?;
    let mut servers = user.servers;
    servers.extend(project.servers);
    let disabled: BTreeSet<String> = user.disabled.union(&project.disabled).cloned().collect();
    let enabled: BTreeSet<String> = user.enabled.union(&project.enabled).cloned().collect();
    servers.retain(|name, config| {
        !disabled.contains(name) && (enabled.contains(name) || config.enabled.unwrap_or(true))
    });
    Ok(servers)
}

fn parse_config(path: &Path) -> Result<ClaudeConfig, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| format!("MCP 配置解析失败 {}：{error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ClaudeConfig::default()),
        Err(error) => Err(format!("MCP 配置读取失败 {}：{error}", path.display())),
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.exposed_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn tier(&self) -> Tier {
        self.tier
    }

    async fn execute(&self, arguments: &Value) -> ToolOutput {
        let Some(arguments) = arguments.as_object() else {
            return ToolOutput {
                output: "MCP 工具参数必须是 JSON object".into(),
                exit_code: Some(1),
            };
        };
        let params =
            CallToolRequestParams::new(self.remote_name.clone()).with_arguments(arguments.clone());
        let result =
            tokio::time::timeout(REQUEST_TIMEOUT, self.client.service.call_tool(params)).await;
        match result {
            Err(_) => ToolOutput {
                output: format!("MCP server「{}」tools/call 超时", self.client.server),
                exit_code: Some(1),
            },
            Ok(Err(error)) => ToolOutput {
                output: format!(
                    "MCP server「{}」tools/call 失败：{error}",
                    self.client.server
                ),
                exit_code: Some(1),
            },
            Ok(Ok(result)) => ToolOutput {
                output: serde_json::to_string_pretty(&result)
                    .unwrap_or_else(|_| json!({ "content": result.content }).to_string()),
                exit_code: Some(if result.is_error == Some(true) { 1 } else { 0 }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio_fixture(temp: &Path) -> (String, Vec<String>) {
        #[cfg(windows)]
        {
            let script = temp.join("mcp-fixture.ps1");
            std::fs::write(
                &script,
                r#"while (($line = [Console]::In.ReadLine()) -ne $null) {
  try { $m = $line | ConvertFrom-Json } catch { continue }
  if ($m.method -eq 'initialize') {
    $r = @{jsonrpc='2.0';id=$m.id;result=@{protocolVersion=$m.params.protocolVersion;capabilities=@{tools=@{}};serverInfo=@{name='fixture';version='1'}}}
  } elseif ($m.method -eq 'tools/list') {
    $r = @{jsonrpc='2.0';id=$m.id;result=@{tools=@(@{name='echo';description='echo';inputSchema=@{type='object'}})}}
  } elseif ($m.method -eq 'tools/call') {
    $r = @{jsonrpc='2.0';id=$m.id;result=@{content=@(@{type='text';text=$m.params.arguments.message});isError=$false}}
  } else { continue }
  [Console]::Out.WriteLine(($r | ConvertTo-Json -Compress -Depth 20))
}"#,
            )
            .unwrap();
            let powershell = std::env::var_os("SystemRoot")
                .map(std::path::PathBuf::from)
                .map(|root| root.join("System32/WindowsPowerShell/v1.0/powershell.exe"))
                .filter(|path| path.is_file())
                .unwrap_or_else(|| std::path::PathBuf::from("powershell.exe"));
            (
                powershell.to_string_lossy().into_owned(),
                vec![
                    "-NoProfile".into(),
                    "-ExecutionPolicy".into(),
                    "Bypass".into(),
                    "-File".into(),
                    script.to_string_lossy().into_owned(),
                ],
            )
        }
        #[cfg(not(windows))]
        {
            let script = temp.join("mcp-fixture.sh");
            std::fs::write(
                &script,
                r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,}]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}\n' "$id" ;;
    *'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"dscode-mcp-ok"}],"isError":false}}\n' "$id" ;;
  esac
done"#,
            )
            .unwrap();
            ("sh".into(), vec![script.to_string_lossy().into_owned()])
        }
    }

    async fn read_http_json(stream: &mut tokio::net::TcpStream) -> Value {
        use tokio::io::AsyncReadExt;
        let mut buffer = Vec::new();
        loop {
            let mut chunk = [0u8; 2048];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0, "HTTP 请求提前关闭");
            buffer.extend_from_slice(&chunk[..count]);
            let Some(header_end) = buffer.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&buffer[..header_end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            let body_start = header_end + 4;
            while buffer.len() < body_start + length {
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0, "HTTP body 提前关闭");
                buffer.extend_from_slice(&chunk[..count]);
            }
            return serde_json::from_slice(&buffer[body_start..body_start + length]).unwrap();
        }
    }

    #[test]
    fn 配置双层覆盖且禁用并集() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user.json");
        let project = temp.path().join("project.json");
        std::fs::write(
            &user,
            r#"{"mcpServers":{"a":{"command":"old"},"b":{"command":"b"}},"disabledServers":["b"]}"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"{"mcpServers":{"a":{"command":"new"},"c":{"command":"c"}},"disabledMcpServers":["c"]}"#,
        )
        .unwrap();
        let servers = effective_servers(&user, &project).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers["a"].command.as_deref(), Some("new"));
    }

    #[test]
    fn http授权头支持环境引用() {
        let name = "DSCODE_MCP_TEST_TOKEN";
        unsafe { std::env::set_var(name, "secret") };
        let config = ServerConfig {
            transport: Some("http".into()),
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            url: Some("https://example.test/mcp".into()),
            headers: BTreeMap::from([("Authorization".into(), format!("env:{name}"))]),
            oauth_token: None,
            enabled: None,
        };
        let (token, headers) = http_headers(&config).unwrap();
        unsafe { std::env::remove_var(name) };
        assert_eq!(token.as_deref(), Some("secret"));
        assert!(headers.is_empty());
    }

    #[tokio::test]
    async fn 离线stdio完成发现与调用() {
        let temp = tempfile::tempdir().unwrap();
        let (command, args) = stdio_fixture(temp.path());
        let config = ServerConfig {
            transport: Some("stdio".into()),
            command: Some(command),
            args,
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            oauth_token: None,
            enabled: None,
        };
        let service = start_service("fixture", &config, temp.path())
            .await
            .unwrap();
        let listed = service.list_tools(None).await.unwrap();
        assert_eq!(listed.tools[0].name.as_ref(), "echo");
        let result = service
            .call_tool(
                CallToolRequestParams::new("echo").with_arguments(
                    json!({ "message": "dscode-mcp-ok" })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert!(serde_json::to_string(&result)
            .unwrap()
            .contains("dscode-mcp-ok"));
    }

    #[tokio::test]
    async fn 离线legacy_sse完成endpoint_post_message往返() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sse, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0u8; 1];
                sse.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
            }
            assert!(String::from_utf8_lossy(&request).starts_with("GET /sse "));
            sse.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\nevent: endpoint\ndata: http://{address}/message\n\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();

            for _ in 0..4 {
                let (mut post, _) = listener.accept().await.unwrap();
                let message = read_http_json(&mut post).await;
                post.write_all(
                    b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
                let Some(id) = message.get("id").cloned() else {
                    continue;
                };
                let result = match message["method"].as_str().unwrap_or_default() {
                    "initialize" => json!({
                        "protocolVersion": message["params"]["protocolVersion"],
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "fixture", "version": "1" }
                    }),
                    "tools/list" => json!({
                        "tools": [{
                            "name": "echo",
                            "description": "echo",
                            "inputSchema": { "type": "object" }
                        }]
                    }),
                    "tools/call" => json!({
                        "content": [{
                            "type": "text",
                            "text": message["params"]["arguments"]["message"]
                        }],
                        "isError": false
                    }),
                    other => panic!("意外 MCP 方法：{other}"),
                };
                let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                sse.write_all(format!("event: message\ndata: {response}\n\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let config = ServerConfig {
            transport: Some("sse".into()),
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            url: Some(format!("http://{address}/sse")),
            headers: BTreeMap::new(),
            oauth_token: None,
            enabled: None,
        };
        let service = start_service("fixture", &config, Path::new("."))
            .await
            .unwrap();
        let listed = service.list_tools(None).await.unwrap();
        assert_eq!(listed.tools[0].name.as_ref(), "echo");
        let result = service
            .call_tool(
                CallToolRequestParams::new("echo").with_arguments(
                    json!({ "message": "dscode-sse-ok" })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert!(serde_json::to_string(&result)
            .unwrap()
            .contains("dscode-sse-ok"));
        server.await.unwrap();
    }
    #[tokio::test]
    #[ignore = "需要本机 npx 与网络安装官方 MCP server-everything"]
    async fn 官方stdio_server完成发现与调用() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user.json");
        let project = temp.path().join("project.json");
        let command = if cfg!(windows) { "npx.cmd" } else { "npx" };
        std::fs::write(&user, "{}").unwrap();
        std::fs::write(
            &project,
            serde_json::json!({
                "mcpServers": {
                    "everything": {
                        "type": "stdio",
                        "command": command,
                        "args": ["-y", "@modelcontextprotocol/server-everything"]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let tools = discover_tools_in(&user, &project, temp.path())
            .await
            .unwrap();
        let echo = tools
            .iter()
            .find(|tool| tool.name().ends_with("_echo"))
            .expect("server-everything 应暴露 echo");
        let output = echo.execute(&json!({ "message": "dscode-mcp-ok" })).await;
        assert_eq!(output.exit_code, Some(0), "{}", output.output);
        assert!(output.output.contains("dscode-mcp-ok"), "{}", output.output);
    }
}
