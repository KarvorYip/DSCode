//! read tool: the single entry point for reading anything (tools.zh.md §3.1).
//! Phase 1 scope: text files with line numbers + `[file#tag]` line-anchored snapshots, offset/limit line ranges,
//! directory entry listings, http(s) URL fetching; code files over 2000 lines degrade to a declaration-line structural summary.

use super::edit::EditSession;
use super::{Tier, Tool, ToolOutput};
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Line-count threshold that triggers the structural summary (on a full read with no offset/limit given).
const SUMMARY_THRESHOLD: usize = 2000;
/// Declaration keywords: in structural-summary mode, only lines containing these substrings are listed.
const DECL_KEYWORDS: &[&str] = &[
    "fn ",
    "let mut ",
    "struct ",
    "enum ",
    "impl ",
    "trait ",
    "mod ",
    "class ",
    "def ",
    "func ",
    "interface ",
    "type ",
];
/// Echo line cap for URL content, so large pages don't flood the context.
const MAX_URL_LINES: usize = 2000;

/// Line-anchored snapshot tag: first 4 hex of the content's sha1; `edit` uses the same function to check freshness —
/// both sides must share this implementation to keep read→edit round-trips consistent.
pub(crate) fn snapshot_tag(bytes: &[u8]) -> String {
    let d = Sha1::digest(bytes);
    let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
    hex[..4].to_string()
}

fn err(msg: String) -> ToolOutput {
    ToolOutput {
        output: msg,
        exit_code: Some(1),
    }
}

pub struct ReadTool(pub Arc<EditSession>);

impl Default for ReadTool {
    fn default() -> Self {
        Self(Arc::new(EditSession::default()))
    }
}

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "读取文件（带行号与 [file#tag] 快照，供 edit 锚定）、目录、skill:// 资源或 http(s) URL；\
         支持 offset/limit 行区间；超过 2000 行的代码文件返回声明行结构摘要"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件/目录路径（相对 cwd）或 http(s) URL" },
                "offset": { "type": "integer", "description": "起始行号（1 起）" },
                "limit": { "type": "integer", "description": "返回行数" }
            },
            "required": ["path"]
        })
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    async fn execute(&self, arguments: &Value) -> ToolOutput {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return err("缺少参数 path".into());
        };
        if path.starts_with("http://") || path.starts_with("https://") {
            return read_url(path).await;
        }
        // Local files are usually tiny; a synchronous read is fine, not worth a spawn_blocking thread hop
        read_local(path, arguments, &self.0)
    }

    /// Internal scheme routing + worktree cwd resolution (tools.zh.md §3.8):
    /// `agent://<id>[/file]` → the artifacts dir; `history://<id>` → the agent's in-memory
    /// transcript (read-only handle); relative paths resolve against the execution cwd.
    async fn execute_ctx(&self, ctx: &super::ToolCtx<'_>, arguments: &Value) -> ToolOutput {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return err("缺少参数 path".into());
        };
        if path.starts_with("skill://") {
            let project_root = ctx
                .cwd
                .map(Path::to_path_buf)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| Path::new(".").to_path_buf());
            let catalog = super::skill::SkillCatalog::discover(&project_root);
            return match catalog.resolve_uri(path) {
                Ok(real) => read_local(&real.to_string_lossy(), arguments, &self.0),
                Err(error) => err(error),
            };
        }
        if let Some(rest) = path.strip_prefix("history://") {
            return match ctx.agents.history_text(rest) {
                Ok(text) => ToolOutput {
                    output: text,
                    exit_code: Some(0),
                },
                Err(e) => err(e),
            };
        }
        if let Some(rest) = path.strip_prefix("agent://") {
            if rest.split(['/', '\\']).any(|seg| seg == "..") {
                return err(format!("agent:// 路径不允许 .. 逃逸：{path}"));
            }
            let real = ctx.agents.artifacts_dir().join(rest);
            return read_local(&real.to_string_lossy(), arguments, &self.0);
        }
        if ctx.cwd.is_some() && !Path::new(path).is_absolute() {
            let mut args = arguments.clone();
            args["path"] = Value::String(ctx.resolve(path).to_string_lossy().into_owned());
            return self.execute(&args).await;
        }
        self.execute(arguments).await
    }
}

