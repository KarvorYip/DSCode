//! edit tool: canonical line-anchored patches with session-bound snapshots and registers.
//! A whole-file tag proves freshness; the shared snapshot store separately proves which
//! lines read/grep exposed before an anchored edit is allowed.

use super::read::snapshot_tag;
use super::{Tier, Tool, ToolOutput};
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Max lines of the new file echoed back after a successful edit; beyond that it truncates and prompts a re-read.
const ECHO_MAX_LINES: usize = 400;

const SNAPSHOT_PATHS: usize = 30;
const SNAPSHOT_VERSIONS: usize = 4;

#[derive(Clone)]
struct Snapshot {
    tag: String,
    text: String,
    seen: BTreeSet<usize>,
}

#[derive(Default)]
struct EditSessionInner {
    snapshots: BTreeMap<PathBuf, VecDeque<Snapshot>>,
    path_order: VecDeque<PathBuf>,
    named_registers: BTreeMap<String, Vec<String>>,
}

/// Session-bound provenance and named-register state shared by read/grep/edit.
#[derive(Default)]
pub struct EditSession {
    inner: Mutex<EditSessionInner>,
}

impl EditSession {
    pub(crate) fn record(
        &self,
        path: &Path,
        tag: &str,
        text: &str,
        seen_lines: impl IntoIterator<Item = usize>,
    ) -> String {
        let key = canonical_snapshot_key(path);
        let tag = tag.to_string();
        let seen: BTreeSet<_> = seen_lines.into_iter().collect();
        let mut inner = self.inner.lock();
        touch_path(&mut inner, &key);
        let history = inner.snapshots.entry(key).or_default();
        if let Some(existing) = history.iter_mut().find(|s| s.tag == tag && s.text == text) {
            existing.seen.extend(seen);
            return tag;
        }
        history.push_front(Snapshot {
            tag: tag.clone(),
            text: text.to_string(),
            seen,
        });
        history.truncate(SNAPSHOT_VERSIONS);
        tag
    }

    fn validate_seen(
        &self,
        path: &Path,
        tag: &str,
        text: &str,
        lines: impl IntoIterator<Item = usize>,
    ) -> Result<(), String> {
        let key = canonical_snapshot_key(path);
        let inner = self.inner.lock();
        let snapshot = inner
            .snapshots
            .get(&key)
            .and_then(|history| history.iter().find(|s| s.tag == tag && s.text == text))
            .ok_or_else(|| "当前会话没有该文件快照；请先用 read 或 grep 查看目标行".to_string())?;
        let unseen: Vec<_> = lines
            .into_iter()
            .filter(|line| !snapshot.seen.contains(line))
            .collect();
        if unseen.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "补丁触及未显示行 {unseen:?}；请先用 read 或 grep 查看这些行"
            ))
        }
    }

    fn named_registers(&self) -> BTreeMap<String, Vec<String>> {
        self.inner.lock().named_registers.clone()
    }

    fn commit_named_registers(&self, registers: BTreeMap<String, Vec<String>>) {
        self.inner.lock().named_registers = registers;
    }

    fn relocate(&self, from: &Path, to: &Path) {
        let from = canonical_snapshot_key(from);
        let to = canonical_snapshot_key(to);
        let mut inner = self.inner.lock();
        if let Some(history) = inner.snapshots.remove(&from) {
            inner.snapshots.insert(to.clone(), history);
            inner.path_order.retain(|path| path != &from && path != &to);
            inner.path_order.push_back(to);
        }
    }
}

fn canonical_snapshot_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .map(|parent| parent.join(path.file_name().unwrap_or_default()))
            .unwrap_or_else(|| path.to_path_buf())
    })
}

fn touch_path(inner: &mut EditSessionInner, key: &PathBuf) {
    inner.path_order.retain(|path| path != key);
    inner.path_order.push_back(key.clone());
    while inner.path_order.len() > SNAPSHOT_PATHS {
        if let Some(evicted) = inner.path_order.pop_front() {
            inner.snapshots.remove(&evicted);
        }
    }
}

/// Patch operations use original, 1-indexed line numbers.
enum Op {
    Replace {
        start: usize,
        end: usize,
        body: Vec<String>,
    },
    Insert {
        after: usize,
        seen_span: Option<(usize, usize)>,
        body: Vec<String>,
    },
    Cut {
        start: usize,
        end: usize,
        register: Option<String>,
    },
    Paste {
        target: PasteTarget,
        register: Option<String>,
    },
}

enum PasteTarget {
    Gap {
        after: usize,
        seen_span: Option<(usize, usize)>,
    },
    Span {
        start: usize,
        end: usize,
    },
}

struct ParsedPatch {
    ops: Vec<Op>,
    move_to: Option<String>,
}

fn err(msg: String) -> ToolOutput {
    ToolOutput {
        output: msg,
        exit_code: Some(1),
    }
}

pub struct EditTool(pub Arc<EditSession>);

impl Default for EditTool {
    fn default() -> Self {
        Self(Arc::new(EditSession::default()))
    }
}

#[async_trait::async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "行锚定补丁：PUT 替换/插入，CUT 删除并捕获寄存器，MV 移动文件；\
         命名寄存器跨本会话 edit 调用持久；只允许修改 read/grep 已显示的行；\
         必须携带当前 [file#tag]，tag 过期需重新 read"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "目标文件路径（相对 cwd）" },
                "tag": { "type": "string", "description": "read/grep 输出头 [file#tag] 中的 4 位 tag" },
                "patch": { "type": "string", "description": "canonical PUT/CUT/MV 补丁文本" }
            },
            "required": ["path", "tag", "patch"]
        })
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    async fn execute(&self, arguments: &Value) -> ToolOutput {
        self.execute_inner(arguments, None)
    }

    async fn execute_ctx(&self, ctx: &super::ToolCtx<'_>, arguments: &Value) -> ToolOutput {
        self.execute_inner(arguments, ctx.cwd)
    }
}

