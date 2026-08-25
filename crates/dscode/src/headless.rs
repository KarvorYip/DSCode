//! Headless mode: the same conversation logic as the TUI, output to stdout, no terminal UI (for automated acceptance).
//! Approval: human escalation cards have no UI available — UiSink defaults to Unavailable → fail-closed denial
//! (approval.zh.md §2.10: prompt-type requirements unsatisfiable under headless resolve to denial).

use crate::chat::{send_user_message, ChatCtx, UiSink};
use crate::i18n::{tr, trf, Lang, StrKey};
use crate::llm::{LlmProvider, Message, ToolCall};
use crate::session::SessionLog;
use std::io::BufRead;

/// Headless sink state: the suspension status line prints once per suspension.
/// `lang` comes from the same config as the TUI; registered slash commands are
/// rejected before they become a model turn because headless has no interactive surface.
pub struct Headless {
    suspend_announced: bool,
    lang: Lang,
}

impl UiSink for Headless {
    fn on_status(&mut self, _status: &str) {}

    fn on_suspend_tick(
        &mut self,
        info: &crate::limits::SuspendInfo,
    ) -> crate::limits::SuspendAction {
        if !self.suspend_announced {
            self.suspend_announced = true;
            let mode = if info.reset_at.is_some() {
                tr(self.lang, StrKey::HeadlessSuspendModeReset)
            } else {
                tr(self.lang, StrKey::HeadlessSuspendModeProbe)
            };
            println!(
                "{}",
                trf(
                    self.lang,
                    StrKey::HeadlessSuspendLine,
                    &[&info.reason as &dyn std::fmt::Display, &mode]
                )
            );
        }
        crate::limits::SuspendAction::Wait
    }

    fn on_suspend_end(&mut self) {
        self.suspend_announced = false;
    }

    fn on_auto_resumed(&mut self, note: &str) {
        println!("⚡ {note}");
    }

    fn on_user(&mut self, text: &str) {
        println!("{}", trf(self.lang, StrKey::UserLine, &[&text]));
    }

    fn on_delta(&mut self, text: &str) {
        print!("{text}");
    }

    fn on_assistant_done(&mut self, _content: &str, _tool_calls: &[ToolCall]) {
        println!();
    }

    fn on_tool_call(&mut self, call: &ToolCall) {
        println!(
            "{}",
            trf(
                self.lang,
                StrKey::HeadlessToolCall,
                &[&call.name, &call.arguments]
            )
        );
    }

    fn on_tool_result(&mut self, _id: &str, output: &str) {
        println!("{}", trf(self.lang, StrKey::ToolResultLine, &[&output]));
    }
}

/// Single-turn headless conversation: the prompt defaults to one line read from stdin.
/// `messages` is constructed by the caller (new sessions prepend system; resume passes the log rebuild result).
pub async fn run<P: LlmProvider>(
    provider: &mut P,
    log: &mut SessionLog,
    ctx: &mut ChatCtx<'_>,
    messages: &mut Vec<Message>,
    prompt: Option<String>,
) -> Result<(), String> {
    let prompt = match prompt {
        Some(p) => p,
        None => {
            eprintln!("{}", tr(ctx.lang, StrKey::HeadlessPromptHint));
            let mut line = String::new();
            std::io::stdin()
                .lock()
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            line.trim().to_string()
        }
    };
    if prompt.is_empty() {
        return Ok(());
    }
    if let Some(parsed) = crate::command::parse(&prompt) {
        let message = match parsed {
            crate::command::Parsed::Known(invocation) => {
                debug_assert!(!invocation
                    .command
                    .available_in(crate::command::Frontend::Headless));
                crate::command::unavailable(ctx.lang, invocation.command)
            }
            crate::command::Parsed::Unknown(name) => crate::command::unknown(ctx.lang, name),
        };
        println!("{message}");
        return Ok(());
    }
    let turn = messages
        .iter()
        .filter(|m| matches!(m, Message::User(_)))
        .count() as u64
        + 1;
    let mut sink = Headless {
        suspend_announced: false,
        lang: ctx.lang,
    };
    send_user_message(provider, messages, log, &mut sink, ctx, turn, &prompt).await
}
