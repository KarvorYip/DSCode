//! Session subsystem (session.zh.md): a session = one append-only JSONL event log;
//! every projection (transcript, model context, title, index) is a replay over the log.
//! Storage layout <root>/<YYYY/MM>/<session-id>.jsonl; first line is the durable header, updated only by fork.
//! seq is dense and increasing from 0; any gap found on read must error. Crash-leftover tails are truncated on open and a repair marker appended.

mod events;
pub mod index;

pub use events::{Event, SESSION_FORMAT_VERSION, SURFACE_TYPES};
/// Tests reference the index helpers through `use super::*`; the release build never re-exports them.
#[cfg(test)]
use index::list_by_cwd;
pub use index::Index;

use chrono::Local;
use events::{lookup, validate, Origin};
use index::Entry as IndexEntry;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Durable header: JSON first line of the file, written at creation; only structural operations
/// (fork) update seedLength. cwd is a DSCode-added field — the index claims "every field is
/// rebuildable from the log", so cwd must be recoverable via the header.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Header {
    #[serde(rename = "formatVersion")]
    format_version: u64,
    #[serde(rename = "seedLength")]
    seed_length: u64,
    #[serde(default)]
    cwd: String,
}

/// One model-visible message extracted from a Claude Code JSONL transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportedMessage {
    User(String),
    Assistant(String),
}

/// Production storage root: ~/.dscode/sessions (global under the user home dir; project dirs hold no session data).
pub fn default_root() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|h| h.join(".dscode").join("sessions"))
        .ok_or_else(|| "无法定位用户主目录".to_string())
}

/// Handle for a single session log; single-writer assumption: at any moment one log belongs to only one process.
#[derive(Debug)]
pub struct SessionLog {
    file: File,
    path: PathBuf,
    id: String,
    header: Header,
    /// Next seq to write (max written seq + 1); advances only after a successful write, keeping on-disk density.
    next_seq: u64,
    index: Index,
    index_path: PathBuf,
    /// Own index entry; maintained incrementally in memory as events are appended.
    entry: IndexEntry,
}

impl SessionLog {
    /// Create a new session: <root>/<YYYY/MM>/<id>.jsonl, write the header (seedLength=0), register in the index.
    pub fn create(id: &str, root: &Path) -> Result<Self, String> {
        let path = month_dir(root).join(format!("{id}.jsonl"));
        if path.exists() {
            return Err(format!("会话日志已存在：{}", path.display()));
        }
        let cwd = current_cwd()?;
        let header = Header {
            format_version: SESSION_FORMAT_VERSION,
            seed_length: 0,
            cwd: cwd.clone(),
        };
        write_header(&path, &header)?;
        let entry = IndexEntry {
            id: id.to_string(),
            title: None,
            cwd,
            created_at: now_rfc3339(),
            last_seq: None,
            compaction_cursor: None,
        };
        Self::open_handles(id, root, path, header, 0, entry)
    }