impl EditTool {
    fn execute_inner(&self, arguments: &Value, cwd: Option<&Path>) -> ToolOutput {
        let Some(authored_path) = arguments.get("path").and_then(Value::as_str) else {
            return err("缺少参数 path".into());
        };
        let Some(tag) = arguments.get("tag").and_then(Value::as_str) else {
            return err("缺少参数 tag（来自 read/grep 输出头 [file#tag]）".into());
        };
        let Some(patch) = arguments.get("patch").and_then(Value::as_str) else {
            return err("缺少参数 patch".into());
        };
        let path = match cwd {
            Some(base) if !Path::new(authored_path).is_absolute() => base.join(authored_path),
            _ => PathBuf::from(authored_path),
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return err(format!("读取失败：{}", path.display()));
        };
        let current_tag = snapshot_tag(&bytes);
        if current_tag != tag {
            return err(format!(
                "锚定 tag 已过期：期望 [{}#{tag}]，文件当前为 #{current_tag}。请重新 read 获取新快照后再编辑",
                path.display()
            ));
        }
        let had_trailing = bytes.last() == Some(&b'\n');
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        let parsed = match parse_patch(patch, &path, &lines) {
            Ok(parsed) => parsed,
            Err(error) => return err(error),
        };
        if let Err(error) = self
            .0
            .validate_seen(&path, tag, &text, touched_lines(&parsed.ops))
        {
            return err(error);
        }
        let destination = parsed.move_to.as_ref().map(|dest| {
            let dest = PathBuf::from(dest);
            match cwd {
                Some(base) if !dest.is_absolute() => base.join(dest),
                _ => dest,
            }
        });
        if let Some(dest) = &destination {
            if dest != &path && dest.exists() {
                return err(format!("MV 目标已存在：{}", dest.display()));
            }
            if dest
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .is_some_and(|parent| !parent.exists())
            {
                return err(format!("MV 目标目录不存在：{}", dest.display()));
            }
        }
        if parsed.ops.is_empty() {
            let final_path = destination.expect("仅 MV 补丁必有目标");
            if final_path != path {
                if let Err(error) = std::fs::rename(&path, &final_path) {
                    return err(format!("MV 失败：{error}"));
                }
                self.0.relocate(&path, &final_path);
            }
            return ToolOutput {
                output: format!("已移动文件。\n[{}#{tag}]", final_path.display()),
                exit_code: Some(0),
            };
        }

        let named = self.0.named_registers();
        let (new_lines, named) = match apply(lines.clone(), parsed.ops, named) {
            Ok(result) => result,
            Err(error) => return err(error),
        };
        if let Err(error) = validate_candidate_structure(&path, &lines, &new_lines) {
            return err(error);
        }
        let mut out_text = new_lines.join("\n");
        if had_trailing && !new_lines.is_empty() {
            out_text.push('\n');
        }
        let final_path = destination.unwrap_or_else(|| path.clone());
        let permissions = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.permissions(),
            Err(error) => return err(format!("读取文件权限失败：{error}")),
        };
        if let Err(error) = commit_candidate(&path, &final_path, out_text.as_bytes(), permissions) {
            return err(error);
        }
        if final_path != path {
            self.0.relocate(&path, &final_path);
        }
        self.0.commit_named_registers(named);

        let new_tag = snapshot_tag(out_text.as_bytes());
        let shown = new_lines.len().min(ECHO_MAX_LINES);
        self.0.record(&final_path, &new_tag, &out_text, 1..=shown);
        let mut out = format!(
            "已应用补丁，文件现为 {} 行。\n[{}#{new_tag}]\n",
            new_lines.len(),
            final_path.display()
        );
        for (index, line) in new_lines.iter().take(ECHO_MAX_LINES).enumerate() {
            out.push_str(&format!("{}:{line}\n", index + 1));
        }
        if new_lines.len() > ECHO_MAX_LINES {
            out.push_str(&format!(
                "…[仅回显前 {ECHO_MAX_LINES} 行，共 {} 行；可用 read + offset 重取]",
                new_lines.len()
            ));
        }
        ToolOutput {
            output: out,
            exit_code: Some(0),
        }
    }
}

static SIDECAR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn sidecar_path(path: &Path, kind: &str) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let serial = SIDECAR_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{name}.dscode-{kind}-{}-{serial}",
        std::process::id()
    ))
}

fn commit_candidate(
    source: &Path,
    destination: &Path,
    content: &[u8],
    permissions: std::fs::Permissions,
) -> Result<(), String> {
    let candidate = loop {
        let candidate = sidecar_path(destination, "candidate");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                if let Err(error) = file
                    .write_all(content)
                    .and_then(|_| file.sync_all())
                    .and_then(|_| std::fs::set_permissions(&candidate, permissions.clone()))
                {
                    let _ = std::fs::remove_file(&candidate);
                    return Err(format!("写入候选文件失败：{error}"));
                }
                break candidate;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("创建候选文件失败：{error}")),
        }
    };
    let backup = sidecar_path(source, "backup");
    if let Err(error) = std::fs::rename(source, &backup) {
        let _ = std::fs::remove_file(&candidate);
        return Err(format!("准备原子提交失败：{error}"));
    }
    if let Err(error) = std::fs::rename(&candidate, destination) {
        let rollback = std::fs::rename(&backup, source);
        let _ = std::fs::remove_file(&candidate);
        return match rollback {
            Ok(()) => Err(format!("提交候选文件失败，源文件已恢复：{error}")),
            Err(rollback) => Err(format!(
                "提交候选文件失败且回滚失败：{error}；回滚错误：{rollback}"
            )),
        };
    }
    let _ = std::fs::remove_file(backup);
    Ok(())
}

