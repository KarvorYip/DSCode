//! grep tool: multi-root regex search (tools.zh.md §3.5).
//! When the regex engine fails to compile (look-around etc.) it falls back to fancy-regex once; only a failure of both reports an error;
//! the ignore crate walk respects gitignore; skip paginates; a tokio timeout wraps the spawn_blocking synchronous walk.

use super::edit::EditSession;
use super::read::snapshot_tag;
use super::{Tier, Tool, ToolOutput};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_LINES: usize = 1000;
/// Files larger than this many bytes are skipped, to keep large binaries/logs out of the results.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

fn err(msg: String) -> ToolOutput {
    ToolOutput {
        output: msg,
        exit_code: Some(1),
    }
}

pub struct GrepTool(pub Arc<EditSession>);

impl Default for GrepTool {
    fn default() -> Self {
        Self(Arc::new(EditSession::default()))
    }
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "跨多根（分号分隔）正则搜索文件内容，尊重 gitignore；结果行格式 file:line:text；\
         regex 引擎不支持时回退 fancy-regex；skip 分页；默认 30 秒超时"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "正则表达式" },
                "path": { "type": "string", "description": "搜索根；分号分隔多个；默认 cwd" },
                "case_insensitive": { "type": "boolean", "description": "大小写不敏感（默认 false）" },
                "skip": { "type": "integer", "description": "跳过前 N 条匹配，用于分页" },
                "timeout_ms": { "type": "integer", "description": "超时毫秒数（默认 30000）" }
            },
            "required": ["pattern"]
        })
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    async fn execute(&self, arguments: &Value) -> ToolOutput {
        let Some(pattern) = arguments.get("pattern").and_then(Value::as_str) else {
            return err("缺少参数 pattern".into());
        };
        let roots: Vec<String> = arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".")
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let ci = arguments
            .get("case_insensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let skip = arguments.get("skip").and_then(Value::as_u64).unwrap_or(0) as usize;
        let timeout_ms = arguments
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let pattern = pattern.to_string();
        #[cfg(test)]
        let delay_ms = arguments
            .get("_test_delay_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let fut = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            std::thread::sleep(Duration::from_millis(delay_ms));
            grep_sync(&pattern, roots, ci, skip)
        });
        match tokio::time::timeout(Duration::from_millis(timeout_ms.max(1)), fut).await {
            Ok(Ok(run)) => {
                for exposure in run.exposures {
                    self.0.record(
                        &exposure.path,
                        &exposure.tag,
                        &exposure.text,
                        exposure.lines,
                    );
                }
                run.output
            }
            Ok(Err(e)) => err(format!("搜索任务失败：{e}")),
            // ponytail: spawn_blocking cannot be cancelled; authorization is committed only above
            Err(_) => err(format!(
                "搜索超时（{timeout_ms}ms）被终止；请收窄 path 或细化 pattern 后重试"
            )),
        }
    }

    /// Worktree isolation: search roots resolve against the execution cwd; a missing path
    /// defaults to it (an isolated child greps its worktree, not the process cwd).
    async fn execute_ctx(&self, ctx: &super::ToolCtx<'_>, arguments: &Value) -> ToolOutput {
        let Some(cwd) = ctx.cwd else {
            return self.execute(arguments).await;
        };
        let roots: Vec<String> = match arguments.get("path").and_then(Value::as_str) {
            Some(p) => p
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| {
                    if Path::new(s).is_absolute() {
                        s.to_string()
                    } else {
                        cwd.join(s).to_string_lossy().into_owned()
                    }
                })
                .collect(),
            None => vec![cwd.to_string_lossy().into_owned()],
        };
        let mut args = arguments.clone();
        args["path"] = Value::String(roots.join(";"));
        self.execute(&args).await
    }
}

struct GrepMatch {
    path: PathBuf,
    display: String,
    line: usize,
    text: String,
}

struct Exposure {
    path: PathBuf,
    tag: String,
    text: String,
    lines: Vec<usize>,
}

struct GrepRun {
    output: ToolOutput,
    exposures: Vec<Exposure>,
}