    /// Reopen a session (resume = crash recovery): locate the log, truncate the half-written tail
    /// and append a repair marker, fully validate keys and seq continuity (gaps error),
    /// refresh the index.
    pub fn open(id: &str, root: &Path) -> Result<Self, String> {
        let path = locate(root, id)?;
        let bytes = fs::read(&path).map_err(|e| format!("读会话日志失败：{e}"))?;
        let (header, events, complete_len, tail_len) = parse_log(&bytes, &path)?;
        validate_events(&events)?;
        if tail_len > 0 {
            // Incomplete write: truncate to the last complete event (the log must end with a newline to stay appendable).
            OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_len(complete_len as u64))
                .map_err(|e| format!("截断崩溃尾部失败：{e}"))?;
        }
        let mut entry = build_entry(id, &events);
        entry.cwd = header.cwd.clone();
        let mut log = Self::open_handles(id, root, path, header, events.len() as u64, entry)?;
        if tail_len > 0 {
            // Repair marker: records that a repair happened; do not invent a synthetic turn/end (missing closers are naturally covered by resume semantics).
            log.log("session/repair", json!({ "truncatedBytes": tail_len }));
        }
        Ok(log)
    }

    /// Append one event. Contract: ignorable=false for known dsh keys, ignorable=true for everything
    /// else (DSCode-owned / unregistered keys) — the dsh reader skip guarantee. seq stays dense;
    /// it does not advance on write failure.
    pub fn log(&mut self, kind: &str, data: Value) {
        let ev = Event {
            kind: kind.to_string(),
            seq: self.next_seq,
            time: now_millis(),
            data,
            ignorable: lookup(kind) != Some(Origin::Dsh),
        };
        if append_event(&mut self.file, &ev).is_ok() {
            self.next_seq += 1;
            self.maintain_index(&ev);
        }
    }

    /// Cursor read: return events with seq >= n in order; TUI resume consumes via this cursor instead of re-reading the whole file.
    pub fn read_from(&self, n: u64) -> Result<Vec<Event>, String> {
        let bytes = fs::read(&self.path).map_err(|e| format!("读会话日志失败：{e}"))?;
        let (_, events, _, _) = parse_log(&bytes, &self.path)?;
        validate_events(&events)?;
        Ok(events.into_iter().filter(|e| e.seq >= n).collect())
    }

    /// Full read (read_from(0)).
    pub fn read_all(&self) -> Result<Vec<Event>, String> {
        self.read_from(0)
    }

    /// Export this append-only session as both its original JSONL and a readable Markdown
    /// projection. Export never mutates the log; command events are appended by the caller.
    pub fn export(&mut self, dir: &Path) -> Result<(PathBuf, PathBuf), String> {
        self.file
            .flush()
            .map_err(|e| format!("刷新会话日志失败：{e}"))?;
        fs::create_dir_all(dir).map_err(|e| format!("创建导出目录失败：{e}"))?;
        let jsonl = dir.join(format!("{}.jsonl", self.id));
        fs::copy(&self.path, &jsonl).map_err(|e| format!("导出 JSONL 失败：{e}"))?;

        let markdown = dir.join(format!("{}.md", self.id));
        let mut out = format!("# DSCode 会话 {}\n\n", self.id);
        for ev in self.read_all()? {
            match ev.kind.as_str() {
                "user/message" => {
                    if let Some(text) = ev.data.get("content").and_then(Value::as_str) {
                        out.push_str("## 用户\n\n");
                        out.push_str(text);
                        out.push_str("\n\n");
                    }
                }
                "assistant/message" => {
                    if let Some(text) = ev.data.get("content").and_then(Value::as_str) {
                        out.push_str("## 助手\n\n");
                        out.push_str(text);
                        out.push_str("\n\n");
                    }
                }
                "tool/call" => {
                    let name = ev.data.get("name").and_then(Value::as_str).unwrap_or("?");
                    let args = ev.data.get("arguments").unwrap_or(&Value::Null);
                    out.push_str(&format!(
                        "### 工具调用 `{name}`\n\n```json\n{args}\n```\n\n"
                    ));
                }
                "tool/result" => {
                    if let Some(text) = ev.data.get("output").and_then(Value::as_str) {
                        out.push_str("### 工具结果\n\n```text\n");
                        out.push_str(text);
                        out.push_str("\n```\n\n");
                    }
                }
                "compaction/summary" => {
                    out.push_str("---\n\n*会话在此处压缩；完整历史仍保留于 JSONL。*\n\n")
                }
                _ => {}
            }
        }
        fs::write(&markdown, out).map_err(|e| format!("导出 Markdown 失败：{e}"))?;
        Ok((markdown, jsonl))
    }

    /// fork = seed-prefix replay: copy the source session's prefix up to the latest completed-turn
    /// boundary (cut at the last turn/end, never mid-turn), renumber seq from 0,
    /// header.seedLength = prefix length.
    pub fn fork(source: &str, target: &str, root: &Path) -> Result<Self, String> {
        let src = Self::open(source, root)?;
        let events = src.read_all()?;
        drop(src);
        let prefix_len = events
            .iter()
            .rposition(|e| e.kind == "turn/end")
            .map_or(0, |i| i + 1);
        let prefix = &events[..prefix_len];

        let path = month_dir(root).join(format!("{target}.jsonl"));
        if path.exists() {
            return Err(format!("会话日志已存在：{}", path.display()));
        }
        let cwd = current_cwd()?;
        let header = Header {
            format_version: SESSION_FORMAT_VERSION,
            seed_length: prefix_len as u64,
            cwd: cwd.clone(),
        };
        write_header(&path, &header)?;
        let mut file = open_append(&path)?;
        let mut seq = 0u64;
        for ev in prefix {
            let mut copy = ev.clone();
            copy.seq = seq;
            append_event(&mut file, &copy)?;
            seq += 1;
        }
        let mut entry = build_entry(target, prefix);
        entry.title = None; // A forked session only folds titles from its own suffix (session.zh.md §projections)
        entry.cwd = cwd;
        entry.created_at = now_rfc3339();
        Self::open_handles(target, root, path, header, seq, entry)
    }

    // —— compaction event write API ——
    // TODO(compaction lock semantics): compaction/start is meant to exclude concurrent writers;
    // under the first-release single-writer assumption no lock is taken — take the log lock here when multi-writer coordination arrives.

    /// Take the compaction boundary (compaction excludes concurrent writers; lock semantics see the TODO).
    pub fn compaction_start(&mut self) {
        self.log("compaction/start", json!({}));
    }

    /// Write the summary text (old events stay in the log as-is; only the projection hides them).
    pub fn compaction_summary(&mut self, summary_text: &str) {
        self.log("compaction/summary", json!({ "summary": summary_text }));
    }

    /// Close compaction; may carry an error.
    pub fn compaction_end(&mut self, error: Option<String>) {
        self.log("compaction/end", json!({ "error": error }));
    }

    // —— Projection API: read-only, never mutates the log; two folds at the same cursor are byte-identical (replay determinism) ——

    /// TUI transcript projection: all events (incl. inherited prefix; pre-compaction history hidden behind a divider).
    pub fn transcript(&self) -> Result<Vec<Event>, String> {
        self.read_all()
    }

    /// Model-context projection: surface events after the latest compaction/summary; from the start when no compaction exists.
    pub fn model_context(&self) -> Result<Vec<Event>, String> {
        let events = self.read_all()?;
        let start = events
            .iter()
            .rposition(|e| e.kind == "compaction/summary")
            .map_or(0, |i| i + 1);
        Ok(events[start..]
            .iter()
            .filter(|e| SURFACE_TYPES.contains(&e.kind.as_str()))
            .cloned()
            .collect())
    }

    /// Title-generation input projection: own suffix only (seq >= seedLength); the inherited prefix is not folded.
    pub fn title_input(&self) -> Result<Vec<Event>, String> {
        let seed = self.header.seed_length;
        Ok(self
            .read_all()?
            .into_iter()
            .filter(|e| e.seq >= seed)
            .collect())
    }

    /// No session/title yet within this session's own suffix (seq >= seedLength) — the title
    /// generation condition; titles inherited from a forked prefix don't count as one's own
    /// (session.zh.md §projections).
    pub fn needs_title(&self) -> bool {
        self.read_from(self.header.seed_length)
            .map(|evs| !evs.iter().any(|e| e.kind == "session/title"))
            .unwrap_or(true)
    }

    /// Incrementally maintain this session's index entry after append and persist it (the index is a cache; failure does not block log writes).
    fn maintain_index(&mut self, ev: &Event) {
        self.entry.last_seq = Some(ev.seq);
        match ev.kind.as_str() {
            "session/title" => {
                self.entry.title = ev
                    .data
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "compaction/summary" => self.entry.compaction_cursor = Some(ev.seq),
            _ => {}
        }
        self.index.upsert(self.entry.clone());
        let _ = self.index.save(&self.index_path);
    }

    fn open_handles(
        id: &str,
        root: &Path,
        path: PathBuf,
        header: Header,
        next_seq: u64,
        entry: IndexEntry,
    ) -> Result<Self, String> {
        let index_path = root.join("index.json");
        let mut index = Index::load(&index_path);
        index.upsert(entry.clone());
        let _ = index.save(&index_path);
        Ok(Self {
            file: open_append(&path)?,
            path,
            id: id.to_string(),
            header,
            next_seq,
            index,
            index_path,
            entry,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

/// Read the model-visible user/assistant messages from a Claude Code JSONL transcript.
/// Metadata, progress records, sidechains and tool-only blocks are intentionally ignored.
pub fn import_claude(path: &Path) -> Result<Vec<ImportedMessage>, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("读取 Claude Code 会话失败 {}：{e}", path.display()))?;
    let mut out = Vec::new();
    for (line_no, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| {
            format!(
                "Claude Code 会话 {} 第 {} 行不是合法 JSON：{e}",
                path.display(),
                line_no + 1
            )
        })?;
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let message = value.get("message").unwrap_or(&value);
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .or_else(|| value.get("type").and_then(Value::as_str));
        let Some(role @ ("user" | "assistant")) = role else {
            continue;
        };
        let Some(content) = message.get("content").and_then(claude_text_content) else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        out.push(match role {
            "user" => ImportedMessage::User(content),
            _ => ImportedMessage::Assistant(content),
        });
    }
    if out.is_empty() {
        return Err("Claude Code 会话中没有可导入的用户或助手文本".into());
    }
    Ok(out)
}

fn claude_text_content(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter(|part| {
                    part.get("type")
                        .and_then(Value::as_str)
                        .is_none_or(|kind| kind == "text")
                })
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

/// Rebuild the full index from the logs (the index is a cache: deleting it never loses data).
/// title takes the latest session/title; createdAt takes the first event's time; cwd comes from the header.
pub fn rebuild_index(root: &Path) -> Result<Index, String> {
    let mut index = Index::default();
    for year in fs::read_dir(root).map_err(|e| format!("读会话根目录失败：{e}"))? {
        let year = year.map_err(|e| format!("遍历会话根目录失败：{e}"))?.path();
        if !year.is_dir() {
            continue;
        }
        for month in fs::read_dir(&year).map_err(|e| format!("遍历年份目录失败：{e}"))? {
            let month = month.map_err(|e| format!("遍历年份目录失败：{e}"))?.path();
            if !month.is_dir() {
                continue;
            }
            for file in fs::read_dir(&month).map_err(|e| format!("遍历月份目录失败：{e}"))?
            {
                let path = file.map_err(|e| format!("遍历月份目录失败：{e}"))?.path();
                if path.extension().map_or(true, |e| e != "jsonl") {
                    continue;
                }
                let id = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let bytes = fs::read(&path).map_err(|e| format!("读会话日志失败：{e}"))?;
                let (header, events, _, _) = parse_log(&bytes, &path)?;
                validate_events(&events)?;
                let mut entry = build_entry(&id, &events);
                entry.cwd = header.cwd;
                index.upsert(entry);
            }
        }
    }
    index.save(&root.join("index.json"))?;
    Ok(index)
}

// —— Internal utilities ——

fn month_dir(root: &Path) -> PathBuf {
    root.join(Local::now().format("%Y/%m").to_string())
}

fn current_cwd() -> Result<String, String> {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| format!("取当前工作目录失败：{e}"))
}

fn now_millis() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn millis_to_rfc3339(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

fn open_append(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| format!("打开会话日志失败：{e}"))
}

fn write_header(path: &Path, header: &Header) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("创建会话目录失败：{e}"))?;
    }
    let line = serde_json::to_string(header).map_err(|e| format!("序列化 header 失败：{e}"))?;
    fs::write(path, format!("{line}\n")).map_err(|e| format!("写 header 失败：{e}"))
}