fn parse_patch(patch: &str, path: &Path, lines: &[String]) -> Result<ParsedPatch, String> {
    let mut groups: Vec<(String, Vec<String>)> = vec![];
    let mut move_to = None;
    for line in patch.lines() {
        if line.starts_with("PUT ") || line.starts_with("CUT ") {
            groups.push((line.to_string(), vec![]));
        } else if let Some(dest) = line.strip_prefix("MV ") {
            if move_to.is_some() {
                return Err("一个文件节只允许一个 MV".into());
            }
            move_to = Some(parse_move_dest(dest)?);
        } else if let Some(body) = line.strip_prefix('+') {
            match groups.last_mut() {
                Some((_, rows)) => rows.push(body.to_string()),
                None => return Err(format!("补丁体行出现在任何 PUT 之前：{line:?}")),
            }
        } else if line.trim().is_empty() {
        } else {
            return Err(format!("无法解析的补丁行：{line:?}"));
        }
    }
    if groups.is_empty() && move_to.is_none() {
        return Err("补丁中没有 PUT、CUT 或 MV 操作".into());
    }

    let mut ops = Vec::with_capacity(groups.len());
    for (directive, body) in groups {
        if let Some(rest) = directive.strip_prefix("CUT ") {
            if !body.is_empty() {
                return Err(format!("CUT 不接受补丁体：{directive}"));
            }
            let (range, register) = split_register(rest.trim_end_matches(':'))?;
            let (start, end) = parse_span(range, path, lines, "CUT")?;
            ops.push(Op::Cut {
                start,
                end,
                register,
            });
            continue;
        }

        let rest = directive.strip_prefix("PUT ").unwrap();
        let had_colon = rest.ends_with(':');
        let (target, register) = split_register(rest.trim_end_matches(':'))?;
        let is_paste = register.is_some() || (!had_colon && body.is_empty());
        if is_paste {
            if had_colon || !body.is_empty() {
                return Err(format!("寄存器 PUT 不接受冒号或补丁体：{directive}"));
            }
            ops.push(Op::Paste {
                target: parse_paste_target(target, path, lines)?,
                register,
            });
        } else {
            if !had_colon {
                return Err(format!("带正文的 PUT 必须以冒号结尾：{directive}"));
            }
            if body.is_empty() {
                return Err(format!("PUT 正文为空；删除请使用 CUT：{directive}"));
            }
            if target.contains(".=") || target.ends_with('*') && !target.starts_with('>') {
                let (start, end) = parse_span(target, path, lines, "PUT")?;
                ops.push(Op::Replace { start, end, body });
            } else {
                let (after, seen_span) = parse_gap(target, path, lines)?;
                ops.push(Op::Insert {
                    after,
                    seen_span,
                    body,
                });
            }
        }
    }
    reject_overlaps(&ops)?;
    Ok(ParsedPatch { ops, move_to })
}

fn split_register(text: &str) -> Result<(&str, Option<String>), String> {
    match text.rsplit_once(" @") {
        Some((target, name)) if !name.is_empty() && !name.contains(char::is_whitespace) => {
            Ok((target.trim(), Some(name.to_string())))
        }
        Some(_) => Err(format!("非法寄存器语法：{text}")),
        None => Ok((text.trim(), None)),
    }
}

fn parse_move_dest(dest: &str) -> Result<String, String> {
    let dest = dest.trim();
    let unquoted = if dest.len() >= 2
        && ((dest.starts_with('"') && dest.ends_with('"'))
            || (dest.starts_with('\'') && dest.ends_with('\'')))
    {
        &dest[1..dest.len() - 1]
    } else {
        dest
    };
    if unquoted.is_empty() {
        Err("MV 缺少目标路径".into())
    } else {
        Ok(unquoted.to_string())
    }
}

fn parse_range(text: &str, total: usize, op: &str) -> Result<(usize, usize), String> {
    let Some((start, end)) = text.split_once(".=") else {
        return Err(format!("无法解析的 {op} 区间：{text}"));
    };
    let (start, end) = (parse_num(start)?, parse_num(end)?);
    if start == 0 || start > end || end > total {
        Err(format!(
            "区间越界：{op} {start}.={end}（文件共 {total} 行）"
        ))
    } else {
        Ok((start, end))
    }
}

fn parse_span(
    text: &str,
    path: &Path,
    lines: &[String],
    op: &str,
) -> Result<(usize, usize), String> {
    if let Some(start) = text.strip_suffix('*') {
        let start = parse_num(start)?;
        let end = block_end(path, lines, start)?;
        Ok((start, end))
    } else {
        parse_range(text, lines.len(), op)
    }
}

fn parse_gap(
    text: &str,
    path: &Path,
    lines: &[String],
) -> Result<(usize, Option<(usize, usize)>), String> {
    let total = lines.len();
    if text == ">$" {
        return Ok((total, (total > 0).then_some((total, total))));
    }
    if let Some(anchor) = text.strip_prefix('>') {
        if let Some(start) = anchor.strip_suffix('*') {
            let start = parse_num(start)?;
            let end = block_end(path, lines, start)?;
            return Ok((end, Some((start, end))));
        }
        let line = parse_num(anchor)?;
        if line == 0 || line > total {
            return Err(format!("插入锚越界：PUT >{line}（文件共 {total} 行）"));
        }
        return Ok((line, Some((line, line))));
    }
    if let Some(line) = text.strip_prefix('<') {
        let line = parse_num(line)?;
        if line == 0 || line > total {
            return Err(format!("插入锚越界：PUT <{line}（文件共 {total} 行）"));
        }
        return Ok((line - 1, Some((line, line))));
    }
    Err(format!("无法解析的 PUT 插入锚：{text}"))
}

fn parse_paste_target(text: &str, path: &Path, lines: &[String]) -> Result<PasteTarget, String> {
    if text.contains(".=") || text.ends_with('*') && !text.starts_with('>') {
        let (start, end) = parse_span(text, path, lines, "PUT")?;
        Ok(PasteTarget::Span { start, end })
    } else {
        let (after, seen_span) = parse_gap(text, path, lines)?;
        Ok(PasteTarget::Gap { after, seen_span })
    }
}

