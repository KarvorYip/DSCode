# DSCode

[English](README.md) | [简体中文](README.zh-CN.md)

DSCode 是单二进制 Rust 终端编码 Agent：事件溯源会话、ask/auto/yolo 审批、原生与 MCP 工具、CDP browser、子代理、goal 续跑，以及 TUI/headless 双前端。

一句话架构：tokio 运行时上，`LlmProvider` trait 驱动对话回合（turn）；回合循环经工具注册表（`Tool` trait + tier 声明）派发，过审批门（纯函数 decision chain → auto 代审 / 人工卡片 / 拒绝，fail-closed 失败即关），并把每个事件追加到 `~/.dscode/sessions/<YYYY/MM>/<id>.jsonl`；resume / fork / compaction / 标题全部是对日志的 replay 投影。

## 安装

```bash
npm install --global @karvorprime/dscode
# 或
cargo binstall dscode
```

GitHub Releases 同时提供 Windows、macOS、Linux 的独立压缩包。

## 构建

要求 Rust 1.75+。Windows 可使用 MSVC 或 GNU target；CI 同时覆盖两者。

```bash
cargo build --release
# Windows 产物：target/release/dscode.exe；其他平台：target/release/dscode
```

## 用法

```bash
./target/release/dscode.exe                        # 交互式 TUI（真实 DeepSeek）
./target/release/dscode.exe --mock                 # TUI + Mock（免密钥，多工具回路）
./target/release/dscode.exe --headless --mock --prompt "run the tool demo"
./target/release/dscode.exe --headless --prompt "run echo ok with bash and tell me the output"
./target/release/dscode.exe --approval-mode ask --headless --mock --prompt "write a demo file"
                                                   # headless: 人工升级 = fail-closed 拒绝 + 审计对
./target/release/dscode.exe sessions               # 列出当前目录的会话（按 cwd 过滤的索引）
./target/release/dscode.exe resume <session-id>    # 恢复（crash 恢复 + 上下文/转录重建）
./target/release/dscode.exe fork <session-id>      # 分叉（已完成回合前缀复制，header.seedLength）
```

`DEEPSEEK_API_KEY` 凭据四层解析：env > `~/.dscode/.credentials.yaml` > 项目 `.env` > `~/.dscode/.env`。

配置为双层 YAML（`~/.dscode/config.yaml` 全局，`.dscode/config.yaml` 项目覆盖）：`approval.mode`（默认 auto）、`modelRoles`（六角色；`approver` 未配置时 auto 落到 yolo 并给一次性提示）、`tools.approval.<tool>`（allow/deny/prompt）、`bash.patterns`（allow/deny/prompt，复合命令切分）、`compaction.autoThreshold`（默认 0.8；null 关闭）、`hooks`（事件 → block / rewrite / notify）、`autoContinue.enabled`（默认开；限额恢复自动续跑，goal rearm 联动随此开关）、`tui.language`（zh/en 界面显示语言，默认 zh）。语法与字段错误以 file:line 中止启动。

## 按键（TUI）

| 按键 | 动作 |
|---|---|
| Enter | 发送输入 |
| Shift+Tab | 循环切换审批模式 ask → auto → yolo（无 approver 时跳过 auto）；写 `approval/policy` 日志 |
| 审批卡 y / s / a / n / d | 批准（once / session / always）/ 拒绝（once / session）；always 档写一条项目配置规则 |
| `/language zh` / `/language en` | 会话内切换界面显示语言；写回全局配置 `tui.language` |
| Ctrl+C / Ctrl+D | 退出 |
| ←/→/Home/End/Backspace/Delete/Paste | 输入编辑 |

状态行始终显示当前审批模式；yolo 以红色高亮作为常显警示。

任务工具的状态存于会话内：变更记为 `task/write` 事件（resume/fork 时回放重建），TUI 从同一投影渲染任务面板（状态图标 + 标题；in_progress 高亮）。

## 工具面

