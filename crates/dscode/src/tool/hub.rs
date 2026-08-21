//! hub tool (tools.zh.md §3.9): the single coordination surface —
//! messaging (send / send+await / broadcast / wait / inbox / list; mailbox cap 100),
//! jobs (unified four-way wait / cancel / snapshot; timeout is a normal outcome, not an error),
//! processes (start with ready.log regex + optional TCP port probe — both must pass before
//! returning / ps / logs / stop / restart / describe / stdin send / wait).
//! `processes.start` additionally screens its command against the bash pattern table
//! (tools.zh.md §4): critical and pattern deny/prompt refuse to launch — the hub surface has
//! no human decision card, so prompt-class resolves to fail-closed denial (§2.10 semantics).
//! Long-running watchers, dev servers, and REPLs must go through hub processes, never
//! throwaway background bash. Single-instance first release: the cross-instance broker
//! (named-pipe transport on Windows) is a known gap.

use super::{Tier, Tool, ToolCtx, ToolOutput};
use crate::agent::proc::ProcSpec;
use serde_json::{json, Value};
use std::time::Duration;

fn ok_json(v: Value) -> ToolOutput {
    ToolOutput {
        output: serde_json::to_string_pretty(&v).unwrap_or_default(),
        exit_code: Some(0),
    }
}

fn err(msg: impl Into<String>) -> ToolOutput {
    ToolOutput {
        output: msg.into(),
        exit_code: Some(1),
    }
}

pub struct HubTool;

#[async_trait::async_trait]
impl Tool for HubTool {
    fn name(&self) -> &str {
        "hub"
    }

    fn description(&self) -> &str {
        "协调面（子代理消息 / 后台 job / 长跑进程）。op=send（to+message，await:true 一问一答）\
broadcast / wait（按 from 等新消息；带 ids 走 job 四路竞速；带 name 等进程）/ inbox（peek \
不消费）/ list（roster）/ jobs（快照）/ cancel（ids）。processes：op=start（name+command\
[+args]，ready:{log,port,timeout} 双过后返回）/ ps / logs（name, cursor/lines/grep/follow）\
/ stop / restart / describe / send（name+text 写 stdin）/ wait（name, for:ready|exit|pattern）。\
长跑 watcher、dev server、REPL 必须走 hub processes，不得用临时后台 bash"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": { "type": "string", "enum": [
                    "send", "broadcast", "wait", "inbox", "list", "jobs", "cancel",
                    "start", "ps", "logs", "stop", "restart", "describe"
                ], "description": "操作：send/broadcast/wait/inbox/list=消息；jobs/cancel/wait(ids)=job；\
        start/ps/logs/stop/restart/describe/send(name)/wait(name)=进程" },
                "to": { "type": "string", "description": "send：目标 agent id（或 Main）" },
                "from": { "type": "string", "description": "wait：按发送方过滤（可选）" },
                "message": { "type": "string", "description": "send/broadcast：消息正文" },
                "await": { "type": "boolean", "description": "send：true 转为一问一答（等待回复）" },
                "timeoutMs": { "type": "integer", "description": "wait 类操作的窗口；超时是正常结果不是错误" },
                "ids": { "type": "array", "items": { "type": "string" }, "description": "cancel / jobs wait 的 job id 列表" },
                "name": { "type": "string", "description": "进程名（processes 组寻址）" },
                "command": { "type": "string", "description": "start：命令（无 args 时经 bash -c 执行）" },
                "args": { "type": "array", "items": { "type": "string" }, "description": "start：直接 exec 的参数表（给出则不再经 shell）" },
                "cwd": { "type": "string", "description": "start：工作目录" },
                "ready": {
                    "type": "object",
                    "properties": {
                        "log": { "type": "string", "description": "输出正则；命中判就绪" },
                        "port": { "type": "integer", "description": "TCP 探测端口；连通判就绪" },
                        "timeout": { "type": "integer", "description": "就绪窗口秒数，默认 30" }
                    },
                    "description": "就绪判定：给出的条件须全部通过 start 才返回"
                },
                "cursor": { "type": "integer", "description": "logs：字节游标（上次返回的 cursor 起续读）" },
                "lines": { "type": "integer", "description": "logs：只留最后 N 行" },
                "grep": { "type": "string", "description": "logs：正则过滤行" },
                "follow": { "type": "boolean", "description": "logs：等待新输出直到 timeoutMs" },
                "head": { "type": "boolean", "description": "logs：从缓冲开头读" },
                "text": { "type": "string", "description": "processes send：写入 stdin 的文本" },
                "enter": { "type": "boolean", "description": "processes send：补换行（默认 true）" },
                "keys": { "type": "array", "items": { "type": "string" }, "description": "processes send：键序列（有限支持，Known Gap）" },
                "for": { "type": "string", "enum": ["ready", "exit", "pattern"], "description": "processes wait：等待目标" },
                "pattern": { "type": "string", "description": "processes wait：for=pattern 的输出正则" }
            },
            "required": ["op"]
        })
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    async fn execute(&self, _arguments: &Value) -> ToolOutput {
        err("hub 需要执行上下文（协调面仅在回合循环/子代理运行时内可用）")
    }

    async fn execute_ctx(&self, ctx: &ToolCtx<'_>, arguments: &Value) -> ToolOutput {
        dispatch(ctx, arguments).await
    }
}