fn block_end(path: &Path, lines: &[String], start: usize) -> Result<usize, String> {
    if start == 0 || start > lines.len() {
        return Err(format!("块锚越界：{start}*（文件共 {} 行）", lines.len()));
    }
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    let first = &lines[start - 1];
    if matches!(extension, "md" | "markdown") {
        let trimmed = first.trim_start();
        let level = trimmed.chars().take_while(|ch| *ch == '#').count();
        if level > 0 && trimmed.chars().nth(level) == Some(' ') {
            for (index, line) in lines.iter().enumerate().skip(start) {
                let next = line.trim_start();
                let next_level = next.chars().take_while(|ch| *ch == '#').count();
                if next_level > 0
                    && next_level <= level
                    && next.chars().nth(next_level) == Some(' ')
                {
                    return Ok(index);
                }
            }
            return Ok(lines.len());
        }
    }

    let indent = first.len() - first.trim_start().len();
    if extension == "py" {
        let header = if first.trim_start().starts_with('@') {
            lines
                .iter()
                .enumerate()
                .skip(start)
                .find(|(_, line)| {
                    line.len() - line.trim_start().len() == indent && line.trim_end().ends_with(':')
                })
                .map(|(index, _)| index + 1)
        } else if first.trim_end().ends_with(':') {
            Some(start)
        } else {
            None
        };
        if let Some(header) = header {
            return Ok(indented_block_end(lines, header, indent));
        }
    }
    let depths = delimiter_depths(lines, extension);
    let base = depths[start - 1].0;
    let decorated = first.trim_start().starts_with("#[") || first.trim_start().starts_with('@');
    let opener = if depths[start - 1].1 > base {
        Some(start - 1)
    } else if decorated {
        (start..lines.len()).find(|index| depths[*index].1 > base)
    } else {
        None
    };
    if let Some(opener) = opener {
        if let Some(closer) = (opener..lines.len()).find(|index| depths[*index].1 == base) {
            return Ok(closer + 1);
        }
        return Err(format!("块锚 {start}* 指向未闭合构造"));
    }
    let end = indented_block_end(lines, start, indent);
    Ok(end.max(start))
}

fn indented_block_end(lines: &[String], start: usize, indent: usize) -> usize {
    let mut end = start;
    for (index, line) in lines.iter().enumerate().skip(start) {
        if line.trim().is_empty() {
            end = index + 1;
            continue;
        }
        let line_indent = line.len() - line.trim_start().len();
        if line_indent <= indent {
            break;
        }
        end = index + 1;
    }
    end
}

fn parse_num(text: &str) -> Result<usize, String> {
    text.trim()
        .parse::<usize>()
        .map_err(|_| format!("行号不是正整数：{text:?}"))
}

fn reject_overlaps(ops: &[Op]) -> Result<(), String> {
    let mut spans: Vec<_> = ops
        .iter()
        .filter_map(|op| match op {
            Op::Replace { start, end, .. }
            | Op::Cut { start, end, .. }
            | Op::Paste {
                target: PasteTarget::Span { start, end },
                ..
            } => Some((*start, *end)),
            _ => None,
        })
        .collect();
    spans.sort_unstable();
    for pair in spans.windows(2) {
        if pair[0].1 >= pair[1].0 {
            return Err(format!(
                "操作区间重叠：[{}, {}] 与 [{}, {}]",
                pair[0].0, pair[0].1, pair[1].0, pair[1].1
            ));
        }
    }
    Ok(())
}

fn touched_lines(ops: &[Op]) -> BTreeSet<usize> {
    let mut lines = BTreeSet::new();
    for op in ops {
        match op {
            Op::Replace { start, end, .. }
            | Op::Cut { start, end, .. }
            | Op::Paste {
                target: PasteTarget::Span { start, end },
                ..
            } => {
                lines.extend(*start..=*end);
            }
            Op::Insert { seen_span, .. }
            | Op::Paste {
                target: PasteTarget::Gap { seen_span, .. },
                ..
            } => {
                if let Some((start, end)) = seen_span {
                    lines.extend(*start..=*end);
                }
            }
        }
    }
    lines
}
fn validate_candidate_structure(
    path: &Path,
    original: &[String],
    candidate: &[String],
) -> Result<(), String> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    const DELIMITED: &[&str] = &[
        "rs", "js", "jsx", "ts", "tsx", "c", "cc", "cpp", "h", "hpp", "java", "go", "cs", "swift",
        "kt",
    ];
    if DELIMITED.contains(&extension) {
        let original_signature = delimiter_signature(original, extension);
        let candidate_signature = delimiter_signature(candidate, extension);
        if original_signature != candidate_signature
            || continuation_layout_valid(original, extension)
                && !continuation_layout_valid(candidate, extension)
        {
            return Err("补丁候选截断了半表达式或语法块；请保持完整构造".into());
        }
    }
    if extension == "py"
        && python_indentation_valid(original)
        && !python_indentation_valid(candidate)
    {
        return Err("补丁候选截断了 Python 缩进块；请保持完整构造".into());
    }
    Ok(())
}

fn delimiter_signature(
    lines: &[String],
    extension: &str,
) -> (
    Vec<char>,
    Vec<char>,
    Option<char>,
    Option<usize>,
    bool,
    usize,
) {
    let scan = scan_delimiters(lines, extension);
    (
        scan.open,
        scan.mismatched,
        scan.quote,
        scan.raw_hashes,
        scan.block_comment,
        scan.template_markers,
    )
}

fn continuation_layout_valid(lines: &[String], extension: &str) -> bool {
    let depths = delimiter_depths(lines, extension);
    for (index, line) in lines.iter().enumerate() {
        let code = line.split("//").next().unwrap_or_default().trim_end();
        let continues = ends_with_continuation(code);
        let rust_needs_terminator = extension == "rs"
            && ["let ", "const ", "static "]
                .iter()
                .any(|prefix| code.trim_start().starts_with(prefix))
            && !code.ends_with(';')
            && !continues;
        if !continues && !rust_needs_terminator {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let Some((next_indent, next)) = next_code_line(lines, index + 1) else {
            return false;
        };
        if rust_needs_terminator {
            if next_indent <= indent && !next.starts_with(['.', '?']) {
                return false;
            }
            continue;
        }
        if next.starts_with([')', ']', '}']) && !code.ends_with(',') {
            return false;
        }
        if depths[index].1 == 0 && next_indent <= indent {
            return false;
        }
    }
    true
}

fn next_code_line(lines: &[String], start: usize) -> Option<(usize, &str)> {
    let mut block_comment = false;
    'lines: for line in lines.iter().skip(start) {
        let indent = line.len() - line.trim_start().len();
        let mut rest = line.trim_start();
        loop {
            if block_comment {
                let Some(end) = rest.find("*/") else {
                    continue 'lines;
                };
                block_comment = false;
                rest = &rest[end + 2..];
            }
            rest = rest.trim_start();
            if rest.is_empty() || rest.starts_with("//") {
                continue 'lines;
            }
            if let Some(after) = rest.strip_prefix("/*") {
                block_comment = true;
                rest = after;
                continue;
            }
            return Some((indent, rest));
        }
    }
    None
}