fn read_local(path: &str, arguments: &Value, state: &EditSession) -> ToolOutput {
    let offset = arguments
        .get("offset")
        .and_then(Value::as_u64)
        .map(|v| v as usize);
    let limit = arguments
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| v as usize);
    let p = Path::new(path);
    let Ok(meta) = std::fs::metadata(p) else {
        return err(format!("路径不存在：{path}"));
    };
    if meta.is_dir() {
        return list_dir(path, p);
    }
    let Ok(bytes) = std::fs::read(p) else {
        return err(format!("读取失败：{path}"));
    };
    let tag = snapshot_tag(&bytes);
    let (text, lossy) = match String::from_utf8(bytes) {
        Ok(t) => (t, false),
        Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
    };
    let lines: Vec<&str> = text.lines().collect();
    if offset.is_none() && limit.is_none() && lines.len() > SUMMARY_THRESHOLD {
        let seen = lines.iter().enumerate().filter_map(|(index, line)| {
            DECL_KEYWORDS
                .iter()
                .any(|keyword| line.contains(keyword))
                .then_some(index + 1)
        });
        state.record(p, &tag, &text, seen);
        return ToolOutput {
            output: summarize(path, &tag, &lines, lossy),
            exit_code: Some(0),
        };
    }
    let total = lines.len();
    let start = offset.unwrap_or(1);
    let end = limit
        .map(|l| start + l.saturating_sub(1))
        .unwrap_or(total)
        .min(total);
    let mut out = format!("[{path}#{tag}]\n");
    if lossy {
        out.push_str("（非 UTF-8 内容，按无损替换解码）\n");
    }
    if total == 0 && start == 1 {
        state.record(p, &tag, &text, std::iter::empty());
        return ToolOutput {
            output: out,
            exit_code: Some(0),
        };
    }
    if start == 0 || start > total {
        out.push_str(&format!(
            "offset {start} 非法：行号 1 起，文件共 {total} 行"
        ));
        return ToolOutput {
            output: out,
            exit_code: Some(0),
        };
    }
    state.record(p, &tag, &text, start..=end);
    for (i, line) in lines[start - 1..end].iter().enumerate() {
        out.push_str(&format!("{}:{line}\n", start + i));
    }
    ToolOutput {
        output: out,
        exit_code: Some(0),
    }
}

/// Directory entry listing: directory names get a `/` suffix, alphabetical order.
fn list_dir(path: &str, p: &Path) -> ToolOutput {
    let Ok(rd) = std::fs::read_dir(p) else {
        return err(format!("读取目录失败：{path}"));
    };
    let mut names: Vec<String> = rd
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                format!("{name}/")
            } else {
                name
            }
        })
        .collect();
    names.sort();
    let mut out = format!("[{path}]（目录，{} 个条目）\n", names.len());
    out.push_str(&names.join("\n"));
    ToolOutput {
        output: out,
        exit_code: Some(0),
    }
}

/// Structural summary for large code files: keep only lines containing declaration keywords, with head/tail notes stating what was elided;
/// the model can re-fetch precisely by line number with offset/limit (a simplified take on dsh's "read on demand" idea).
fn summarize(path: &str, tag: &str, lines: &[&str], lossy: bool) -> String {
    let mut out = format!(
        "[{path}#{tag}]（结构摘要：全文 {} 行，仅列声明行；可用 offset/limit 精确重取）\n",
        lines.len()
    );
    if lossy {
        out.push_str("（非 UTF-8 内容，按无损替换解码）\n");
    }
    let mut kept = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if DECL_KEYWORDS.iter().any(|k| line.contains(k)) {
            out.push_str(&format!("{}:{line}\n", i + 1));
            kept += 1;
        }
    }
    out.push_str(&format!(
        "[结构摘要结束：列出 {kept} 行声明，省略 {} 行]",
        lines.len() - kept
    ));
    out
}