| 工具 | Tier | 说明 |
|---|---|---|
| `read` | read | 带行号 + `[file#tag]` 锚点快照，offset/limit，目录列举，URL，超 2000 行给结构摘要 |
| `glob` | read | 默认尊重 gitignore，hidden 开关，分号分隔多根 |
| `grep` | read | regex → fancy-regex 回退，多根，skip 分页，超时 |
| `write` | write | 整文件覆盖，父目录自动创建 |
| `edit` | write | 行锚定补丁（PUT/CUT，过期 tag 硬错误，紧范围） |
| `bash` | exec | 30 秒超时，输出截断，强制 UTF-8 |
| `TaskCreate` | write | 创建任务（可选 addBlocks/addBlockedBy 依赖边）；返回 taskId 句柄 |
| `TaskUpdate` | write | 按 taskId 增量更新：状态流转 pending → in_progress → completed/deleted + 依赖边增删（终态拒绝更新） |
| `TaskGet` | read | 按 id 读单任务（已删除任务可查，标记 deleted） |
| `TaskList` | read | 列出全部未删除任务 |

六步 decision chain（纯函数、表驱动）：tool deny > user deny > yolo 特例 > 按工具覆盖 > bash patterns（critical pattern 永远升级到人工）> 模式默认。每个触发门决策产生成对 `approval/asked` + `approval/decided` 审计事件（仅记日志；模型只看到工具结果）。

## 限额恢复

provider 用量错误不丢任务：按**解析错误体**分类——带可解析 `reset` 字段（相对秒数或绝对时间戳）的是**限额类**（挂起；有 reset time 则到点 reset+30s 自动探测，无则按 1min→5min→15min→30min 阶梯探测）；无 reset 字段的 429 是**速率类**（原位指数退避 1s→2s→…→60s，连续 5 次失败升级为限额类处理）；无法归类的错误按普通失败呈现，绝不挂起。挂起是进程本地暂停态——不开新 turn、不 fork、不回滚，恢复即重发同一未完成请求；挂起对 session log 透明，进程中途退出走普通 resume 路径。防抖：同一配额窗口内重复限额错误不重置阶梯；用户取消则永久放弃本次挂起（会话与记录保留）。

TUI 挂起面板展示原因 + provider + 倒计时与快捷键 `[r]` 立即重试 / `[c]` 取消挂起 / `[p]` 收起面板，状态栏同步挂起态——面板关闭不丢信号。headless `-p` 无面板：stdout 一行状态 + 自动恢复照常，取消即进程退出。配置键 `autoContinue.enabled`（默认开，双层合并）：开——到点自动探测恢复，每会话首次自动恢复显示一次性高亮，并顺带 rearm disarmed 活跃 goal（每次成功写一条 `goal/rearm` 审计事件，与恢复共享同一张高亮卡；手动「立即重试」不 rearm）；关——恢复只响应手动「立即重试」，无任何自动探测。

## 验收

```bash
cargo test            # 195 单测: session, approval/config/hooks, tools, goal, agent+hub, limits
./target/release/dscode.exe --headless --mock --prompt "run the tool demo"
./target/release/dscode.exe --approval-mode ask --headless --mock --prompt "write a demo file"   # 审计对 + fail-closed
./target/release/dscode.exe sessions && ./target/release/dscode.exe resume <id> && ./target/release/dscode.exe fork <id>
```

mock 预期：write → read（带 `[file#tag]` 锚点）→ bash 三工具回路，外加 yolo 提示（未配置 approver）。ask 预期：Write/Exec 档拒绝、Read 档放行，日志中出现 `approval/*` 审计对。


## 布局

```
crates/dscode/
  src/main.rs           # CLI 解析与组装（sessions/resume/fork、审批模式、provider/approver）
  src/llm.rs            # LlmProvider (chat_stream + complete) + DeepSeek(SSE) + Mock（多工具脚本）
  src/chat.rs           # 回合循环: 注册表派发、审批门、hooks、compaction、标题
  src/tui.rs            # ratatui inline viewport + 决策卡 + 模式显示 + Shift+Tab + resume 转录
  src/headless.rs       # headless stdout 前端（审批 fail-closed）
  src/tool/             # Tool trait + Registry + bash/read/write/edit/glob/grep
  src/session/          # JSONL 事件日志: envelope、crash 恢复、fork、投影、索引
  src/approval/         # decision chain、pattern 表、ApprovalProvider (AutoReviewer/HeadlessReject)、审计
  src/config.rs         # 双层 YAML + 四层凭据 + always 规则写回
  src/hooks.rs          # 声明式事件 → block/rewrite/notify
  src/limits.rs         # 限额恢复：错误分类、退避阶梯、挂起运行时
```

## 许可证

MIT。详见 [LICENSE](LICENSE)。