fn ends_with_continuation(code: &str) -> bool {
    const OPERATORS: &[&str] = &[
        "<<=", ">>=", "..=", "==", "!=", "<=", ">=", "&&", "||", "+=", "-=", "*=", "/=", "%=",
        "&=", "|=", "^=", "<<", ">>", "=>", "..", "::", "+", "-", "*", "/", "%", "&", "|", "^",
        "<", ">", "=", ",", ".",
    ];
    let code = code.trim_end();
    OPERATORS.iter().any(|operator| code.ends_with(operator))
        || code.split_whitespace().last() == Some("as")
}
fn python_indentation_valid(lines: &[String]) -> bool {
    let depths = delimiter_depths(lines, "py");
    let mut indents = vec![0usize];
    let mut opens_block = false;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let current = *indents.last().unwrap();
        if indent > current {
            let continued_expression = depths.get(index).is_some_and(|(before, _)| *before > 0);
            if !opens_block && !continued_expression {
                return false;
            }
            indents.push(indent);
        } else if indent < current {
            while indents.last().is_some_and(|level| *level > indent) {
                indents.pop();
            }
            if indents.last().copied() != Some(indent) {
                return false;
            }
        }
        opens_block = trimmed.ends_with(':');
    }
    true
}

struct DelimiterScan {
    depths: Vec<(i32, i32)>,
    open: Vec<char>,
    mismatched: Vec<char>,
    quote: Option<char>,
    raw_hashes: Option<usize>,
    block_comment: bool,
    template_markers: usize,
}

fn delimiter_depths(lines: &[String], extension: &str) -> Vec<(i32, i32)> {
    scan_delimiters(lines, extension).depths
}

fn scan_delimiters(lines: &[String], extension: &str) -> DelimiterScan {
    let template_interpolation = matches!(extension, "js" | "jsx" | "ts" | "tsx");
    let single_quote_string = matches!(extension, "js" | "jsx" | "ts" | "tsx" | "py");
    let mut depth = 0i32;
    let mut stack = Vec::new();
    let mut mismatched = Vec::new();
    let mut block_comment = false;
    let mut quote = None;
    let mut escaped = false;
    let mut raw_hashes = None;
    let mut template_markers = Vec::new();
    let mut depths = Vec::with_capacity(lines.len());
    for line in lines {
        let before = depth;
        let chars: Vec<char> = line.chars().collect();
        let mut index = 0usize;
        while index < chars.len() {
            let current = chars[index];
            let next = chars.get(index + 1).copied();
            if let Some(hashes) = raw_hashes {
                if current == '"'
                    && chars
                        .get(index + 1..index + 1 + hashes)
                        .is_some_and(|tail| tail.iter().all(|ch| *ch == '#'))
                {
                    raw_hashes = None;
                    index += hashes + 1;
                } else {
                    index += 1;
                }
                continue;
            }
            if block_comment {
                if current == '*' && next == Some('/') {
                    block_comment = false;
                    index += 2;
                    continue;
                }
                index += 1;
                continue;
            }
            if let Some(active_quote) = quote {
                if escaped {
                    escaped = false;
                } else if current == '\\' {
                    escaped = true;
                } else if template_interpolation
                    && active_quote == '`'
                    && current == '$'
                    && next == Some('{')
                {
                    let marker = stack.len();
                    stack.push('{');
                    template_markers.push(marker);
                    depth += 1;
                    quote = None;
                    index += 2;
                    continue;
                } else if current == active_quote {
                    quote = None;
                }
                index += 1;
                continue;
            }
            if current == '/' && next == Some('/') {
                break;
            }
            if current == '/' && next == Some('*') {
                block_comment = true;
                index += 2;
                continue;
            }
            if current == 'r' {
                let hashes = chars[index + 1..]
                    .iter()
                    .take_while(|ch| **ch == '#')
                    .count();
                if chars.get(index + 1 + hashes) == Some(&'"') {
                    raw_hashes = Some(hashes);
                    index += hashes + 2;
                    continue;
                }
            }
            if current == '"'
                || current == '`'
                || current == '\'' && (single_quote_string || is_char_literal(&chars, index))
            {
                quote = Some(current);
            } else if matches!(current, '(' | '[' | '{') {
                stack.push(current);
                depth += 1;
            } else if matches!(current, ')' | ']' | '}') {
                let expected = match current {
                    ')' => '(',
                    ']' => '[',
                    '}' => '{',
                    _ => unreachable!(),
                };
                if stack.last() == Some(&expected) {
                    stack.pop();
                } else {
                    mismatched.push(current);
                }
                depth -= 1;
                if current == '}' && template_markers.last() == Some(&stack.len()) {
                    template_markers.pop();
                    quote = Some('`');
                }
            }
            index += 1;
        }
        depths.push((before, depth));
    }
    DelimiterScan {
        depths,
        open: stack,
        mismatched,
        quote,
        raw_hashes,
        block_comment,
        template_markers: template_markers.len(),
    }
}

fn is_char_literal(chars: &[char], quote: usize) -> bool {
    let Some(first) = chars.get(quote + 1) else {
        return false;
    };
    let closing = if *first != '\\' {
        quote + 2
    } else if chars.get(quote + 2) == Some(&'u') && chars.get(quote + 3) == Some(&'{') {
        let Some(end) = chars[quote + 4..].iter().position(|ch| *ch == '}') else {
            return false;
        };
        quote + 5 + end
    } else {
        quote + 3
    };
    chars.get(closing) == Some(&'\'')
}

