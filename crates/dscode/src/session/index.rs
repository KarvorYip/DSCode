//! Session index: <root>/index.json, one entry per session.
//! The index is a cache — deleting it never loses data; it can be fully rebuilt from log headers + scans (see rebuild_index).

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Index entry for one session log (field names follow the session.zh.md index schema).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// Current title; the latest session/title wins; a user-set title is not overwritten.
    #[serde(default)]
    pub title: Option<String>,
    /// The session's runtime working directory; stored verbatim, not hashed.
    #[serde(default)]
    pub cwd: String,
    /// Creation time (RFC3339).
    #[serde(default)]
    pub created_at: String,
    /// Max seq in the log; null for an empty log.
    #[serde(default)]
    pub last_seq: Option<u64>,
    /// Seq of the latest compaction/summary; the model context starts after it.
    #[serde(default)]
    pub compaction_cursor: Option<u64>,
}

/// The full index; entries with the same id are replaced via upsert.
#[derive(Default, Debug, Serialize, Deserialize)]
pub struct Index {
    pub sessions: Vec<Entry>,
}

impl Index {
    /// Load the index; missing or corrupt returns empty (cache semantics, rebuildable).
    pub fn load(path: &Path) -> Index {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建索引目录失败：{e}"))?;
        }
        let text =
            serde_json::to_string_pretty(self).map_err(|e| format!("序列化索引失败：{e}"))?;
        std::fs::write(path, text).map_err(|e| format!("写索引失败：{e}"))
    }

    /// Replace or append one entry; the incremental-maintenance entry point during appends.
    pub fn upsert(&mut self, entry: Entry) {
        match self.sessions.iter_mut().find(|e| e.id == entry.id) {
            Some(slot) => *slot = entry,
            None => self.sessions.push(entry),
        }
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.sessions.iter().find(|e| e.id == id)
    }

    /// Filter by exact directory match ("sessions of this repo"); no fuzzy matching in the first release.
    pub fn list_by_cwd(&self, cwd: &str) -> Vec<&Entry> {
        self.sessions.iter().filter(|e| e.cwd == cwd).collect()
    }
}

/// Read the index from root and filter by cwd.
pub fn list_by_cwd(root: &Path, cwd: &str) -> Result<Vec<Entry>, String> {
    let idx = Index::load(&root.join("index.json"));
    Ok(idx.list_by_cwd(cwd).into_iter().cloned().collect())
}
