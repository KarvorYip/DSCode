//! glob tool: match files and directories by glob pattern (tools.zh.md §3.4).
//! Respects gitignore by default (`require_git(false)`: it also applies in directories without .git), skips hidden files by default;
//! semicolon-separated targets; output is newest-first, grouped by directory, capped at 500 entries.

use super::{Tier, Tool, ToolOutput};
use globset::GlobBuilder;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use std::time::SystemTime;

const MAX_HITS: usize = 500;

fn err(msg: String) -> ToolOutput {
    ToolOutput {
        output: msg,
        exit_code: Some(1),
    }
}

pub struct GlobTool;

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "按 glob 模式列出匹配的文件与目录；分号分隔多个目标；\
         默认尊重 gitignore、跳过隐藏文件（均有开关）；\
         输出 newest-first、按目录分组，上限 500 条"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "glob 模式；分号分隔多个目标" },
                "hidden": { "type": "boolean", "description": "是否包含隐藏文件（默认 false）" },
                "respect_gitignore": { "type": "boolean", "description": "是否尊重 gitignore（默认 true）" }
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
        let show_hidden = arguments
            .get("hidden")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let respect_git = arguments
            .get("respect_gitignore")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let pattern = pattern.to_string();
        match tokio::task::spawn_blocking(move || glob_sync(&pattern, show_hidden, respect_git))
            .await
        {
            Ok(out) => out,
            Err(e) => err(format!("遍历任务失败：{e}")),
        }
    }

    /// Worktree isolation: relative glob targets resolve against the execution cwd.
    async fn execute_ctx(&self, ctx: &super::ToolCtx<'_>, arguments: &Value) -> ToolOutput {
        let Some(cwd) = ctx.cwd else {
            return self.execute(arguments).await;
        };
        let Some(pattern) = arguments.get("pattern").and_then(Value::as_str) else {
            return self.execute(arguments).await;
        };
        let rewritten: Vec<String> = pattern
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|seg| {
                if Path::new(seg).is_absolute() {
                    seg.to_string()
                } else {
                    cwd.join(seg).to_string_lossy().into_owned()
                }
            })
            .collect();
        if rewritten.is_empty() {
            return self.execute(arguments).await;
        }
        let mut args = arguments.clone();
        args["pattern"] = Value::String(rewritten.join(";"));
        self.execute(&args).await
    }
}

fn glob_sync(pattern: &str, show_hidden: bool, respect_git: bool) -> ToolOutput {
    let targets: Vec<&str> = pattern
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if targets.is_empty() {
        return err("pattern 为空".into());
    }
    // (mtime, display path, is-dir); display paths use / separators and no ./ prefix
    let mut hits: Vec<(SystemTime, String, bool)> = vec![];
    let mut seen: HashSet<String> = HashSet::new();
    for pat in targets {
        let norm = pat.replace('\\', "/");
        let glob = match GlobBuilder::new(&norm).literal_separator(true).build() {
            Ok(g) => g.compile_matcher(),
            Err(e) => return err(format!("glob 模式无效（{pat}）：{e}")),
        };
        let root = fixed_root(&norm);
        if !Path::new(&root).exists() {
            continue; // this target's root doesn't exist → no matches, move on to the other targets
        }
        let mut wb = ignore::WalkBuilder::new(&root);
        wb.hidden(!show_hidden).require_git(false);
        if !respect_git {
            wb.git_ignore(false)
                .git_exclude(false)
                .git_global(false)
                .parents(false);
        }
        for entry in wb.build() {
            let Ok(entry) = entry else { continue };
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let mut disp = entry.path().to_string_lossy().replace('\\', "/");
            if let Some(s) = disp.strip_prefix("./") {
                disp = s.to_string();
            }
            // Directories also try the trailing-/ form so patterns like "src/" can list the directory itself
            let matched = glob.is_match(&disp) || (is_dir && glob.is_match(format!("{disp}/")));
            if matched && seen.insert(disp.clone()) {
                // metadata and modified have different error types (walkdir::Error / io::Error); ok() each and combine
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                hits.push((mtime, disp, is_dir));
            }
        }
    }
    if hits.is_empty() {
        return ToolOutput {
            output: "无匹配".into(),
            exit_code: Some(0),
        };
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0)); // newest-first
    let total = hits.len();
    let mut out = String::new();
    let mut current_dir: Option<String> = None;
    for (_, disp, is_dir) in hits.iter().take(MAX_HITS) {
        let (dir, name) = split_parent(disp);
        if current_dir.as_deref() != Some(dir.as_str()) {
            out.push_str(&format!("{dir}/\n"));
            current_dir = Some(dir);
        }
        out.push_str(&format!("  {name}{}\n", if *is_dir { "/" } else { "" }));
    }
    if total > MAX_HITS {
        out.push_str(&format!("…[共 {total} 条，仅显示前 {MAX_HITS} 条]"));
    }
    ToolOutput {
        output: out,
        exit_code: Some(0),
    }
}