fn apply(
    original: Vec<String>,
    ops: Vec<Op>,
    mut named: BTreeMap<String, Vec<String>>,
) -> Result<(Vec<String>, BTreeMap<String, Vec<String>>), String> {
    let mut anonymous = None;
    let mut pending_anonymous_cuts = 0usize;
    let mut resolved = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            Op::Cut {
                start,
                end,
                register,
            } => {
                let captured = original[start - 1..end].to_vec();
                if let Some(register) = register {
                    named.insert(register, captured);
                } else {
                    anonymous = Some(captured);
                    pending_anonymous_cuts += 1;
                }
                resolved.push(Op::Cut {
                    start,
                    end,
                    register: None,
                });
            }
            Op::Paste { target, register } => {
                let body = if let Some(register) = register {
                    named
                        .get(&register)
                        .cloned()
                        .ok_or_else(|| format!("命名寄存器 @{register} 为空"))?
                } else {
                    if pending_anonymous_cuts > 1 {
                        return Err("多个匿名 CUT 使粘贴来源不明确；请使用命名寄存器".into());
                    }
                    pending_anonymous_cuts = 0;
                    anonymous
                        .clone()
                        .ok_or_else(|| "匿名寄存器为空".to_string())?
                };
                match target {
                    PasteTarget::Gap { after, seen_span } => {
                        resolved.push(Op::Insert {
                            after,
                            seen_span,
                            body,
                        });
                    }
                    PasteTarget::Span { start, end } => {
                        resolved.push(Op::Replace { start, end, body });
                    }
                }
            }
            other => resolved.push(other),
        }
    }

    resolved.sort_by(|left, right| {
        let left = (anchor(left), rank(left));
        let right = (anchor(right), rank(right));
        right.cmp(&left)
    });
    let mut lines = original;
    for op in resolved {
        match op {
            Op::Replace { start, end, body } => {
                lines.splice(start - 1..end, body);
            }
            Op::Insert { after, body, .. } => {
                lines.splice(after..after, body);
            }
            Op::Cut { start, end, .. } => {
                lines.drain(start - 1..end);
            }
            Op::Paste { .. } => unreachable!("pastes are resolved before apply"),
        }
    }
    Ok((lines, named))
}

fn anchor(op: &Op) -> usize {
    match op {
        Op::Replace { start, .. } | Op::Cut { start, .. } => *start,
        Op::Insert { after, .. } => *after,
        Op::Paste {
            target: PasteTarget::Gap { after, .. },
            ..
        } => *after,
        Op::Paste {
            target: PasteTarget::Span { start, .. },
            ..
        } => *start,
    }
}

fn rank(op: &Op) -> u8 {
    matches!(op, Op::Insert { .. }).into()
}

#[cfg(test)]
mod tests {
    use super::super::grep::GrepTool;
    use super::super::read::ReadTool;
    use super::*;
    use serde_json::json;

