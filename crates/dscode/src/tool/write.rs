//! write tool: create/overwrite whole files (tools.zh.md §3.2).
//! Whole-content semantics; surgical edits belong to `edit`. Path policy is decided by user approval, not limited inside the tool.

use super::{Tier, Tool, ToolOutput};
use serde_json::Value;
use std::path::Path;

fn err(msg: String) -> ToolOutput {
    ToolOutput {
        output: msg,
        exit_code: Some(1),
    }
}

pub struct WriteTool;

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "新建或整文件覆写；父目录自动创建；返回写入字节数"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "目标文件路径（相对 cwd）" },
                "content": { "type": "string", "description": "完整文件内容" }
            },
            "required": ["path", "content"]
        })
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    async fn execute(&self, arguments: &Value) -> ToolOutput {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return err("缺少参数 path".into());
        };
        let Some(content) = arguments.get("content").and_then(Value::as_str) else {
            return err("缺少参数 content".into());
        };
        let p = Path::new(path);
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return err(format!("创建父目录失败：{e}"));
                }
            }
        }
        match std::fs::write(p, content) {
            Ok(()) => ToolOutput {
                output: format!("已写入 {path}：{} 字节", content.len()),
                exit_code: Some(0),
            },
            Err(e) => err(format!("写入失败：{e}")),
        }
    }

    /// Worktree isolation: relative paths resolve against the execution cwd (tools.zh.md §3.8).
    async fn execute_ctx(&self, ctx: &super::ToolCtx<'_>, arguments: &Value) -> ToolOutput {
        if let Some(p) = arguments.get("path").and_then(Value::as_str) {
            if ctx.cwd.is_some() && !Path::new(p).is_absolute() {
                let mut args = arguments.clone();
                args["path"] = Value::String(ctx.resolve(p).to_string_lossy().into_owned());
                return self.execute(&args).await;
            }
        }
        self.execute(arguments).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn write_新建文件自动建父目录并往返() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a").join("b").join("c.txt");
        let out = WriteTool
            .execute(&json!({ "path": f.to_str().unwrap(), "content": "hello" }))
            .await;
        assert_eq!(out.exit_code, Some(0));
        assert!(
            out.output.contains("5 字节"),
            "应报告字节数：{}",
            out.output
        );
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello");
    }

    #[tokio::test]
    async fn write_覆写旧内容() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("d.txt");
        std::fs::write(&f, "old content").unwrap();
        let out = WriteTool
            .execute(&json!({ "path": f.to_str().unwrap(), "content": "new" }))
            .await;
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "new");
    }
}