/// The fixed prefix before the pattern's first segment containing wildcard metacharacters serves as the walk root; if none, cwd.
fn fixed_root(norm: &str) -> String {
    let mut root: Vec<&str> = vec![];
    for seg in norm.split('/') {
        if seg.chars().any(|c| matches!(c, '*' | '?' | '[' | '{')) {
            break;
        }
        root.push(seg);
    }
    if root.is_empty() {
        ".".into()
    } else {
        root.join("/")
    }
}

fn split_parent(disp: &str) -> (String, String) {
    match disp.rsplit_once('/') {
        Some((d, n)) => (d.to_string(), n.to_string()),
        None => (".".into(), disp.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn glob_默认尊重gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(tmp.path().join("a.log"), "x").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "x").unwrap();
        let base = tmp.path().to_str().unwrap().replace('\\', "/");
        let out = GlobTool
            .execute(&json!({ "pattern": format!("{base}/*") }))
            .await;
        assert!(out.output.contains("b.txt"), "应列出 b.txt：{}", out.output);
        assert!(
            !out.output.contains("a.log"),
            "gitignore 默认应过滤 a.log：{}",
            out.output
        );
    }

    #[tokio::test]
    async fn glob_关闭gitignore后包含被忽略文件() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(tmp.path().join("a.log"), "x").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "x").unwrap();
        let base = tmp.path().to_str().unwrap().replace('\\', "/");
        let out = GlobTool
            .execute(&json!({ "pattern": format!("{base}/*"), "respect_gitignore": false }))
            .await;
        assert!(
            out.output.contains("a.log"),
            "关闭后应包含 a.log：{}",
            out.output
        );
        assert!(out.output.contains("b.txt"));
    }

    #[tokio::test]
    async fn glob_hidden开关() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".hid.txt"), "x").unwrap();
        std::fs::write(tmp.path().join("vis.txt"), "x").unwrap();
        let base = tmp.path().to_str().unwrap().replace('\\', "/");
        let out = GlobTool
            .execute(&json!({ "pattern": format!("{base}/*") }))
            .await;
        assert!(out.output.contains("vis.txt"));
        assert!(
            !out.output.contains(".hid.txt"),
            "默认应跳过隐藏文件：{}",
            out.output
        );
        let out2 = GlobTool
            .execute(&json!({ "pattern": format!("{base}/*"), "hidden": true }))
            .await;
        assert!(
            out2.output.contains(".hid.txt"),
            "hidden=true 应包含隐藏文件：{}",
            out2.output
        );
        assert!(out2.output.contains("vis.txt"));
    }

    #[tokio::test]
    async fn glob_分号多目标() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("b")).unwrap();
        std::fs::write(tmp.path().join("a").join("1.txt"), "x").unwrap();
        std::fs::write(tmp.path().join("b").join("2.txt"), "x").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "x").unwrap();
        let base = tmp.path().to_str().unwrap().replace('\\', "/");
        let out = GlobTool
            .execute(&json!({ "pattern": format!("{base}/a/*.txt;{base}/b/*.txt") }))
            .await;
        assert!(out.output.contains("1.txt"), "应命中目标一：{}", out.output);
        assert!(out.output.contains("2.txt"), "应命中目标二：{}", out.output);
        assert!(
            !out.output.contains("c.txt"),
            "未列出的目标不应出现：{}",
            out.output
        );
    }
}