async fn dispatch(ctx: &ToolCtx<'_>, args: &Value) -> ToolOutput {
    let host = ctx.agents.clone();
    let me = ctx.agent_id;
    let op = args.get("op").and_then(Value::as_str).unwrap_or("");
    let str_of = |k: &str| args.get(k).and_then(Value::as_str);
    let timeout = |def: u64| {
        Duration::from_millis(args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(def))
    };

    match op {
        // ---- messaging ----
        "send" if str_of("name").is_some() => {
            // processes send: stdin write by name.
            let name = str_of("name").unwrap();
            let text = str_of("text").unwrap_or("");
            let enter = args.get("enter").and_then(Value::as_bool).unwrap_or(true);
            let keys: Vec<String> = args
                .get("keys")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            match host.proc_send_stdin(name, text, enter, &keys).await {
                Ok(v) => ok_json(v),
                Err(e) => err(e),
            }
        }
        "send" => {
            let Some(to) = str_of("to") else {
                return err("send 需要 to（agent id）或 name（进程名）");
            };
            let message = str_of("message").unwrap_or("");
            let want_await = args.get("await").and_then(Value::as_bool).unwrap_or(false);
            if want_await {
                match host.await_reply(me, to, message, timeout(30_000)).await {
                    Ok(v) => ok_json(v),
                    Err(e) => err(e),
                }
            } else {
                let receipt = host.send(me, to, message);
                ok_json(json!({ "to": to, "receipt": receipt.as_str(),
                    "detail": match &receipt { crate::agent::SendReceipt::Failed(e) => json!(e), _ => Value::Null } }))
            }
        }
        "broadcast" => {
            let message = str_of("message").unwrap_or("");
            let out = host.broadcast(me, message);
            ok_json(json!(out
                .into_iter()
                .map(|(to, r)| json!({ "to": to, "receipt": r.as_str() }))
                .collect::<Vec<_>>()))
        }
        "wait" => {
            if let Some(ids) = args.get("ids").and_then(Value::as_array) {
                // jobs wait: four-way race, first wins; timeout is a normal result.
                let ids: Vec<String> = ids.iter().filter_map(Value::as_str).map(str::to_string).collect();
                let v = host.job_wait(me, &ids, timeout(10_000)).await;
                ok_json(v)
            } else if let Some(name) = str_of("name") {
                let for_what = str_of("for").unwrap_or("exit");
                match host.proc_wait(name, for_what, str_of("pattern"), timeout(10_000)).await {
                    Ok(v) => ok_json(v),
                    Err(e) => err(e),
                }
            } else {
                match host.wait_inbox(me, str_of("from"), timeout(10_000)).await {
                    crate::agent::WaitOutcome::Message(m) => ok_json(json!({
                        "reason": "message", "from": m.from, "text": m.text,
                    })),
                    crate::agent::WaitOutcome::Timeout => ok_json(json!({ "reason": "timeout" })),
                }
            }
        }
        "inbox" => {
            let msgs = host.inbox_peek(me);
            ok_json(json!(msgs
                .iter()
                .map(|m| json!({ "from": m.from, "text": m.text }))
                .collect::<Vec<_>>()))
        }
        "list" => ok_json(host.roster()),

        // ---- jobs ----
        "jobs" => ok_json(host.roster()["jobs"].clone()),
        "cancel" => {
            let Some(ids) = args.get("ids").and_then(Value::as_array) else {
                return err("cancel 需要 ids");
            };
            let ids: Vec<String> = ids.iter().filter_map(Value::as_str).map(str::to_string).collect();
            let out = host.job_cancel(&ids);
            ok_json(json!(out
                .into_iter()
                .map(|(id, r)| json!({ "id": id, "result": r }))
                .collect::<Vec<_>>()))
        }

        // ---- processes ----
        "start" => {
            let Some(name) = str_of("name") else {
                return err("start 需要 name");
            };
            let Some(command) = str_of("command") else {
                return err("start 需要 command");
            };
            // Bash pattern screening (tools.zh.md §4): critical / deny / prompt refuse to launch.
            if let Err(e) = screen_command(&ctx.config.rules, command) {
                return err(e);
            }
            let ready = args.get("ready");
            let spec = ProcSpec {
                name: name.to_string(),
                command: command.to_string(),
                args: args.get("args").and_then(Value::as_array).map(|a| {
                    a.iter().filter_map(Value::as_str).map(str::to_string).collect()
                }),
                cwd: str_of("cwd").map(std::path::PathBuf::from),
                env: vec![],
                ready_log: ready.and_then(|r| r.get("log")).and_then(Value::as_str).map(str::to_string),
                ready_port: ready
                    .and_then(|r| r.get("port"))
                    .and_then(Value::as_u64)
                    .and_then(|p| u16::try_from(p).ok()),
                ready_timeout: Duration::from_secs(
                    ready
                        .and_then(|r| r.get("timeout"))
                        .and_then(Value::as_u64)
                        .unwrap_or(30),
                ),
            };
            match host.proc_start(spec).await {
                Ok(v) => ok_json(v),
                Err(e) => err(e),
            }
        }
        "ps" => ok_json(host.proc_ps()),
        "logs" => {
            let Some(name) = str_of("name") else {
                return err("logs 需要 name");
            };
            match host.proc_logs(name, args).await {
                Ok(v) => ok_json(v),
                Err(e) => err(e),
            }
        }
        "stop" => {
            let Some(name) = str_of("name") else {
                return err("stop 需要 name");
            };
            match host.proc_stop(name).await {
                Ok(v) => ok_json(v),
                Err(e) => err(e),
            }
        }
        "restart" => {
            let Some(name) = str_of("name") else {
                return err("restart 需要 name");
            };
            match host.proc_restart(name).await {
                Ok(v) => ok_json(v),
                Err(e) => err(e),
            }
        }
        "describe" => {
            let Some(name) = str_of("name") else {
                return err("describe 需要 name");
            };
            match host.proc_describe(name) {
                Ok(v) => ok_json(v),
                Err(e) => err(e),
            }
        }
        other => err(format!(
            "未知 op「{other}」（messaging: send/broadcast/wait/inbox/list；jobs: jobs/cancel/wait+ids；\
processes: start/ps/logs/stop/restart/describe/send+name/wait+name）"
        )),
    }
}

