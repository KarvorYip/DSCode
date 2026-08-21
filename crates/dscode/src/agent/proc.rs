//! Hub processes (tools.zh.md §3.9): long-running watcher / dev-server / REPL processes managed
//! by name — start (ready.log regex + optional TCP port probe, both must pass before returning),
//! ps / logs / stop / restart / describe, plus by-name stdin send and wait.
//! Single-instance first release: cross-instance broker transport (named pipes) is a known gap.
//! stdin interaction and key sequences are minimal (no ConPTY here — that lands with the bash
//! tool's phase 2-3 work); writes go through a plain piped stdin.

use super::{AgentHost, POLL_TICK};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex as AsyncMutex;

/// Output ring cap: 512 KiB per process, trimmed from the front.
const OUT_CAP: usize = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcStatus {
    Starting,
    Running,
    Exited(Option<i32>),
}

impl ProcStatus {
    fn as_str(&self) -> &'static str {
        match self {
            ProcStatus::Starting => "starting",
            ProcStatus::Running => "running",
            ProcStatus::Exited(_) => "exited",
        }
    }
}

pub struct ProcEntry {
    pub name: String,
    pub command: String,
    pub args: Option<Vec<String>>,
    pub cwd: Option<PathBuf>,
    pub ready_log: Option<String>,
    pub ready_port: Option<u16>,
    pub started_at: Instant,
    pub pid: Option<u32>,
    pub status: ProcStatus,
    pub out: Arc<Mutex<Vec<u8>>>,
    pub stdin: Option<Arc<AsyncMutex<tokio::process::ChildStdin>>>,
    pub child: Arc<AsyncMutex<Option<tokio::process::Child>>>,
}

pub struct ProcSpec {
    pub name: String,
    pub command: String,
    pub args: Option<Vec<String>>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub ready_log: Option<String>,
    pub ready_port: Option<u16>,
    pub ready_timeout: Duration,
}

fn lock_out(out: &Mutex<Vec<u8>>) -> std::sync::MutexGuard<'_, Vec<u8>> {
    out.lock().unwrap_or_else(|e| e.into_inner())
}

