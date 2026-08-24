# DSCode
[English](README.md) | [简体中文](README.zh-CN.md)


DSCode is a single-binary Rust terminal coding agent: event-sourced sessions, ask/auto/yolo approval, native and MCP tools, CDP browser automation, sub-agents, goal-driven continuation, and TUI/headless frontends.

Architecture in one sentence: on a tokio runtime, the `LlmProvider` trait drives the conversation turn; the turn loop dispatches through the tool registry (`Tool` trait + tier declarations), passes the approval gate (pure-function decision chain → auto-review / human card / deny, fail-closed), and appends every event to `~/.dscode/sessions/<YYYY/MM>/<id>.jsonl`; resume / fork / compaction / titles are all replay projections of the log.

## Install

```bash
npm install --global @karvorprime/dscode
# or
cargo binstall dscode
```

GitHub Releases also provides standalone archives for Windows, macOS, and Linux.

## Build

Requires Rust 1.75+. Windows users may build with either the MSVC or GNU target; CI covers both.

```bash
cargo build --release
# artifact: target/release/dscode.exe on Windows, target/release/dscode elsewhere
```

## Usage

```bash
./target/release/dscode.exe                        # interactive TUI (live DeepSeek)
./target/release/dscode.exe --mock                 # TUI + Mock (no key needed, multi-tool loop)
./target/release/dscode.exe --headless --mock --prompt "run the tool demo"
./target/release/dscode.exe --headless --prompt "run echo ok with bash and tell me the output"
./target/release/dscode.exe --approval-mode ask --headless --mock --prompt "write a demo file"
                                                   # headless: human escalation = fail-closed deny + audit pair
./target/release/dscode.exe sessions               # list sessions for the current directory (cwd-filtered index)
./target/release/dscode.exe resume <session-id>    # resume (crash recovery + context/transcript rebuild)
./target/release/dscode.exe fork <session-id>      # fork (completed-turn prefix copy, header.seedLength)
```

Credentials, four tiers for `DEEPSEEK_API_KEY`: env > `~/.dscode/.credentials.yaml` > project `.env` > `~/.dscode/.env`.

Config is two-layer YAML (`~/.dscode/config.yaml` global, `.dscode/config.yaml` project override): `approval.mode` (default auto), `modelRoles` (six roles; with `approver` unconfigured, auto falls to yolo with a one-time notice), `tools.approval.<tool>` (allow/deny/prompt), `bash.patterns` (allow/deny/prompt with compound-command segmentation), `compaction.autoThreshold` (default 0.8; null disables), `hooks` (event → block / rewrite / notify). Syntax and field errors abort startup with file:line.

## Keys (TUI)

| Key | Action |
|---|---|
| Enter | Send input |
| Shift+Tab | Cycle approval mode ask → auto → yolo (skips auto when no approver); logs `approval/policy` |
| Approval card y / s / a / n / d | Approve (once / session / always) / deny (once / session); the always tier writes a project config rule |
| `/language zh` / `/language en` | Switch UI display language live; writes `tui.language` back to the global config |
| Ctrl+C / Ctrl+D | Exit |
| ←/→/Home/End/Backspace/Delete/Paste | Input editing |

Config is two-layer YAML (`~/.dscode/config.yaml` global, `.dscode/config.yaml` project override): `approval.mode` (default auto), `modelRoles` (six roles; with `approver` unconfigured, auto falls to yolo with a one-time notice), `tools.approval.<tool>` (allow/deny/prompt), `bash.patterns` (allow/deny/prompt with compound-command segmentation), `compaction.autoThreshold` (default 0.8; null disables), `hooks` (event → block / rewrite / notify), `goal.enabled` (default true, mounts the goal stack in the TUI; headless never mounts), `goal.defaultMaxGoalRounds` (default 50, null = unlimited), `autoContinue.enabled` (default true, limit-recovery auto continue; the goal-rearm linkage rides this switch), `tui.language` (zh/en UI display language, default zh). Syntax and field errors abort startup with file:line.

Task tools keep state session-resident: mutations are recorded as `task/write` events (replayed on resume/fork), and the TUI renders a task panel (status icon + title, in_progress highlighted) from the same projection.

## Goal

`goal` is a cross-turn completion commitment: the model may call `create_goal` only inside a turn carrying an accepted human message (host proof — goal rounds and subagents never qualify), after which the goal-round-driver keeps driving `<goal_round>` continuation turns while the goal is active + armed and neither budget is exhausted. Every transition is CAS-guarded by `revision` (stale edits are rejected and the current state returned) and logged as one append-only `goal/change` event carrying the full snapshot; arming is process-local — after resume/fork the goal is visible and editable but drives no rounds until `/goal resume`.