fn grep_sync(pattern: &str, roots: Vec<String>, ci: bool, skip: usize) -> GrepRun {
    let matcher = match compile_matcher(pattern, ci) {
        Ok(m) => m,
        Err(e) => {
            return GrepRun {
                output: err(e),
                exposures: vec![],
            };
        }
    };
    let mut total = 0usize;
    let mut shown = Vec::new();
    let mut file_texts: BTreeMap<PathBuf, String> = BTreeMap::new();
    for root in &roots {
        let mut wb = ignore::WalkBuilder::new(Path::new(root));
        wb.require_git(false); // directories without .git also respect .gitignore
        for entry in wb.build() {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            if bytes.contains(&0) {
                continue; // NUL bytes mean binary; skip
            }
            let text = String::from_utf8_lossy(&bytes);
            let mut disp = entry.path().to_string_lossy().replace('\\', "/");
            if let Some(s) = disp.strip_prefix("./") {
                disp = s.to_string();
            }
            let mut page_hit = false;
            for (i, line) in text.lines().enumerate() {
                if matcher(line) {
                    if total >= skip && shown.len() < MAX_LINES {
                        shown.push(GrepMatch {
                            path: entry.path().to_path_buf(),
                            display: disp.clone(),
                            line: i + 1,
                            text: line.to_string(),
                        });
                        page_hit = true;
                    }
                    total += 1;
                }
            }
            if page_hit {
                file_texts.insert(entry.path().to_path_buf(), text.into_owned());
            }
        }
    }
    if total == 0 {
        return GrepRun {
            output: ToolOutput {
                output: "无匹配".into(),
                exit_code: Some(0),
            },
            exposures: vec![],
        };
    }
    if shown.is_empty() {
        return GrepRun {
            output: ToolOutput {
                output: format!("共 {total} 条匹配，skip={skip} 后无剩余；请减小 skip 翻页"),
                exit_code: Some(0),
            },
            exposures: vec![],
        };
    }
    let mut seen: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    for item in &shown {
        seen.entry(item.path.clone()).or_default().push(item.line);
    }
    let mut tags = BTreeMap::new();
    let mut exposures = Vec::new();
    for (path, lines) in seen {
        if let Some(text) = file_texts.get(&path) {
            let tag = snapshot_tag(text.as_bytes());
            tags.insert(path.clone(), tag.clone());
            exposures.push(Exposure {
                path,
                tag,
                text: text.clone(),
                lines,
            });
        }
    }
    let mut out = String::new();
    let mut current: Option<&Path> = None;
    for item in &shown {
        if current != Some(item.path.as_path()) {
            if !out.is_empty() {
                out.push('\n');
            }
            if let Some(tag) = tags.get(&item.path) {
                out.push_str(&format!("[{}#{tag}]\n", item.display));
            }
            current = Some(item.path.as_path());
        }
        out.push_str(&format!("{}:{}:{}\n", item.display, item.line, item.text));
    }
    out.pop();
    if skip > 0 || total > skip + shown.len() {
        out.push_str(&format!(
            "\n[共 {total} 条匹配，跳过前 {skip} 条，显示 {} 条]",
            shown.len()
        ));
    }
    GrepRun {
        output: ToolOutput {
            output: out,
            exit_code: Some(0),
        },
        exposures,
    }
}