fn append_event(file: &mut File, ev: &Event) -> Result<(), String> {
    let line = serde_json::to_string(ev).map_err(|e| format!("序列化事件失败：{e}"))?;
    writeln!(file, "{line}")
        .and_then(|_| file.flush())
        .map_err(|e| format!("写事件失败：{e}"))
}

/// Locate <root>/*/*/<id>.jsonl (ids are globally unique; year/month dirs exist only for sharding).
fn locate(root: &Path, id: &str) -> Result<PathBuf, String> {
    let name = format!("{id}.jsonl");
    if let Ok(years) = fs::read_dir(root) {
        for year in years.flatten() {
            if let Ok(months) = fs::read_dir(year.path()) {
                for month in months.flatten() {
                    let p = month.path().join(&name);
                    if p.is_file() {
                        return Ok(p);
                    }
                }
            }
        }
    }
    Err(format!("找不到会话日志：{}/*/*/{}", root.display(), name))
}

/// Parse log bytes: first-line header + event lines.
/// Returns (header, events, byte length of the complete region, byte length of the half-line tail).
/// A last line without a trailing newline = incomplete crash write (truncated by the caller); a newline-terminated line that fails to parse = corruption, error out.
fn parse_log(bytes: &[u8], path: &Path) -> Result<(Header, Vec<Event>, usize, usize), String> {
    let Some(nl) = bytes.iter().rposition(|&b| b == b'\n') else {
        return Err(format!(
            "会话日志无完整行（header 不完整）：{}",
            path.display()
        ));
    };
    let complete = &bytes[..=nl];
    let tail = &bytes[nl + 1..];
    let text = std::str::from_utf8(complete)
        .map_err(|e| format!("会话日志非 UTF-8：{}（{e}）", path.display()))?;
    let mut lines = text.split('\n').filter(|l| !l.is_empty());
    let header_line = lines
        .next()
        .ok_or_else(|| format!("会话日志缺少 header 行：{}", path.display()))?;
    let header: Header = serde_json::from_str(header_line)
        .map_err(|e| format!("header 解析失败（{}）：{e}", path.display()))?;
    if header.format_version != SESSION_FORMAT_VERSION {
        return Err(format!(
            "不支持的会话格式版本：{}（期望 {SESSION_FORMAT_VERSION}）",
            header.format_version
        ));
    }
    let all: Vec<&str> = lines.collect();
    let mut events = Vec::new();
    let mut bad_tail = 0usize;
    for (i, line) in all.iter().enumerate() {
        match serde_json::from_str::<Event>(line) {
            Ok(ev) => events.push(ev),
            Err(e) => {
                // A failed JSON parse of the last line is also treated as an incomplete crash write (session.zh.md §resume): truncate.
                // A parse failure on a middle line is corruption — error out.
                if i + 1 == all.len() {
                    bad_tail = line.len() + 1;
                } else {
                    return Err(format!("第 {} 行事件解析失败：{e}", i + 2));
                }
            }
        }
    }
    Ok((
        header,
        events,
        complete.len() - bad_tail,
        tail.len() + bad_tail,
    ))
}

