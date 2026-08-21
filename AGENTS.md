# DSCode — AI Collaboration Rules

This file contains repository-wide rules safe for a public checkout. If `AGENTS.local.md` exists, read it after this file for machine-specific commands and maintainer-only references. The local file may add constraints but must not weaken these rules, and must never be committed.

## Project Overview

- A leave-fork, full-stack Rust rewrite of DeepSeek Harness (dsh): a single-binary terminal coding agent.
- Stack: Rust + tokio + ratatui/crossterm + reqwest + serde/regex/ignore family.
- Layout: `crates/dscode/src/` contains the CLI, LLM adapters, shared chat loop, TUI/headless frontends, tools, sessions, approvals, goals, agents, hooks, and configuration.
- Runtime shape: TUI and headless mode share one turn loop. Session data lives under `~/.dscode/sessions/<YYYY/MM>/`; project directories only hold `.dscode/config.yaml`.

## Common Commands

```sh
cargo build --release
cargo test
dscode --headless --mock --prompt "run the tool demo"
dscode --headless --prompt "..."
dscode sessions
dscode resume <id>
dscode fork <id>
```

## Hard Constraints

- Implement only behavior covered by public requirements or maintainer-provided acceptance criteria. Do not infer unpublished product scope or disclose non-public material.
- Commit messages and code comments are English. User-facing TUI, prompt, headless, and error strings remain Chinese. Test names may use descriptive Chinese.
- The session log is the source of truth: transcript, model context, resume, fork, and compaction are replay projections; every model-visible input has a corresponding event.
- Approval fails closed: reviewer errors, missing UI, timeouts, and missing configuration deny. Critical patterns always escalate to a human. Deny is terminal.
- Decision-chain and pattern-table changes update their table-driven tests in the same change. Yolo skips prompts only; it never overrides denies.
- Do not add speculative features, compatibility aliases, or one-implementation abstractions.
- Do not add a crate unless the standard library and current dependency set cannot cover the requirement.
- Never put credentials in code, logs, fixtures, documentation, or commits. Credentials resolve only through `config::resolve_credential`.
- Never commit machine-specific paths, local toolchain configuration, private repository references, or unpublished specifications.

## Verification

- Run the smallest relevant test while developing and `cargo check` after each behavioral slice.
- Before completion, run `cargo fmt --check`, the full `cargo test`, and `cargo build --release`.
- Exercise user-visible changes through the real surface: TUI/terminal interaction, headless execution, or the produced release binary as appropriate.
- Approval, tool, and session semantic changes must keep their corresponding contract tests green.

## Commit Conventions

- Use English Conventional Commit messages, for example `feat(approval): add remembered decisions`.
- Keep one topic per commit; matching README or public usage documentation belongs in the same commit.
- Do not push, rewrite history, or run destructive Git commands unless the user explicitly requests it.