/// URL fetching: plain async reqwest with a 30s timeout; the tag is derived from the decoded text and is display-only
/// (URLs carry no edit-anchoring semantics).
async fn read_url(url: &str) -> ToolOutput {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => return err(format!("构建 HTTP 客户端失败：{e}")),
    };
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => return err(format!("读取 URL 失败：{e}")),
    };
    let resp = match resp.error_for_status() {
        Ok(r) => r,
        Err(e) => return err(format!("URL 返回错误状态：{e}")),
    };
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return err(format!("读取 URL 响应失败：{e}")),
    };
    let tag = snapshot_tag(text.as_bytes());
    let mut out = format!("[{url}#{tag}]\n");
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().take(MAX_URL_LINES).enumerate() {
        out.push_str(&format!("{}:{line}\n", i + 1));
    }
    if lines.len() > MAX_URL_LINES {
        out.push_str(&format!(
            "…[URL 内容仅回显前 {MAX_URL_LINES} 行，共 {} 行；暂不支持 URL 分页]",
            lines.len()
        ));
    }
    ToolOutput {
        output: out,
        exit_code: Some(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn read_文本文件带行号与tag头() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let path = f.to_str().unwrap();
        let out = ReadTool::default().execute(&json!({ "path": path })).await;
        let head = out.output.lines().next().unwrap();
        assert!(
            head.starts_with(&format!("[{path}#")),
            "头行应为 [path#tag]：{head}"
        );
        assert!(head.ends_with("]"), "头行应以 ] 收尾：{head}");
        // tag is the first 4 hex of sha1
        let tag = head
            .trim_start_matches('[')
            .trim_end_matches(']')
            .rsplit('#')
            .next()
            .unwrap();
        assert_eq!(tag.len(), 4);
        assert!(tag.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(out.output.contains("1:one"));
        assert!(out.output.contains("3:three"));
        assert_eq!(out.exit_code, Some(0));
    }

    #[tokio::test]
    async fn read_offset_limit行区间() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("b.txt");
        std::fs::write(&f, "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let out = ReadTool::default()
            .execute(&json!({ "path": f.to_str().unwrap(), "offset": 2, "limit": 2 }))
            .await;
        assert!(out.output.contains("2:l2"));
        assert!(out.output.contains("3:l3"));
        assert!(!out.output.contains("1:l1"));
        assert!(!out.output.contains("4:l4"));
    }

    #[tokio::test]
    async fn read_目录返回条目清单() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("x.txt"), "x").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let out = ReadTool::default()
            .execute(&json!({ "path": tmp.path().to_str().unwrap() }))
            .await;
        assert!(out.output.contains("目录"), "应标注目录：{}", out.output);
        assert!(out.output.contains("x.txt"));
        assert!(
            out.output.contains("sub/"),
            "子目录应带 / 后缀：{}",
            out.output
        );
    }

    #[tokio::test]
    async fn read_大文件降级为结构摘要() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("big.rs");
        let mut s = String::new();
        for i in 0..2001 {
            if i % 500 == 0 {
                s.push_str(&format!("fn f{i}() {{}}\n"));
            } else {
                s.push_str(&format!("filler {i}\n"));
            }
        }
        std::fs::write(&f, s).unwrap();
        let out = ReadTool::default()
            .execute(&json!({ "path": f.to_str().unwrap() }))
            .await;
        assert!(
            out.output.contains("结构摘要"),
            "应进入摘要模式：{}",
            out.output.lines().next().unwrap()
        );
        assert!(out.output.contains("1:fn f0()"));
        assert!(out.output.contains("省略"));
        assert!(!out.output.contains("filler 3\n"), "非声明行不应出现");
        // With an offset given, even a large file goes through the precise range, not the summary
        let out2 = ReadTool::default()
            .execute(&json!({ "path": f.to_str().unwrap(), "offset": 3, "limit": 1 }))
            .await;
        assert!(
            out2.output.contains("3:filler 2"),
            "offset 读取应绕过摘要：{}",
            out2.output
        );
    }

    #[tokio::test]
    async fn read_url不可达返回错误() {
        // Port 1 is almost certainly connection refused; no external network needed
        let out = ReadTool::default()
            .execute(&json!({ "path": "http://127.0.0.1:1/x" }))
            .await;
        assert!(out.output.contains("失败"), "应报告失败：{}", out.output);
        assert_eq!(out.exit_code, Some(1));
    }
}