/// Key validation (unknown key without ignorable:true errors, naming the key) + seq dense-from-0 continuity validation (gaps error).
fn validate_events(events: &[Event]) -> Result<(), String> {
    for (i, ev) in events.iter().enumerate() {
        validate(ev)?;
        if ev.seq != i as u64 {
            return Err(format!(
                "seq 缺口：位置 {i} 期望 seq {i}，实得 {}（键 \"{}\"）",
                ev.seq, ev.kind
            ));
        }
    }
    Ok(())
}

/// Rebuild an index entry by scanning the log (cwd is filled in by the caller from the header).
fn build_entry(id: &str, events: &[Event]) -> IndexEntry {
    IndexEntry {
        id: id.to_string(),
        title: events
            .iter()
            .rev()
            .find(|e| e.kind == "session/title")
            .and_then(|e| e.data.get("title").and_then(Value::as_str))
            .map(str::to_string),
        cwd: String::new(),
        created_at: events
            .first()
            .map(|e| millis_to_rfc3339(e.time))
            .unwrap_or_default(),
        last_seq: events.last().map(|e| e.seq),
        compaction_cursor: events
            .iter()
            .rev()
            .find(|e| e.kind == "compaction/summary")
            .map(|e| e.seq),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-write a log file directly under root/2000/01/ (header + lines), for corruption / unknown-key cases.
    fn craft_log(root: &Path, id: &str, cwd: &str, lines: &[String]) -> PathBuf {
        let dir = root.join("2000").join("01");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        let mut text = format!(r#"{{"formatVersion":0,"seedLength":0,"cwd":"{cwd}"}}"#);
        text.push('\n');
        for l in lines {
            text.push_str(l);
            text.push('\n');
        }
        fs::write(&path, text).unwrap();
        path
    }

    fn raw_line(kind: &str, seq: u64, ignorable: bool) -> String {
        serde_json::to_string(&Event {
            kind: kind.to_string(),
            seq,
            time: 1755234481000,
            data: json!({}),
            ignorable,
        })
        .unwrap()
    }

    #[test]
    fn envelope_字段往返保真() {
        let root = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create("rt", root.path()).unwrap();
        log.log("turn/start", json!({ "turn": 1 }));
        log.log("task/write", json!({ "todos": [{ "text": "a" }] }));
        let events = log.read_all().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "turn/start");
        assert_eq!(events[0].seq, 0);
        assert!(events[0].time > 0);
        assert_eq!(events[0].data, json!({ "turn": 1 }));
        assert!(!events[0].ignorable, "已知 dsh 键 ignorable=false");
        // Own keys are ignorable=true
        assert_eq!(events[1].kind, "task/write");
        assert!(events[1].ignorable, "DSCode 自有键 ignorable=true");
        // Serialization round-trip is field-by-field identical
        for ev in &events {
            let s = serde_json::to_string(ev).unwrap();
            let back: Event = serde_json::from_str(&s).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), s);
        }
        // Still faithful after reopening
        drop(log);
        let reopened = SessionLog::open("rt", root.path()).unwrap();
        assert_eq!(reopened.read_all().unwrap().len(), 2);
    }

    #[test]
    fn 未知键且必需时读取报错点名键() {
        let root = tempfile::tempdir().unwrap();
        craft_log(root.path(), "bad", "", &[raw_line("mystery/x", 0, false)]);
        let err = SessionLog::open("bad", root.path()).unwrap_err();
        assert!(err.contains("mystery/x"), "报错须点名未知键：{err}");
        // Unknown keys with ignorable:true are readable (dsh reader skip contract)
        craft_log(
            root.path(),
            "skip",
            "",
            &[
                raw_line("mystery/x", 0, true),
                raw_line("turn/start", 1, false),
            ],
        );
        let log = SessionLog::open("skip", root.path()).unwrap();
        assert_eq!(log.read_all().unwrap().len(), 2);
    }

    #[test]
    fn 崩溃半行截断并追加修复标记() {
        let root = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create("crash", root.path()).unwrap();
        log.log("turn/start", json!({ "turn": 1 }));
        log.log("user/message", json!({ "text": "hi" }));
        log.log("turn/end", json!({ "turn": 1 }));
        let path = log.path.clone();
        drop(log);
        // Simulate a mid-write crash: leave a half line without a trailing newline at the tail
        let half = r#"{"type":"assistant/chunk","seq":3,"time":1"#;
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        write!(f, "{half}").unwrap();
        drop(f);
        // Reopen: truncate + repair marker + normal load
        let recovered = SessionLog::open("crash", root.path()).unwrap();
        let events = recovered.read_all().unwrap();
        assert_eq!(events.len(), 4, "3 个完整事件 + 1 个修复标记");
        assert_eq!(events[3].kind, "session/repair");
        assert_eq!(events[3].seq, 3, "修复标记续接稠密 seq");
        assert!(events[3].ignorable);
        assert_eq!(
            events[3].data.get("truncatedBytes").and_then(Value::as_u64),
            Some(half.len() as u64)
        );
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.last() == Some(&b'\n'), "恢复后日志以换行结尾");
        // Appends stay dense after repair
        let mut recovered = recovered;
        recovered.log("turn/start", json!({ "turn": 2 }));
        assert_eq!(recovered.read_all().unwrap()[4].seq, 4);
    }

    #[test]
    fn seq缺口加载失败() {
        let root = tempfile::tempdir().unwrap();
        craft_log(
            root.path(),
            "gap",
            "",
            &[
                raw_line("turn/start", 0, false),
                raw_line("turn/end", 2, false),
            ],
        );
        let err = SessionLog::open("gap", root.path()).unwrap_err();
        assert!(err.contains("缺口"), "缺口须报错：{err}");
    }

    #[test]
    fn read_from_游标按序返回() {
        let root = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create("cur", root.path()).unwrap();
        for i in 0..5 {
            log.log("assistant/chunk", json!({ "i": i }));
        }
        let tail = log.read_from(3).unwrap();
        assert_eq!(
            tail.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![3, 4],
            "恰返回 seq>=n 按序"
        );
        assert!(log.read_from(9).unwrap().is_empty());
    }

    #[test]
    fn fork_复制完成回合前缀_重编号_seedLength() {
        let root = tempfile::tempdir().unwrap();
        let mut src = SessionLog::create("src", root.path()).unwrap();
        // Two complete turns + one interrupted turn (no turn/end; must never be cut into the new log)
        for turn in 1..=2 {
            src.log("turn/start", json!({ "turn": turn }));
            src.log("user/message", json!({ "text": format!("u{turn}") }));
            src.log(
                "assistant/message",
                json!({ "content": format!("a{turn}") }),
            );
            src.log("turn/end", json!({ "turn": turn }));
        }
        src.log("turn/start", json!({ "turn": 3 }));
        src.log("user/message", json!({ "text": "u3" }));
        drop(src);

        let mut child = SessionLog::fork("src", "child", root.path()).unwrap();
        let prefix = child.read_all().unwrap();
        assert_eq!(prefix.len(), 8, "前缀恰含两个完成回合（4 事件/回合）");
        assert_eq!(
            prefix.iter().map(|e| e.seq).collect::<Vec<_>>(),
            (0..8).collect::<Vec<_>>(),
            "seq 从 0 重编号且稠密"
        );
        assert_eq!(prefix.last().unwrap().kind, "turn/end", "切点在 turn/end");
        assert_eq!(child.header.seed_length, 8, "seedLength=前缀长度");
        assert!(
            prefix.iter().all(|e| e.data != json!({ "text": "u3" })),
            "中断回合不进前缀"
        );

        // Projections treat only the suffix as the session's own events
        assert!(
            child.title_input().unwrap().is_empty(),
            "inherited prefix 不属标题输入"
        );
        child.log("session/title", json!({ "title": "子会话" }));
        let title_events = child.title_input().unwrap();
        assert_eq!(title_events.len(), 1);
        assert_eq!(title_events[0].seq, 8, "子会话自己的事件续接稠密 seq");

        // Both sessions independently resumable; the source is unaffected
        drop(child);
        let src_back = SessionLog::open("src", root.path()).unwrap();
        assert_eq!(src_back.read_all().unwrap().len(), 10);
        assert!(SessionLog::open("child", root.path()).is_ok());
    }

    #[test]
    fn 模型上下文_从最新摘要之后开始() {
        let root = tempfile::tempdir().unwrap();
        // No compaction: from the start (surface filter)
        let mut log = SessionLog::create("m1", root.path()).unwrap();
        log.log("user/message", json!({ "text": "a" }));
        log.log("assistant/chunk", json!({ "text": "…" })); // not a surface type
        log.log("assistant/message", json!({ "content": "b" }));
        let ctx = log.model_context().unwrap();
        assert_eq!(
            ctx.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            vec!["user/message", "assistant/message"]
        );
        // With compaction: after the latest summary
        log.compaction_start();
        log.compaction_summary("此前对话摘要");
        log.compaction_end(None);
        log.log("user/message", json!({ "text": "c" }));
        log.log(
            "tool/result",
            json!({ "tool_call_id": "t1", "output": "o" }),
        );
        let ctx = log.model_context().unwrap();
        assert_eq!(
            ctx.iter().map(|e| e.data.clone()).collect::<Vec<_>>(),
            vec![
                json!({ "text": "c" }),
                json!({ "tool_call_id": "t1", "output": "o" })
            ],
            "仅最新 summary 之后的 surface 事件"
        );
        // The transcript still contains pre-compaction history (hiding is projection-local)
        assert!(log.transcript().unwrap().len() > ctx.len());
        // The index cursor points at the latest summary
        assert_eq!(log.entry.compaction_cursor, Some(4));
    }

    #[test]
    fn 投影确定性_两次折叠逐字节一致() {
        let root = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create("det", root.path()).unwrap();
        log.log("user/message", json!({ "text": "a" }));
        log.log("assistant/message", json!({ "content": "b" }));
        log.compaction_start();
        log.compaction_summary("摘要");
        log.compaction_end(Some("x".into()));
        log.log("user/message", json!({ "text": "c" }));
        type Fold = fn(&SessionLog) -> Vec<Event>;
        let folds: [Fold; 3] = [
            |l: &SessionLog| l.model_context().unwrap(),
            |l: &SessionLog| l.title_input().unwrap(),
            |l: &SessionLog| l.transcript().unwrap(),
        ];
        for fold in folds {
            let a = serde_json::to_string(&fold(&log)).unwrap();
            let b = serde_json::to_string(&fold(&log)).unwrap();
            assert_eq!(a, b, "同游标两次 fold 逐字节一致");
        }
        // Cursor reads are equally deterministic
        let a = serde_json::to_string(&log.read_from(2).unwrap()).unwrap();
        let b = serde_json::to_string(&log.read_from(2).unwrap()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn 索引按cwd精确过滤与重建() {
        let root = tempfile::tempdir().unwrap();
        let mut a = SessionLog::create("a", root.path()).unwrap();
        a.log("turn/start", json!({ "turn": 1 }));
        a.log("session/title", json!({ "title": "标题A" }));
        drop(a);
        let mut b = SessionLog::create("b", root.path()).unwrap();
        b.log("turn/start", json!({ "turn": 1 }));
        drop(b);

        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mine = list_by_cwd(root.path(), &cwd).unwrap();
        assert_eq!(mine.len(), 2, "同 cwd 两会话均在索引");
        let ea = mine.iter().find(|e| e.id == "a").unwrap();
        assert_eq!(ea.title.as_deref(), Some("标题A"), "session/title 最新者胜");
        assert_eq!(ea.last_seq, Some(1), "lastSeq 随追加前进");
        assert!(mine.iter().all(|e| e.cwd == cwd), "cwd 原文直存");

        // Hand-written foreign-cwd log → delete the index → after rebuild, exact filtering with no cross-boundary leakage
        craft_log(
            root.path(),
            "other",
            "Z:/elsewhere",
            &[raw_line("turn/start", 0, false)],
        );
        fs::remove_file(root.path().join("index.json")).unwrap();
        rebuild_index(root.path()).unwrap();
        assert_eq!(
            list_by_cwd(root.path(), "Z:/elsewhere").unwrap().len(),
            1,
            "重建后异 cwd 会话可按其 cwd 精确命中"
        );
        let rebuilt = list_by_cwd(root.path(), &cwd).unwrap();
        assert_eq!(rebuilt.len(), 2, "过滤互不越界");
        let ea2 = rebuilt.iter().find(|e| e.id == "a").unwrap();
        assert_eq!(ea2.title.as_deref(), Some("标题A"), "重建复原 title");
    }

    #[test]
    fn 存储布局_年月目录() {
        let root = tempfile::tempdir().unwrap();
        let log = SessionLog::create("lay", root.path()).unwrap();
        let ym = Local::now().format("%Y/%m").to_string();
        let expect = root.path().join(&ym).join("lay.jsonl");
        assert!(expect.is_file(), "会话须落在 <root>/<YYYY/MM>/<id>.jsonl");
        // First-line durable header: formatVersion=0, seedLength=0
        let first = fs::read_to_string(&expect)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        let h: Header = serde_json::from_str(&first).unwrap();
        assert_eq!(h.format_version, 0);
        assert_eq!(h.seed_length, 0);
        assert!(!h.cwd.is_empty());
        drop(log);
    }
    #[test]
    fn 会话导出生成原始jsonl与可读markdown且不改日志() {
        let root = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create("export", root.path()).unwrap();
        log.log("user/message", json!({ "content": "你好" }));
        log.log(
            "assistant/message",
            json!({ "content": "回答", "tool_calls": [] }),
        );
        log.log(
            "tool/call",
            json!({ "name": "read", "arguments": { "path": "a.txt" } }),
        );
        log.log("tool/result", json!({ "output": "ok" }));
        let before = serde_json::to_string(&log.read_all().unwrap()).unwrap();

        let (markdown, jsonl) = log.export(output.path()).unwrap();
        let rendered = fs::read_to_string(markdown).unwrap();
        assert!(
            rendered.contains("## 用户") && rendered.contains("你好"),
            "{rendered}"
        );
        assert!(
            rendered.contains("## 助手") && rendered.contains("回答"),
            "{rendered}"
        );
        assert!(
            rendered.contains("工具调用 `read`") && rendered.contains("a.txt"),
            "{rendered}"
        );
        assert!(
            rendered.contains("工具结果") && rendered.contains("ok"),
            "{rendered}"
        );
        assert_eq!(fs::read(jsonl).unwrap(), fs::read(&log.path).unwrap());
        assert_eq!(
            serde_json::to_string(&log.read_all().unwrap()).unwrap(),
            before,
            "导出不得追加或改写事件"
        );
    }

    #[test]
    fn claude导入仅保留主链用户与助手文本() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            concat!(
                "{\"message\":{\"role\":\"user\",\"content\":\"你好\"}}\n",
                "{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"回答\"},{\"type\":\"tool_use\",\"text\":\"忽略\"}]}}\n",
                "{\"isSidechain\":true,\"message\":{\"role\":\"assistant\",\"content\":\"支线\"}}\n",
                "{\"type\":\"progress\",\"message\":{\"content\":\"进度\"}}\n"
            ),
        )
        .unwrap();
        assert_eq!(
            import_claude(file.path()).unwrap(),
            vec![
                ImportedMessage::User("你好".into()),
                ImportedMessage::Assistant("回答".into())
            ]
        );
    }
}