    fn tmp_file(content: &str) -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("t.txt");
        std::fs::write(&f, content).unwrap();
        (tmp, f.to_str().unwrap().to_string())
    }

    async fn fresh_edit(path: &str) -> (String, EditTool) {
        let state = Arc::new(EditSession::default());
        let out = ReadTool(state.clone())
            .execute(&json!({ "path": path }))
            .await;
        let head = out.output.lines().next().unwrap();
        let tag = head
            .trim_start_matches('[')
            .trim_end_matches(']')
            .rsplit('#')
            .next()
            .unwrap()
            .into();
        (tag, EditTool(state))
    }

    #[tokio::test]
    async fn edit_过期tag被拒并要求重读() {
        let (_tmp, path) = tmp_file("a\nb\nc\n");
        let (tag, edit) = fresh_edit(&path).await;
        // The file was externally modified after the snapshot
        std::fs::write(&path, "a\nb\nCHANGED\nc\n").unwrap();
        let out = edit
            .execute(&json!({ "path": path, "tag": tag, "patch": "CUT 2.=2" }))
            .await;
        assert!(out.output.contains("过期"), "应报 tag 过期：{}", out.output);
        assert!(
            out.output.contains("重新 read"),
            "应要求重新 read：{}",
            out.output
        );
        assert_eq!(out.exit_code, Some(1));
        // The file stays as externally modified, untouched by the patch
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a\nb\nCHANGED\nc\n"
        );
    }

    #[tokio::test]
    async fn edit_替换只碰目标行() {
        let (_tmp, path) = tmp_file("l1\nl2\nl3\nl4\nl5\n");
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({
                "path": path, "tag": tag,
                "patch": "PUT 2.=3:\n+NEW2\n+NEW3"
            }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "l1\nNEW2\nNEW3\nl4\nl5\n",
            "只有行 2、3 被替换"
        );
    }

    #[tokio::test]
    async fn edit_插入删除与多操作() {
        let (_tmp, path) = tmp_file("a\nb\nc\nd\n");
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({
                "path": path, "tag": tag,
                "patch": "PUT >1:\n+X\nCUT 3.=4"
            }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
        // PUT >1 inserts X after line 1; CUT 3.=4 deletes original lines 3..4 (c, d)
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nX\nb\n");
    }

    #[tokio::test]
    async fn edit_区间越界报错() {
        let (_tmp, path) = tmp_file("a\nb\n");
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({ "path": path, "tag": tag, "patch": "CUT 9.=10" }))
            .await;
        assert!(out.output.contains("越界"), "应报越界：{}", out.output);
        assert_eq!(out.exit_code, Some(1));
    }

    #[tokio::test]
    async fn edit_mv移动文件并保留新锚点() {
        let (tmp, path) = tmp_file("a\n");
        let (tag, edit) = fresh_edit(&path).await;
        let dest = tmp.path().join("moved.txt");
        let out = edit
            .execute(&json!({
                "path": path,
                "tag": tag,
                "patch": format!("MV \"{}\"", dest.display())
            }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
        assert!(!Path::new(&path).exists());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "a\n");
        assert!(out.output.contains(dest.to_string_lossy().as_ref()));
    }

    #[tokio::test]
    async fn edit_拒绝修改未显示行() {
        let (_tmp, path) = tmp_file("a\nb\nc\nd\n");
        let state = Arc::new(EditSession::default());
        let read = ReadTool(state.clone())
            .execute(&json!({ "path": path, "offset": 2, "limit": 1 }))
            .await;
        let head = read.output.lines().next().unwrap();
        let tag = head
            .trim_start_matches('[')
            .trim_end_matches(']')
            .rsplit('#')
            .next()
            .unwrap();
        let out = EditTool(state)
            .execute(&json!({ "path": path, "tag": tag, "patch": "CUT 4.=4" }))
            .await;
        assert_eq!(out.exit_code, Some(1));
        assert!(out.output.contains("未显示"), "{}", out.output);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\nd\n");
    }

    #[tokio::test]
    async fn edit_允许修改grep已显示行() {
        let (_tmp, path) = tmp_file("a\nb\nc\nd\n");
        let state = Arc::new(EditSession::default());
        let search = GrepTool(state.clone())
            .execute(&json!({ "path": path, "pattern": "^d$" }))
            .await;
        let header = search
            .output
            .lines()
            .find(|line| line.starts_with('['))
            .unwrap();
        let tag = header.rsplit('#').next().unwrap().trim_end_matches(']');
        let out = EditTool(state)
            .execute(&json!({ "path": path, "tag": tag, "patch": "PUT 4.=4:\n+D" }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\nD\n");
    }

    #[tokio::test]
    async fn edit_命名寄存器跨调用跨文件持久() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source.txt");
        let target = tmp.path().join("target.txt");
        std::fs::write(&source, "a\nb\n").unwrap();
        std::fs::write(&target, "x\ny\n").unwrap();
        let state = Arc::new(EditSession::default());
        let read = ReadTool(state.clone());
        let edit = EditTool(state.clone());
        let source_read = read.execute(&json!({ "path": source })).await;
        let source_tag = source_read
            .output
            .lines()
            .next()
            .unwrap()
            .rsplit('#')
            .next()
            .unwrap()
            .trim_end_matches(']');
        let target_read = read.execute(&json!({ "path": target })).await;
        let target_tag = target_read
            .output
            .lines()
            .next()
            .unwrap()
            .rsplit('#')
            .next()
            .unwrap()
            .trim_end_matches(']');

        let cut = edit
            .execute(&json!({ "path": source, "tag": source_tag, "patch": "CUT 2.=2 @row" }))
            .await;
        assert_eq!(cut.exit_code, Some(0), "{}", cut.output);
        let paste = edit
            .execute(&json!({ "path": target, "tag": target_tag, "patch": "PUT >2 @row" }))
            .await;
        assert_eq!(paste.exit_code, Some(0), "{}", paste.output);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x\ny\nb\n");
    }

    #[tokio::test]
    async fn edit_重叠区间拒绝() {
        let (_tmp, path) = tmp_file("a\nb\nc\nd\ne\n");
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({ "path": path, "tag": tag, "patch": "PUT 1.=3:\n+A\nCUT 3.=4" }))
            .await;
        assert!(out.output.contains("重叠"), "应报区间重叠：{}", out.output);
        assert_eq!(out.exit_code, Some(1));
        // Failure paths like out-of-range/overlap never touch the disk
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\nd\ne\n");
    }

    #[tokio::test]
    async fn edit_补丁体空行与字面加号() {
        let (_tmp, path) = tmp_file("a\nb\n");
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({
                "path": path,
                "tag": tag,
                "patch": "PUT <1:\n+\n++literal"
            }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "\n+literal\na\nb\n"
        );
    }

    #[tokio::test]
    async fn edit_匿名寄存器单调用移动() {
        let (_tmp, path) = tmp_file("a\nb\nc\nd\n");
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({ "path": path, "tag": tag, "patch": "CUT 2.=2\nPUT >4" }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nc\nd\nb\n");
    }
    #[tokio::test]
    async fn edit_拒绝截断语法块边界() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.rs");
        std::fs::write(&path, "if ready {\n    run();\n}\n").unwrap();
        let path = path.to_string_lossy().into_owned();
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({ "path": path, "tag": tag, "patch": "CUT 1.=1" }))
            .await;
        assert_eq!(out.exit_code, Some(1));
        assert!(out.output.contains("半表达式"), "{}", out.output);
    }

    #[tokio::test]
    async fn edit_允许等结构替换代码块开头() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.rs");
        std::fs::write(&path, "if ready {\n    run();\n}\n").unwrap();
        let path = path.to_string_lossy().into_owned();
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({ "path": path, "tag": tag, "patch": "PUT 1.=1:\n+if enabled {" }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
    }

    #[tokio::test]
    async fn edit_允许重命名Python块声明() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.py");
        std::fs::write(&path, "def old():\n    return 1\n").unwrap();
        let path = path.to_string_lossy().into_owned();
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({ "path": path, "tag": tag, "patch": "PUT 1.=1:\n+def new():" }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
    }

    #[tokio::test]
    async fn edit_空文件读取后允许首次插入() {
        let (_tmp, path) = tmp_file("");
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({ "path": path, "tag": tag, "patch": "PUT >$:\n+first" }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
    }

    #[tokio::test]
    async fn edit_块锚替换与块后插入() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.rs");
        std::fs::write(&path, "fn old() {\n    old_body();\n}\nafter();\n").unwrap();
        let path = path.to_string_lossy().into_owned();
        let (tag, edit) = fresh_edit(&path).await;
        let replaced = edit
            .execute(&json!({
                "path": path,
                "tag": tag,
                "patch": "PUT 1*:\n+fn new() {\n+    new_body();\n+}"
            }))
            .await;
        assert_eq!(replaced.exit_code, Some(0), "{}", replaced.output);
        let tag = replaced
            .output
            .lines()
            .find(|line| line.starts_with('['))
            .unwrap()
            .rsplit('#')
            .next()
            .unwrap()
            .trim_end_matches(']');
        let inserted = edit
            .execute(&json!({ "path": path, "tag": tag, "patch": "PUT >1*:\n+between();" }))
            .await;
        assert_eq!(inserted.exit_code, Some(0), "{}", inserted.output);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn new() {\n    new_body();\n}\nbetween();\nafter();\n"
        );
    }

    #[tokio::test]
    async fn edit_纯mv保持原始字节() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source.bin");
        let destination = tmp.path().join("destination.bin");
        let bytes = b"a\r\n\xffb\r\n";
        std::fs::write(&source, bytes).unwrap();
        let state = Arc::new(EditSession::default());
        let read = ReadTool(state.clone())
            .execute(&json!({ "path": source }))
            .await;
        let tag = read
            .output
            .lines()
            .next()
            .unwrap()
            .rsplit('#')
            .next()
            .unwrap()
            .trim_end_matches(']');
        let out = EditTool(state)
            .execute(&json!({
                "path": source,
                "tag": tag,
                "patch": format!("MV {}", destination.display())
            }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
        assert!(!source.exists());
        assert_eq!(std::fs::read(destination).unwrap(), bytes);
    }

    #[tokio::test]
    async fn edit_拒绝截断无括号多行表达式() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.rs");
        std::fs::write(&path, "let x = left +\n    right;\nafter();\n").unwrap();
        let path = path.to_string_lossy().into_owned();
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({ "path": path, "tag": tag, "patch": "CUT 2.=2" }))
            .await;
        assert_eq!(out.exit_code, Some(1));
        assert!(out.output.contains("半表达式"), "{}", out.output);
    }

    #[tokio::test]
    async fn edit_纯mv不扩大原快照可见行() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source.txt");
        let destination = tmp.path().join("destination.txt");
        std::fs::write(&source, "seen\nhidden\nstill_hidden\n").unwrap();
        let state = Arc::new(EditSession::default());
        let read = ReadTool(state.clone())
            .execute(&json!({ "path": source, "offset": 1, "limit": 1 }))
            .await;
        let tag = read
            .output
            .lines()
            .next()
            .unwrap()
            .rsplit('#')
            .next()
            .unwrap()
            .trim_end_matches(']');
        let edit = EditTool(state);
        let moved = edit
            .execute(&json!({
                "path": source,
                "tag": tag,
                "patch": format!("MV {}", destination.display())
            }))
            .await;
        assert_eq!(moved.exit_code, Some(0), "{}", moved.output);
        let rejected = edit
            .execute(&json!({ "path": destination, "tag": tag, "patch": "CUT 3.=3" }))
            .await;
        assert_eq!(rejected.exit_code, Some(1));
        assert!(rejected.output.contains("未显示行"), "{}", rejected.output);
    }

    #[tokio::test]
    async fn edit_拒绝截断等号多行表达式() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.rs");
        std::fs::write(
            &path,
            "fn f() {\n    let same=left==\n        right;\n    // done\n}\n",
        )
        .unwrap();
        let path = path.to_string_lossy().into_owned();
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({ "path": path, "tag": tag, "patch": "CUT 3.=3" }))
            .await;
        assert_eq!(out.exit_code, Some(1));
        assert!(out.output.contains("半表达式"), "{}", out.output);
    }

    #[tokio::test]
    async fn edit_跨行模板与raw字符串正文不计分隔符() {
        for (name, content) in [
            ("sample.ts", "const s = `\n{\n`;\n"),
            ("sample.rs", "let s = r#\"\n{\n\"#;\n"),
            ("sample.go", "var s = `${\n{\n`\n"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(name);
            std::fs::write(&path, content).unwrap();
            let path = path.to_string_lossy().into_owned();
            let (tag, edit) = fresh_edit(&path).await;
            let out = edit
                .execute(&json!({ "path": path, "tag": tag, "patch": "PUT 2.=2:\n+text" }))
                .await;
            assert_eq!(out.exit_code, Some(0), "{}: {}", name, out.output);
        }
    }

    #[tokio::test]
    async fn edit_Rust生命周期不被当作字符字面量() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.rs");
        std::fs::write(&path, "fn f<'a>(x: &'a str) {}\n").unwrap();
        let path = path.to_string_lossy().into_owned();
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({
                "path": path,
                "tag": tag,
                "patch": "PUT 1.=1:\n+fn f(x: &str) {}"
            }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
    }

    #[tokio::test]
    async fn edit_JS单引号字符串正文不计分隔符() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.ts");
        std::fs::write(&path, "const s = 'unmatched {';\n").unwrap();
        let path = path.to_string_lossy().into_owned();
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({
                "path": path,
                "tag": tag,
                "patch": "PUT 1.=1:\n+const s = 'plain';"
            }))
            .await;
        assert_eq!(out.exit_code, Some(0), "{}", out.output);
    }

    #[tokio::test]
    async fn edit_拒绝截断Rust_leading_dot链() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.rs");
        std::fs::write(
            &path,
            "fn f() {\n    let x = source()\n        .map(g);\n    consume(x);\n}\n",
        )
        .unwrap();
        let path = path.to_string_lossy().into_owned();
        let (tag, edit) = fresh_edit(&path).await;
        let out = edit
            .execute(&json!({ "path": path, "tag": tag, "patch": "CUT 3.=3" }))
            .await;
        assert_eq!(out.exit_code, Some(1));
        assert!(out.output.contains("半表达式"), "{}", out.output);
    }

    #[tokio::test]
    async fn edit_拒绝引入未闭合词法结构() {
        for (original, replacement) in [
            ("const S: &str = \"ok\";\n", "const S: &str = \"oops;"),
            ("fn f() {}\n", "fn f() {} /*"),
            ("const S: &str = r#\"ok\"#;\n", "const S: &str = r#\"oops"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("sample.rs");
            std::fs::write(&path, original).unwrap();
            let path = path.to_string_lossy().into_owned();
            let (tag, edit) = fresh_edit(&path).await;
            let out = edit
                .execute(&json!({
                    "path": path,
                    "tag": tag,
                    "patch": format!("PUT 1.=1:\n+{replacement}")
                }))
                .await;
            assert_eq!(out.exit_code, Some(1), "{}", out.output);
            assert!(out.output.contains("半表达式"), "{}", out.output);
        }
    }
}