/// Prefer the regex engine; on unsupported syntax (look-around etc.) fall back to fancy-regex; if both fail, report both errors.
/// fancy-regex 0.13's builder has no case_insensitive option, so case-insensitivity is expressed with a `(?i)` prefix
/// (the flag applies to the whole rest of the pattern, including the right side of an alternation).
fn compile_matcher(pattern: &str, ci: bool) -> Result<Box<dyn Fn(&str) -> bool + Send>, String> {
    match regex::RegexBuilder::new(pattern)
        .case_insensitive(ci)
        .build()
    {
        Ok(re) => Ok(Box::new(move |s: &str| re.is_match(s))),
        Err(e1) => {
            let expr = if ci {
                format!("(?i){pattern}")
            } else {
                pattern.to_string()
            };
            match fancy_regex::Regex::new(&expr) {
                // fancy-regex's is_match returns a Result; runtime failures (backtracking limits etc.) degrade to non-match
                Ok(re) => Ok(Box::new(move |s: &str| re.is_match(s).unwrap_or(false))),
                Err(e2) => Err(format!(
                    "正则编译失败：regex 引擎：{e1}；fancy-regex 回退：{e2}"
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn grep_多根搜索() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("d1")).unwrap();
        std::fs::create_dir_all(tmp.path().join("d2")).unwrap();
        std::fs::write(tmp.path().join("d1").join("a.txt"), "needle here\nplain\n").unwrap();
        std::fs::write(tmp.path().join("d2").join("b.txt"), "x\nneedle2\n").unwrap();
        let out = GrepTool::default().execute(&json!({
            "pattern": "needle",
            "path": format!("{};{}", tmp.path().join("d1").display(), tmp.path().join("d2").display())
        }))
            .await;
        assert!(
            out.output.contains("a.txt:1:needle here"),
            "应命中根一：{}",
            out.output
        );
        assert!(
            out.output.contains("b.txt:2:needle2"),
            "应命中根二：{}",
            out.output
        );
    }

    #[tokio::test]
    async fn grep_skip分页() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("p.txt");
        std::fs::write(&f, "hit 1\nhit 2\nhit 3\nhit 4\nhit 5\n").unwrap();
        let out = GrepTool::default()
            .execute(&json!({ "pattern": "hit", "path": f.to_str().unwrap(), "skip": 2 }))
            .await;
        assert!(
            out.output.contains("3:hit 3"),
            "skip 后应从第 3 条开始：{}",
            out.output
        );
        assert!(out.output.contains("5:hit 5"));
        assert!(
            !out.output.contains(":hit 1"),
            "前 2 条应被跳过：{}",
            out.output
        );
        assert!(
            out.output.contains("[共 5 条匹配，跳过前 2 条"),
            "应带分页尾注：{}",
            out.output
        );
    }

    #[tokio::test]
    async fn grep_大小写开关() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("c.txt");
        std::fs::write(&f, "Hello\nhello\n").unwrap();
        let out = GrepTool::default()
            .execute(&json!({ "pattern": "hello", "path": f.to_str().unwrap() }))
            .await;
        assert!(out.output.contains(":2:hello"));
        assert!(
            !out.output.contains(":1:Hello"),
            "默认大小写敏感：{}",
            out.output
        );
        let out2 = GrepTool::default()
            .execute(&json!({
                "pattern": "hello", "path": f.to_str().unwrap(), "case_insensitive": true
            }))
            .await;
        assert!(
            out2.output.contains(":1:Hello"),
            "不敏感应两行都中：{}",
            out2.output
        );
        assert!(out2.output.contains(":2:hello"));
    }

    #[tokio::test]
    async fn grep_fancy_regex回退() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("r.txt");
        std::fs::write(&f, "foobar\nfoobaz\n").unwrap();
        // lookahead is syntax the regex engine doesn't support; it should automatically fall back to fancy-regex
        let out = GrepTool::default()
            .execute(&json!({ "pattern": "foo(?=bar)", "path": f.to_str().unwrap() }))
            .await;
        assert_eq!(out.exit_code, Some(0), "回退应成功：{}", out.output);
        assert!(
            out.output.contains("foobar"),
            "lookahead 应命中 foobar：{}",
            out.output
        );
        assert!(
            !out.output.contains("foobaz"),
            "lookahead 不应命中 foobaz：{}",
            out.output
        );
        assert!(!out.output.contains("编译失败"));
    }

    #[tokio::test]
    async fn grep_两个引擎都不支持时报双错() {
        let out = GrepTool::default()
            .execute(&json!({ "pattern": "(?<unclosed" }))
            .await;
        assert!(
            out.output.contains("正则编译失败"),
            "应报编译失败：{}",
            out.output
        );
        assert!(out.output.contains("regex 引擎"));
        assert!(out.output.contains("fancy-regex"));
        assert_eq!(out.exit_code, Some(1));
    }

    #[tokio::test]
    async fn grep_默认尊重gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(tmp.path().join("a.log"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "needle\n").unwrap();
        let out = GrepTool::default()
            .execute(&json!({ "pattern": "needle", "path": tmp.path().to_str().unwrap() }))
            .await;
        assert!(out.output.contains("b.txt:1:needle"));
        assert!(
            !out.output.contains("a.log"),
            "gitignore 过滤的文件不应被搜到：{}",
            out.output
        );
    }

    #[tokio::test]
    async fn grep_超时结果不授权后续编辑() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("slow.txt");
        std::fs::write(&path, "needle\n").unwrap();
        let state = Arc::new(EditSession::default());
        let out = GrepTool(state.clone())
            .execute(&json!({
                "pattern": "needle",
                "path": path,
                "timeout_ms": 1,
                "_test_delay_ms": 25
            }))
            .await;
        assert_eq!(out.exit_code, Some(1));
        assert!(out.output.contains("搜索超时"), "{}", out.output);
        tokio::time::sleep(Duration::from_millis(40)).await;

        let tag = snapshot_tag(&std::fs::read(&path).unwrap());
        let edit = super::super::edit::EditTool(state)
            .execute(&json!({ "path": path, "tag": tag, "patch": "CUT 1.=1" }))
            .await;
        assert_eq!(edit.exit_code, Some(1));
        assert!(edit.output.contains("没有该文件快照"), "{}", edit.output);
    }
}