| `/goal` form | Action |
|---|---|
| `/goal` or `/goal show` | Show the current goal snapshot |
| `/goal <objective>` | Create (user authority) |
| `/goal edit <objective>` | Rewrite the objective (current revision; overrides any in-flight model edit) |
| `/goal pause` | Pause + disarm (no rounds run, no budget burns) |
| `/goal resume` | The single explicit re-arm path |
| `/goal clear` | Terminate and drop the goal (`status: cleared` event) |

Dual budgets (both optional, independent): `maxGoalRounds` counts driven goal rounds (deployment default `goal.defaultMaxGoalRounds`); `token_budget` accumulates every model request's usage over the goal lifetime (create → terminal, pause windows excluded, compaction summary requests included). Remaining < 20% injects a soft warning into the continuation prompt; exhaustion hard-stops the driver while the goal stays active-but-stopped (remedy: `/goal resume` + edit the budget, or `/goal clear`). Three consecutive rounds without progress (no tool calls and no assistant text — implementation definition) force `blocked`. Approval is orthogonal: an active goal changes decision-chain output by exactly nothing (table-driven test proves byte-identical audits). Config: `goal.enabled` (default true, TUI only), `goal.defaultMaxGoalRounds` (default 50, null = unlimited). The status bar shows the goal badge (truncated objective + round N/M + budget awareness + blocked reason); create events get a one-time ★ highlight card.

## Limit Recovery

Provider usage errors never lose the task. The error body — never provider identity or the HTTP status alone — is classified three ways: a parseable `reset` field (relative seconds or an absolute timestamp) marks **quota-class**; a bare 429 without one is **rate-class**; everything else is unclassifiable and renders as an ordinary error (never suspends). Rate-class errors retry in place with exponential backoff (1s → 2s → … capped 60s); five consecutive failures escalate to quota-class handling. Quota-class errors suspend the turn **in-process** — no new turn, no fork, no rollback; recovery resends the same unfinished request, the session log stays transparent, and exiting mid-suspension is covered by the ordinary resume path.

Backoff: a known reset time probes at reset + 30s margin; without one a 1min → 5min → 15min → 30min ladder. Debounce: repeated errors from the same quota window never reset the ladder (no hot-looping a still-exhausted window). User cancel abandons the suspension permanently (records kept).

TUI: a suspend panel (reason + provider + countdown + `[r]` retry now / `[c]` cancel / `[p]` collapse) with the status bar mirroring the state — the panel closes without losing the signal. Headless `-p`: one stdout status line, auto-recovery as usual, cancel = process exit. Factory default `autoContinue.enabled: true` — auto probe + auto continue; each session's first auto-resume gets a one-time highlight. The auto path additionally re-arms a disarmed active goal (one `goal/rearm` audit per successful goal-carrying recovery, sharing the same highlight card); manual retry never re-arms.

## Sub-agents

`spawn` dispatches sub-agents. Definitions are discovered from project `.dscode/agents` > user `~/.dscode/agents` > bundled (scout = read-only research / task = general worker / advisor = observer whose output stays in `agent://` artifacts), first source winning on name. Children run forced-yolo: config rules and the user-decision snapshot stay effective, while critical patterns and prompt-class escalations resolve to fail-closed denial — `spawn` itself is the approval boundary for everything below it. The hidden `yield` tool is the only legal exit; a child that keeps ending in plain text gets three reminders and then a forced `toolChoice=yield` at the request layer. `agent://<id>` artifacts land in `~/.dscode/artifacts/<id>/` and `history://<id>` reads the in-memory transcript, both through `read`. Async results auto-inject into the conversation flow (`user/message`, source: agent); `agent/spawned` / `agent/completed` / `agent/message` are log-only events.

`isolated: true` runs the child in a git worktree under `<repo>/.dscode-worktrees/<agent-id>` and exports an applicable `changes.patch` (git add -A + diff HEAD) next to the artifact. `task.maxRecursionDepth` (default 2) strips `spawn` from children at the cap; direct self-recursion (same agent type) is intercepted.

Long-running processes (dev servers, watchers, REPLs) must go through `hub` processes: `ready.log` regex and an optional TCP port probe must both pass before `start` returns, and `processes.start` additionally screens its command against the bash pattern table (critical / deny / prompt all refuse to launch — no decision card exists on the hub surface, prompt-class is fail-closed).

## Tool Surface

