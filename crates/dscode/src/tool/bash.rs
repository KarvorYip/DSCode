//! bash tool: executes in a restricted directory (process cwd), 30s timeout, truncated output (tools.zh.md §3.6).
//! Phase 1 addition: child-process encoding forced to UTF-8; approval is screened via the pattern table at the gate layer, not inside this tool.

use super::{Tier, Tool, ToolOutput};
use serde_json::Value;
use std::time::Duration;

const MAX_OUTPUT_CHARS: usize = 4000;
const TIMEOUT: Duration = Duration::from_secs(30);

pub struct BashTool;

#[async_trait::async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "在受限目录执行 bash 命令，30 秒超时，输出截断"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要执行的 bash 命令" }
            },
            "required": ["command"]
        })
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    async fn execute(&self, arguments: &Value) -> ToolOutput {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        run_bash(command).await
    }

    /// Worktree isolation: run in the execution cwd when one is set.
    async fn execute_ctx(&self, ctx: &super::ToolCtx<'_>, arguments: &Value) -> ToolOutput {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match ctx.cwd {
            Some(cwd) => run_bash_in(command, Some(cwd)).await,
            None => run_bash(command).await,
        }
    }
}

// ponytail: restricted dir = fixed process cwd, no sandbox/allowlist yet; process-level isolation is a later ticket
pub async fn run_bash(arguments: &str) -> ToolOutput {
    run_bash_in(arguments, None).await
}

/// `run_bash` with an explicit working directory (worktree isolation).
pub async fn run_bash_in(arguments: &str, cwd: Option<&std::path::Path>) -> ToolOutput {
    let command = parse_command(arguments);
    let child = tokio::process::Command::new(crate::shell::bash_executable())
        .arg("-c")
        .arg(command)
        .current_dir(cwd.unwrap_or(std::path::Path::new(".")))
        // Force UTF-8 for child-process output (tools.zh.md §3.6: GB default-codepage corruption must not recur)
        .env("PYTHONIOENCODING", "utf-8")
        .output();
    match tokio::time::timeout(TIMEOUT, child).await {
        Ok(Ok(out)) => {
            let mut text = String::new();
            if !out.stdout.is_empty() {
                text.push_str(&String::from_utf8_lossy(&out.stdout));
            }
            if !out.stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&String::from_utf8_lossy(&out.stderr));
            }
            ToolOutput {
                output: truncate(text),
                exit_code: out.status.code(),
            }
        }
        Ok(Err(e)) => ToolOutput {
            output: format!("启动 bash 失败：{e}"),
            exit_code: None,
        },
        Err(_) => ToolOutput {
            output: "命令超时（30s）被终止".into(),
            exit_code: None,
        },
    }
}

/// Accepts two argument shapes: {"command":"..."} or a bare command string.
fn parse_command(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|v| v.get("command").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| arguments.to_string())
}

fn truncate(s: String) -> String {
    if s.chars().count() > MAX_OUTPUT_CHARS {
        let cut: String = s.chars().take(MAX_OUTPUT_CHARS).collect();
        format!("{cut}\n...[输出已截断]")
    } else {
        s
    }
}