impl AgentHost {
    pub(crate) fn proc_table(&self) -> MutexGuard<'_, BTreeMap<String, ProcEntry>> {
        self.procs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start a named process; returns only after every supplied readiness condition passes
    /// (ready.log regex on accumulated output + optional TCP port probe — both must pass).
    pub async fn proc_start(&self, spec: ProcSpec) -> Result<Value, String> {
        {
            let table = self.proc_table();
            if let Some(p) = table.get(&spec.name) {
                if !matches!(p.status, ProcStatus::Exited(_)) {
                    return Err(format!(
                        "进程名已被占用（{}），先 stop 或 restart",
                        spec.name
                    ));
                }
            }
        }
        let mut cmd = if let Some(args) = &spec.args {
            let mut c = tokio::process::Command::new(&spec.command);
            c.args(args);
            c
        } else {
            // No-args form: run through bash -c, consistent with the bash tool (git-bash on Windows).
            let mut c = tokio::process::Command::new(crate::shell::bash_executable());
            c.arg("-c").arg(&spec.command);
            c
        };
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Force UTF-8 output encoding for children (tools.zh.md §3.6 GB-codepage pitfall).
        cmd.env("PYTHONIOENCODING", "utf-8");
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd.spawn().map_err(|e| format!("启动进程失败：{e}"))?;
        let pid = child.id();
        let mut stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let out: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let mut readers: Vec<Box<dyn tokio::io::AsyncRead + Unpin + Send>> = Vec::new();
        if let Some(s) = stdout {
            readers.push(Box::new(s));
        }
        if let Some(s) = stderr {
            readers.push(Box::new(s));
        }
        for stream in readers {
            let buf = out.clone();
            tokio::spawn(async move {
                let mut stream = stream;
                let mut chunk = [0u8; 4096];
                loop {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut b = lock_out(&buf);
                            b.extend_from_slice(&chunk[..n]);
                            if b.len() > OUT_CAP {
                                let excess = b.len() - OUT_CAP;
                                b.drain(..excess);
                            }
                        }
                    }
                }
            });
        }
        let child_arc = Arc::new(AsyncMutex::new(Some(child)));
        let stdin_arc = stdin.take().map(|s| Arc::new(AsyncMutex::new(s)));

        self.proc_table().insert(
            spec.name.clone(),
            ProcEntry {
                name: spec.name.clone(),
                command: spec.command.clone(),
                args: spec.args.clone(),
                cwd: spec.cwd.clone(),
                ready_log: spec.ready_log.clone(),
                ready_port: spec.ready_port,
                started_at: Instant::now(),
                pid,
                status: ProcStatus::Starting,
                out: out.clone(),
                stdin: stdin_arc,
                child: child_arc,
            },
        );

        // Readiness probing: all supplied conditions must pass within the window.
        let log_re = spec
            .ready_log
            .as_deref()
            .map(regex::Regex::new)
            .transpose()
            .map_err(|e| format!("ready.log 非法正则：{e}"))?;
        let deadline = Instant::now() + spec.ready_timeout;
        loop {
            if let Some(code) = self.proc_try_exit(&spec.name) {
                let tail = self.proc_tail(&spec.name, 400);
                self.proc_table().remove(&spec.name);
                return Err(format!("进程提前退出（code {code:?}）。输出尾部：\n{tail}"));
            }
            let log_ok = log_re.as_ref().is_none_or(|re| {
                let b = lock_out(&out);
                re.is_match(&String::from_utf8_lossy(&b))
            });
            let port_ok = match spec.ready_port {
                None => true,
                Some(port) => matches!(
                    tokio::time::timeout(
                        Duration::from_millis(300),
                        tokio::net::TcpStream::connect(("127.0.0.1", port)),
                    )
                    .await,
                    Ok(Ok(_))
                ),
            };
            if log_ok && port_ok {
                if let Some(p) = self.proc_table().get_mut(&spec.name) {
                    p.status = ProcStatus::Running;
                }
                return Ok(json!({
                    "name": spec.name,
                    "pid": pid,
                    "ready": true,
                    "readyLog": spec.ready_log,
                    "readyPort": spec.ready_port,
                }));
            }
            if Instant::now() >= deadline {
                self.proc_kill(&spec.name).await;
                let tail = self.proc_tail(&spec.name, 400);
                self.proc_table().remove(&spec.name);
                return Err(format!(
                    "就绪探测超时（log {:?} / port {:?}）。输出尾部：\n{tail}",
                    spec.ready_log, spec.ready_port
                ));
            }
            tokio::time::sleep(POLL_TICK).await;
        }
    }

    /// Poll the child once; records the exit code when it has terminated.
    /// Lock-busy rounds simply skip the check (the caller's deadline still bounds the loop).
    fn proc_try_exit(&self, name: &str) -> Option<Option<i32>> {
        let child_arc = {
            let table = self.proc_table();
            table.get(name).map(|p| p.child.clone())?
        };
        if let Ok(mut guard) = child_arc.try_lock() {
            if let Some(status) = guard.as_mut().and_then(|c| c.try_wait().ok().flatten()) {
                let code = status.code();
                drop(guard);
                if let Some(p) = self.proc_table().get_mut(name) {
                    p.status = ProcStatus::Exited(code);
                }
                return Some(code);
            }
        }
        None
    }

    fn proc_tail(&self, name: &str, chars: usize) -> String {
        let table = self.proc_table();
        match table.get(name) {
            None => String::new(),
            Some(p) => {
                let b = lock_out(&p.out);
                let text = String::from_utf8_lossy(&b);
                let n = text.chars().count();
                if n <= chars {
                    text.into_owned()
                } else {
                    text.chars().skip(n - chars).collect()
                }
            }
        }
    }

    async fn proc_kill(&self, name: &str) -> Option<Option<i32>> {
        let child_arc = {
            let table = self.proc_table();
            table.get(name).map(|p| p.child.clone())?
        };
        let mut guard = child_arc.lock().await;
        if let Some(child) = guard.as_mut() {
            let _ = child.kill().await;
        }
        let code = match guard.as_mut() {
            Some(c) => c.wait().await.ok().and_then(|s| s.code()),
            None => None,
        };
        drop(guard);
        if let Some(p) = self.proc_table().get_mut(name) {
            p.status = ProcStatus::Exited(code);
        }
        Some(code)
    }

    pub async fn proc_stop(&self, name: &str) -> Result<Value, String> {
        let code = self
            .proc_kill(name)
            .await
            .ok_or_else(|| format!("未知进程：{name}"))?;
        Ok(json!({ "name": name, "stopped": true, "exitCode": code }))
    }

    pub async fn proc_restart(&self, name: &str) -> Result<Value, String> {
        let spec = {
            let table = self.proc_table();
            let p = table.get(name).ok_or_else(|| format!("未知进程：{name}"))?;
            ProcSpec {
                name: p.name.clone(),
                command: p.command.clone(),
                args: p.args.clone(),
                cwd: p.cwd.clone(),
                env: Vec::new(),
                ready_log: p.ready_log.clone(),
                ready_port: p.ready_port,
                ready_timeout: Duration::from_secs(30),
            }
        };
        if {
            let table = self.proc_table();
            table
                .get(name)
                .is_some_and(|p| !matches!(p.status, ProcStatus::Exited(_)))
        } {
            self.proc_kill(name).await;
        }
        self.proc_table().remove(name);
        let out = self.proc_start(spec).await?;
        Ok(json!({ "restarted": true, "start": out }))
    }

    pub fn proc_ps(&self) -> Value {
        // Refresh exit states lazily (cheap try_wait per live child).
        let names: Vec<String> = {
            let table = self.proc_table();
            table
                .values()
                .filter(|p| matches!(p.status, ProcStatus::Running | ProcStatus::Starting))
                .map(|p| p.name.clone())
                .collect()
        };
        for n in names {
            self.proc_try_exit(&n);
        }
        let table = self.proc_table();
        let list: Vec<Value> = table
            .values()
            .map(|p| {
                json!({
                    "name": p.name.clone(),
                    "command": p.command.clone(),
                    "pid": p.pid,
                    "status": p.status.as_str(),
                    "exitCode": match p.status { ProcStatus::Exited(c) => c, _ => None::<i32> },
                    "uptimeSecs": p.started_at.elapsed().as_secs(),
                })
            })
            .collect();
        json!({ "processes": list })
    }

    pub fn proc_describe(&self, name: &str) -> Result<Value, String> {
        let table = self.proc_table();
        let p = table.get(name).ok_or_else(|| format!("未知进程：{name}"))?;
        Ok(json!({
            "name": p.name.clone(),
            "command": p.command.clone(),
            "args": p.args.clone(),
            "cwd": p.cwd.clone(),
            "pid": p.pid,
            "status": p.status.as_str(),
            "uptimeSecs": p.started_at.elapsed().as_secs(),
            "ready": { "log": p.ready_log.clone(), "port": p.ready_port },
        }))
    }

    /// logs: cursor-based (byte offset into the retained buffer), optional lines (tail N) /
    /// grep / follow with timeout. head=true forces cursor 0.
    pub async fn proc_logs(&self, name: &str, opts: &Value) -> Result<Value, String> {
        let follow = opts.get("follow").and_then(Value::as_bool).unwrap_or(false);
        let head = opts.get("head").and_then(Value::as_bool).unwrap_or(false);
        let timeout = Duration::from_millis(
            opts.get("timeoutMs")
                .and_then(Value::as_u64)
                .unwrap_or(5000),
        );
        let (out, status) = {
            let table = self.proc_table();
            let p = table.get(name).ok_or_else(|| format!("未知进程：{name}"))?;
            (p.out.clone(), p.status.clone())
        };
        let deadline = Instant::now() + timeout;
        loop {
            let (cursor, text) = {
                let b = lock_out(&out);
                let start = if head {
                    0
                } else {
                    opts.get("cursor").and_then(Value::as_u64).unwrap_or(0) as usize
                };
                // Ring trimming shifts offsets; clamp defensively.
                let start = start.min(b.len());
                let text = String::from_utf8_lossy(&b[start..]).into_owned();
                (b.len(), text)
            };
            if !text.is_empty() || !follow || Instant::now() >= deadline {
                let mut lines: Vec<&str> = text.lines().collect();
                if let Some(g) = opts.get("grep").and_then(Value::as_str) {
                    let re = regex::Regex::new(g).map_err(|e| format!("grep 非法正则：{e}"))?;
                    lines.retain(|l| re.is_match(l));
                }
                if let Some(n) = opts.get("lines").and_then(Value::as_u64) {
                    let n = n as usize;
                    if lines.len() > n {
                        lines.drain(..lines.len() - n);
                    }
                }
                return Ok(json!({
                    "name": name,
                    "cursor": cursor,
                    "lines": lines,
                    "exited": matches!(status, ProcStatus::Exited(_)),
                }));
            }
            tokio::time::sleep(POLL_TICK).await;
        }
    }

    /// Write to the process stdin (text + optional enter); key sequences are a known gap.
    pub async fn proc_send_stdin(
        &self,
        name: &str,
        text: &str,
        enter: bool,
        keys: &[String],
    ) -> Result<Value, String> {
        let stdin = {
            let table = self.proc_table();
            table
                .get(name)
                .ok_or_else(|| format!("未知进程：{name}"))?
                .stdin
                .clone()
                .ok_or_else(|| format!("进程 {name} 无可用 stdin"))?
        };
        use tokio::io::AsyncWriteExt;
        let mut guard = stdin.lock().await;
        let mut payload = String::new();
        for k in keys {
            // ponytail: plain byte sequences, no ConPTY semantics; full key sequences are a known gap
            payload.push_str(match k.as_str() {
                "ENTER" | "CTRL_J" => "\n",
                "TAB" => "\t",
                "ESC" => "\x1b",
                "CTRL_C" => "\x03",
                "CTRL_D" => "\x04",
                other => other,
            });
        }
        if !text.is_empty() {
            payload.push_str(text);
        }
        if enter {
            payload.push('\n');
        }
        guard
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| format!("写 stdin 失败：{e}"))?;
        Ok(
            json!({ "name": name, "sent": payload.len(), "note": "非 PTY stdin；键序列语义有限（Known Gap）" }),
        )
    }

    /// By-name wait: for=ready|exit|pattern; timeout is a normal result.
    pub async fn proc_wait(
        &self,
        name: &str,
        for_what: &str,
        pattern: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, String> {
        let (rlog, rport, out, child_arc) = {
            let table = self.proc_table();
            let p = table.get(name).ok_or_else(|| format!("未知进程：{name}"))?;
            (
                p.ready_log.clone(),
                p.ready_port,
                p.out.clone(),
                p.child.clone(),
            )
        };
        let re = match (for_what, pattern) {
            ("pattern", Some(p)) => {
                Some(regex::Regex::new(p).map_err(|e| format!("pattern 非法正则：{e}"))?)
            }
            ("pattern", None) => return Err("for=pattern 需要 pattern 参数".into()),
            _ => None,
        };
        let log_re = rlog
            .as_deref()
            .map(regex::Regex::new)
            .transpose()
            .map_err(|e| format!("ready.log 非法正则：{e}"))?;
        let deadline = Instant::now() + timeout;
        loop {
            // Exit check (skip on lock-busy rounds; deadline still bounds the loop).
            let exited = if let Ok(mut guard) = child_arc.try_lock() {
                guard
                    .as_mut()
                    .and_then(|c| c.try_wait().ok().flatten())
                    .map(|s| s.code())
            } else {
                None
            };
            if exited.is_some() {
                if let Some(p) = self.proc_table().get_mut(name) {
                    p.status = ProcStatus::Exited(exited.flatten());
                }
            }
            match for_what {
                "exit" => {
                    if let Some(code) = exited {
                        return Ok(json!({ "reason": "exit", "exitCode": code }));
                    }
                }
                "ready" => {
                    if exited.is_none() {
                        let log_ok = log_re.as_ref().is_none_or(|r| {
                            let b = lock_out(&out);
                            r.is_match(&String::from_utf8_lossy(&b))
                        });
                        let port_ok = match rport {
                            None => true,
                            Some(port) => matches!(
                                tokio::time::timeout(
                                    Duration::from_millis(300),
                                    tokio::net::TcpStream::connect(("127.0.0.1", port)),
                                )
                                .await,
                                Ok(Ok(_))
                            ),
                        };
                        if log_ok && port_ok {
                            return Ok(json!({ "reason": "ready" }));
                        }
                    }
                }
                "pattern" => {
                    if let Some(r) = &re {
                        let b = lock_out(&out);
                        if r.is_match(&String::from_utf8_lossy(&b)) {
                            return Ok(json!({ "reason": "pattern" }));
                        }
                    }
                }
                other => return Err(format!("未知 wait 目标「{other}」（ready/exit/pattern）")),
            }
            if Instant::now() >= deadline {
                return Ok(json!({ "reason": "timeout" }));
            }
            tokio::time::sleep(POLL_TICK).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::llm::{AnyProvider, MockSubagent};
    use std::sync::Arc;

    fn host() -> Arc<AgentHost> {
        Arc::new(AgentHost::new(
            Arc::new(Config::default()),
            Arc::new(|_h: Option<&str>| AnyProvider::MockSubagent(MockSubagent::default())),
        ))
    }

    #[tokio::test]
    async fn start就绪log过后返回且可stop() {
        let h = host();
        let out = h
            .proc_start(ProcSpec {
                name: "t1".into(),
                command: "echo READY-SIGNAL && sleep 5".into(),
                args: None,
                cwd: None,
                env: vec![],
                ready_log: Some("READY-SIGNAL".into()),
                ready_port: None,
                ready_timeout: Duration::from_secs(10),
            })
            .await
            .expect("start 应在 ready.log 过后返回");
        assert_eq!(out["ready"], true);
        let ps = h.proc_ps();
        let t1 = ps["processes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "t1")
            .unwrap();
        assert_eq!(t1["status"], "running");
        h.proc_stop("t1").await.unwrap();
        let ps = h.proc_ps();
        let t1 = ps["processes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "t1")
            .unwrap();
        assert_eq!(t1["status"], "exited");
    }

    #[tokio::test]
    async fn start端口就绪探测() {
        let h = host();
        // A real listener on an ephemeral port, separate from the spawned process.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // keep accepting during the probe window
            let _ = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if listener.accept().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        });
        let out = h
            .proc_start(ProcSpec {
                name: "t2".into(),
                command: "sleep 5".into(),
                args: None,
                cwd: None,
                env: vec![],
                ready_log: None,
                ready_port: Some(port),
                ready_timeout: Duration::from_secs(10),
            })
            .await
            .expect("port 探测应通过");
        assert_eq!(out["ready"], true);
        h.proc_stop("t2").await.unwrap();
    }

    #[tokio::test]
    async fn wait_exit与pattern与超时为正常结果() {
        let h = host();
        h.proc_start(ProcSpec {
            name: "t3".into(),
            command: "echo MARKER-XYZ; sleep 5".into(),
            args: None,
            cwd: None,
            env: vec![],
            ready_log: Some("MARKER-XYZ".into()),
            ready_port: None,
            ready_timeout: Duration::from_secs(10),
        })
        .await
        .unwrap();
        let out = h
            .proc_wait("t3", "pattern", Some("MARKER-XYZ"), Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(out["reason"], "pattern");
        // timeout is a normal outcome
        let t = h
            .proc_wait("t3", "exit", None, Duration::from_millis(80))
            .await
            .unwrap();
        assert_eq!(t["reason"], "timeout");
        h.proc_stop("t3").await.unwrap();
        let e = h
            .proc_wait("t3", "exit", None, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(e["reason"], "exit");
    }

    #[tokio::test]
    async fn logs读取与重复名拒绝() {
        let h = host();
        h.proc_start(ProcSpec {
            name: "t4".into(),
            command: "echo LINE-ONE; sleep 5".into(),
            args: None,
            cwd: None,
            env: vec![],
            ready_log: Some("LINE-ONE".into()),
            ready_port: None,
            ready_timeout: Duration::from_secs(10),
        })
        .await
        .unwrap();
        let logs = h.proc_logs("t4", &json!({})).await.unwrap();
        assert!(logs["lines"].to_string().contains("LINE-ONE"));
        // duplicate live name rejected
        let err = h
            .proc_start(ProcSpec {
                name: "t4".into(),
                command: "sleep 5".into(),
                args: None,
                cwd: None,
                env: vec![],
                ready_log: None,
                ready_port: None,
                ready_timeout: Duration::from_secs(5),
            })
            .await
            .unwrap_err();
        assert!(err.contains("占用"), "应拒绝重复名：{err}");
        h.proc_stop("t4").await.unwrap();
    }
}