| Tool | Tier | Notes |
|---|---|---|
| `read` | read | Numbered lines + `[file#tag]` anchor snapshot, offset/limit, directory listing, URL, structural summary over 2000 lines |
| `glob` | read | gitignore respected by default, hidden toggle, semicolon-separated multi-root |
| `grep` | read | regex → fancy-regex fallback, multi-root, skip paging, timeout |
| `write` | write | Whole-file overwrite, parent dirs auto-created |
| `edit` | write | Line-anchored patches (PUT/CUT, stale tag is a hard error, tight ranges) |
| `get_goal` | read-side | Current goal snapshot or none (compact JSON; never mounts in headless) |
| `create_goal` | write | Host proof only: a turn carrying an accepted human message; goal rounds and subagents denied; omitted max_goal_rounds falls to the deployment default |
| `update_goal` | write | Five actions (edit/pause/resume/complete/blocked), CAS by revision; complete/blocked legal only inside a goal round; one goal tool call per turn |
| `bash` | exec | 30s timeout, output truncation, UTF-8 enforced |
| `TaskCreate` | write | Create a task (optional addBlocks/addBlockedBy dependency edges); returns a taskId handle |
| `TaskUpdate` | write | Incremental update by taskId: status flow pending → in_progress → completed/deleted + edge add/remove (terminal states reject updates) |
| `TaskGet` | read | One task by id (deleted tasks visible, marked deleted) |
| `TaskList` | read | All non-deleted tasks |
| `spawn` | exec | Sub-agent dispatch: batch `{context, tasks[]}` or flat `{task}`; per-item `agent` / `outputSchema`+`schemaMode` / `isolated`; async jobs auto-deliver on completion, or sync under the `task.maxConcurrency` semaphore |
| `hub` | write | Coordination surface: messaging (send / send+await / broadcast / wait / inbox-peek / list; mailbox cap 100), jobs (four-way wait — timeout is a normal result / cancel / snapshot), processes (start gated on ready.log regex + optional TCP port, ps / logs / stop / restart / describe / stdin send / wait) |
| `yield` | hidden | The sub-agent's only legal exit (`{"result": ...}`); intercepted by the runner, top-level calls rejected |

The six-step decision chain (pure function, table-driven): tool deny > user deny > yolo special case > per-tool override > bash patterns (critical patterns always escalate to a human) > mode default. Every triggered gate decision emits a paired `approval/asked` + `approval/decided` audit event (log-only; the model only sees tool results).

## Acceptance

```bash
cargo test            # unit tests per domain (session / approval / tools / goal / agent+hub / limits)
cargo test -- agent spawn hub    # 48 sub-agent & hub tests (depth stripping, self-recursion, semaphore, schema, send/wait, worktree patch)
./target/release/dscode.exe --headless --mock --prompt "run the tool demo"
./target/release/dscode.exe --approval-mode ask --headless --mock --prompt "write a demo file"   # audit pairs + fail-closed
./target/release/dscode.exe sessions && ./target/release/dscode.exe resume <id> && ./target/release/dscode.exe fork <id>
```

Expected for mock: a write → read (with `[file#tag]` anchor) → bash three-tool loop plus the yolo notice (no approver configured). Expected for ask: Write/Exec tiers denied, Read tier allowed, `approval/*` audit pairs in the log.


## Layout

```
crates/dscode/
  src/main.rs           # CLI parsing and assembly (sessions/resume/fork, approval mode, provider/approver)
  src/llm.rs            # LlmProvider (chat_stream + complete) + DeepSeek(SSE) + Mock (multi-tool script)
  src/chat.rs           # Turn loop: registry dispatch, approval gate, hooks, compaction, titles
  src/tui.rs            # ratatui inline viewport + decision card + mode display + Shift+Tab + resume transcript
  src/headless.rs       # headless stdout frontend (approval fails closed)
  src/tool/             # Tool trait + Registry + ToolCtx + bash/read/write/edit/glob/grep/spawn/hub(+yield)
  src/agent/            # Sub-agents: definitions & discovery, lifecycle host (mailbox/jobs), runner (yield loop), worktree isolation, hub processes
  src/session/          # JSONL event log: envelope, crash recovery, fork, projections, index
  src/approval/         # Decision chain, pattern tables, ApprovalProvider (AutoReviewer/HeadlessReject), audit
  src/config.rs         # Two-layer YAML + four-tier credentials + always-rule write-back
  src/hooks.rs          # Declarative events → block/rewrite/notify
  src/limits.rs         # Limit recovery: error classification, backoff ladder, suspension runtime
```

## License

MIT. See [LICENSE](LICENSE).