/// processes.start command screening (tools.zh.md §4): reuses the bash pattern table as pure
/// functions — critical always refuses; pattern deny refuses; pattern prompt requires a human,
/// and the hub surface has no card → fail-closed refusal (§2.10 semantics).
pub(crate) fn screen_command(
    rules: &crate::approval::ApprovalRules,
    command: &str,
) -> Result<(), String> {
    let segments = crate::approval::split_compound(command);
    if crate::approval::critical_hit(command, &segments) {
        return Err(format!("critical pattern 命中，拒绝启动进程：{command}"));
    }
    let hit = |pats: &[String]| {
        pats.iter()
            .any(|p| segments.iter().any(|s| crate::approval::glob_match(p, s)))
    };
    if hit(&rules.bash_global.deny) || hit(&rules.bash_project.deny) {
        return Err(format!("bash.patterns deny 命中，拒绝启动进程：{command}"));
    }
    if hit(&rules.bash_global.prompt) || hit(&rules.bash_project.prompt) {
        return Err(format!(
            "bash.patterns prompt 命中（需人工确认）；hub 无决定卡，fail-closed 拒绝：{command}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentHost;
    use crate::approval::{ApprovalRules, BashPatterns};
    use crate::config::Config;
    use crate::llm::{AnyProvider, MockSubagent};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn host() -> Arc<AgentHost> {
        Arc::new(AgentHost::new(
            Arc::new(Config::default()),
            Arc::new(|_h: Option<&str>| AnyProvider::MockSubagent(MockSubagent::default())),
        ))
    }

    fn ctx_for(host: &Arc<AgentHost>) -> ToolCtx<'_> {
        ToolCtx {
            config: &host.config,
            agents: host,
            agent_id: "Main",
            def_name: None,
            depth: 0,
            is_subagent: false,
            cwd: None,
            decisions: Some(BTreeMap::new()),
        }
    }

    fn rules_with(kind: &str, pattern: &str) -> ApprovalRules {
        let mut rules = ApprovalRules::default();
        let pats = BashPatterns {
            allow: vec![],
            deny: if kind == "deny" {
                vec![pattern.into()]
            } else {
                vec![]
            },
            prompt: if kind == "prompt" {
                vec![pattern.into()]
            } else {
                vec![]
            },
        };
        rules.bash_global = pats;
        rules
    }

    #[test]
    fn screen_critical拒绝() {
        let rules = ApprovalRules::default();
        assert!(screen_command(&rules, "rm -rf /").is_err());
        assert!(
            screen_command(&rules, "echo x | rm -rf /").is_err(),
            "管道右侧 critical 也要拦"
        );
    }

    #[test]
    fn screen_deny与prompt拒绝_普通命令放行() {
        assert!(screen_command(&rules_with("deny", "cargo *"), "cargo build --release").is_err());
        // compound segments are screened one by one (approval.zh.md §2.5)
        assert!(
            screen_command(&rules_with("deny", "rm -rf *"), "cd /tmp && rm -rf build").is_err()
        );
        assert!(
            screen_command(&rules_with("prompt", "git push*"), "git push origin master").is_err()
        );
        assert!(screen_command(&ApprovalRules::default(), "echo hello && sleep 1").is_ok());
    }

    #[tokio::test]
    async fn hub_send_wait往返与receipt() {
        let h = host();
        let target = h.register_agent("scout");
        let ctx = ctx_for(&h);
        let out = HubTool
            .execute_ctx(
                &ctx,
                &json!({ "op": "send", "to": target, "message": "进度如何？" }),
            )
            .await;
        assert!(
            out.output.contains("injected"),
            "运行中目标回执 injected：{}",
            out.output
        );
        // target waits (from Main) and receives it
        let got = h
            .wait_inbox(&target, Some("Main"), Duration::from_secs(1))
            .await;
        assert!(matches!(got, crate::agent::WaitOutcome::Message(_)));
        // send-await round trip: target replies (simulate by pre-sending from target to Main)
        h.send(&target, "Main", "做完了");
        let out2 = HubTool
            .execute_ctx(
                &ctx,
                &json!({ "op": "send", "to": target, "message": "收到没", "await": true, "timeoutMs": 500 }),
            )
            .await;
        // await_reply waits for the *next* reply from target; none comes within the window
        // after the pre-sent one is consumed by the wait — timeout is a normal result either way.
        assert!(
            out2.output.contains("timeout") || out2.output.contains("text"),
            "一问一答应正常返回：{}",
            out2.output
        );
    }

    #[tokio::test]
    async fn hub_wait超时为正常结果与inbox不消费() {
        let h = host();
        h.register_agent("scout");
        let ctx = ctx_for(&h);
        let out = HubTool
            .execute_ctx(&ctx, &json!({ "op": "wait", "timeoutMs": 60 }))
            .await;
        assert!(
            out.output.contains("timeout"),
            "超时应为正常结果：{}",
            out.output
        );
        assert_eq!(out.exit_code, Some(0), "超时不是错误");
        // inbox peek does not consume
        h.send("ScoutX", "Main", "留一条");
        let out2 = HubTool.execute_ctx(&ctx, &json!({ "op": "inbox" })).await;
        assert!(out2.output.contains("留一条"));
        let out3 = HubTool.execute_ctx(&ctx, &json!({ "op": "inbox" })).await;
        assert!(
            out3.output.contains("留一条"),
            "peek 不消费：{}",
            out3.output
        );
    }

    #[tokio::test]
    async fn hub_list列roster含Main() {
        let h = host();
        let ctx = ctx_for(&h);
        let out = HubTool.execute_ctx(&ctx, &json!({ "op": "list" })).await;
        assert!(
            out.output.contains("Main"),
            "roster 应含 Main：{}",
            out.output
        );
    }

    #[tokio::test]
    async fn hub_processes_start经pattern筛查() {
        let h = host(); // default rules: no critical/pattern entries configured
        let ctx = ctx_for(&h);
        // deny-class: craft rules with a deny pattern via a config-level host
        let cfg = {
            let mut c = Config::default();
            c.rules.bash_global.deny = vec!["forbidden-server*".to_string()];
            c
        };
        let h2 = Arc::new(AgentHost::new(
            Arc::new(cfg),
            Arc::new(|_h: Option<&str>| AnyProvider::MockSubagent(MockSubagent::default())),
        ));
        let ctx2 = ToolCtx {
            config: &h2.config,
            agents: &h2,
            agent_id: "Main",
            def_name: None,
            depth: 0,
            is_subagent: false,
            cwd: None,
            decisions: None,
        };
        let out = HubTool
            .execute_ctx(
                &ctx2,
                &json!({ "op": "start", "name": "bad", "command": "forbidden-server --port 1" }),
            )
            .await;
        assert!(
            out.output.contains("deny"),
            "pattern deny 应拒绝启动：{}",
            out.output
        );
        assert_eq!(out.exit_code, Some(1));
        let _ = ctx; // keep the first host referenced
        let _ = h;
    }

    #[tokio::test]
    async fn hub_processes_start_ready_log后返回() {
        let h = host();
        let ctx = ctx_for(&h);
        let out = HubTool
            .execute_ctx(
                &ctx,
                &json!({
                    "op": "start",
                    "name": "demo-svc",
                    "command": "echo HUB-READY && sleep 5",
                    "ready": { "log": "HUB-READY", "timeout": 10 }
                }),
            )
            .await;
        assert!(
            out.output.contains("\"ready\": true"),
            "ready.log 双过后 start 才返回：{}",
            out.output
        );
        let ps = HubTool.execute_ctx(&ctx, &json!({ "op": "ps" })).await;
        assert!(ps.output.contains("demo-svc"));
        let stop = HubTool
            .execute_ctx(&ctx, &json!({ "op": "stop", "name": "demo-svc" }))
            .await;
        assert!(stop.output.contains("stopped"));
    }

    #[tokio::test]
    async fn hub_未知op与缺参数报错() {
        let h = host();
        let ctx = ctx_for(&h);
        let out = HubTool.execute_ctx(&ctx, &json!({ "op": "zzz" })).await;
        assert!(out.output.contains("未知 op"), "{}", out.output);
        let out2 = HubTool
            .execute_ctx(&ctx, &json!({ "op": "start", "name": "x" }))
            .await;
        assert!(out2.output.contains("command"), "{}", out2.output);
    }
}
