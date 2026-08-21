//! Git worktree isolation for `isolated` spawn items (tools.zh.md §3.8): the child works in
//! `<repo>/.dscode-worktrees/<agent-id>`, and its changes are exported as an applicable `.patch`
//! (`git add -A` + `git diff HEAD`). Merge-patch first, branch optional; overlayfs/ProjFS/reflink
//! backends are deferred (tools.zh.md §6).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Worktrees live under `<repo>/.dscode-worktrees/`; the branch is `dscode/<agent-id>`.
pub fn worktree_dir(repo_root: &Path, agent_id: &str) -> PathBuf {
    repo_root.join(".dscode-worktrees").join(agent_id)
}

/// Create a git worktree for the agent at HEAD; returns the worktree path.
pub fn setup(repo_root: &Path, agent_id: &str) -> Result<PathBuf, String> {
    let dir = worktree_dir(repo_root, agent_id);
    if dir.exists() {
        return Err(format!("worktree 已存在：{}", dir.display()));
    }
    let branch = format!("dscode/{agent_id}");
    // A same-named branch can be left behind by an aborted earlier run; reuse-bare is not worth it, just fail loud.
    run_git(
        repo_root,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &dir.to_string_lossy(),
            "HEAD",
        ],
    )
    .map_err(|e| format!("创建 worktree 失败：{e}"))?;
    Ok(dir)
}

/// Export the worktree's changes as a unified patch; Ok(None) = no changes (empty diff).
pub fn finalize(worktree: &Path, patch_path: &Path) -> Result<Option<PathBuf>, String> {
    run_git(worktree, &["add", "-A"]).map_err(|e| format!("暂存变更失败：{e}"))?;
    let out = worktree_git(worktree, &["diff", "HEAD"])?;
    if out.trim().is_empty() {
        return Ok(None);
    }
    if let Some(parent) = patch_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建产物目录失败：{e}"))?;
    }
    std::fs::write(patch_path, &out).map_err(|e| format!("写 patch 失败：{e}"))?;
    Ok(Some(patch_path.to_path_buf()))
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    worktree_git(cwd, args)
}

fn worktree_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("无法启动 git：{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {}：{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn worktree_patch_生成含变更内容() {
        if !git_available() {
            return; // git unavailable in this environment: skip the real run
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        run_git(repo, &["init", "-q"]).unwrap();
        run_git(repo, &["config", "user.email", "t@t"]).unwrap();
        run_git(repo, &["config", "user.name", "t"]).unwrap();
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        run_git(repo, &["add", "-A"]).unwrap();
        run_git(repo, &["commit", "-q", "-m", "init"]).unwrap();

        let wt = setup(repo, "agent-test-1").expect("worktree 创建失败");
        assert!(wt.join("base.txt").exists(), "worktree 应包含仓库文件");
        std::fs::write(wt.join("new-file.txt"), "child change\n").unwrap();

        let patch = repo.join("out.patch");
        let Some(p) = finalize(&wt, &patch).unwrap() else {
            panic!("有变更，patch 不应为空");
        };
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("new-file.txt"), "patch 应含变更文件：{text}");
        assert!(
            text.starts_with("diff --git"),
            "patch 应是 unified diff：{text}"
        );
    }

    #[test]
    fn worktree_无变更时patch为空() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        run_git(repo, &["init", "-q"]).unwrap();
        run_git(repo, &["config", "user.email", "t@t"]).unwrap();
        run_git(repo, &["config", "user.name", "t"]).unwrap();
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        run_git(repo, &["add", "-A"]).unwrap();
        run_git(repo, &["commit", "-q", "-m", "init"]).unwrap();

        let wt = setup(repo, "agent-test-2").unwrap();
        let patch = repo.join("out2.patch");
        assert!(finalize(&wt, &patch).unwrap().is_none(), "无变更应为 None");
    }
}
