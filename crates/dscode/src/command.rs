//! Shared slash-command registry for the TUI and headless frontends.

use crate::i18n::Lang;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frontend {
    Tui,
    Headless,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Wizard,
    Settings,
    Tui,
    Export,
    Sessions,
    Agents,
    Hotkeys,
    ApprovalMode,
    Goal,
    Language,
    Compact,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invocation<'a> {
    pub command: Command,
    pub args: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parsed<'a> {
    Known(Invocation<'a>),
    Unknown(&'a str),
}

struct Spec {
    command: Command,
    name: &'static str,
    usage_zh: &'static str,
    usage_en: &'static str,
    description_zh: &'static str,
    description_en: &'static str,
}

const SPECS: &[Spec] = &[
    Spec {
        command: Command::Help,
        name: "help",
        usage_zh: "/help",
        usage_en: "/help",
        description_zh: "显示可用斜杠命令",
        description_en: "Show available slash commands",
    },
    Spec {
        command: Command::Wizard,
        name: "wizard",
        usage_zh: "/wizard",
        usage_en: "/wizard",
        description_zh: "重新运行首启引导",
        description_en: "Rerun the setup wizard",
    },
    Spec {
        command: Command::Settings,
        name: "settings",
        usage_zh: "/settings",
        usage_en: "/settings",
        description_zh: "显示当前生效配置",
        description_en: "Show effective configuration",
    },
    Spec {
        command: Command::Tui,
        name: "tui",
        usage_zh: "/tui fullscreen|default",
        usage_en: "/tui fullscreen|default",
        description_zh: "切换 TUI 渲染模式",
        description_en: "Switch the TUI render mode",
    },
    Spec {
        command: Command::Export,
        name: "export",
        usage_zh: "/export [目录]",
        usage_en: "/export [directory]",
        description_zh: "导出当前会话的 Markdown 和 JSONL",
        description_en: "Export the current session as Markdown and JSONL",
    },
    Spec {
        command: Command::Sessions,
        name: "sessions",
        usage_zh: "/sessions [序号或 id]",
        usage_en: "/sessions [index or id]",
        description_zh: "列出或恢复当前目录的会话",
        description_en: "List or resume a session for the current directory",
    },
    Spec {
        command: Command::Agents,
        name: "agents",
        usage_zh: "/agents",
        usage_en: "/agents",
        description_zh: "切换 Agent Hub 面板",
        description_en: "Toggle the Agent Hub panel",
    },
    Spec {
        command: Command::Hotkeys,
        name: "hotkeys",
        usage_zh: "/hotkeys",
        usage_en: "/hotkeys",
        description_zh: "显示快捷键",
        description_en: "Show keyboard shortcuts",
    },
    Spec {
        command: Command::ApprovalMode,
        name: "approval-mode",
        usage_zh: "/approval-mode ask|auto|yolo",
        usage_en: "/approval-mode ask|auto|yolo",
        description_zh: "切换会话审批模式",
        description_en: "Switch the session approval mode",
    },
    Spec {
        command: Command::Goal,
        name: "goal",
        usage_zh: "/goal [show|目标|edit|pause|resume|clear]",
        usage_en: "/goal [show|objective|edit|pause|resume|clear]",
        description_zh: "查看或管理 goal",
        description_en: "Show or manage a goal",
    },
    Spec {
        command: Command::Language,
        name: "language",
        usage_zh: "/language [zh|en]",
        usage_en: "/language [zh|en]",
        description_zh: "切换界面显示语言",
        description_en: "Switch the display language",
    },
    Spec {
        command: Command::Compact,
        name: "compact",
        usage_zh: "/compact",
        usage_en: "/compact",
        description_zh: "压缩当前会话上下文",
        description_en: "Compact the current session context",
    },
];

impl Command {
    pub fn available_in(self, frontend: Frontend) -> bool {
        matches!(frontend, Frontend::Tui)
    }

    pub fn accepts_args(self) -> bool {
        matches!(
            self,
            Self::Tui
                | Self::Export
                | Self::Sessions
                | Self::ApprovalMode
                | Self::Goal
                | Self::Language
        )
    }

    pub fn usage(self, lang: Lang) -> &'static str {
        let spec = spec(self);
        match lang {
            Lang::Zh => spec.usage_zh,
            Lang::En => spec.usage_en,
        }
    }

    pub fn name(self) -> &'static str {
        spec(self).name
    }

    fn from_name(name: &str) -> Option<Self> {
        SPECS
            .iter()
            .find(|spec| spec.name == name)
            .map(|spec| spec.command)
    }
}

pub fn parse(input: &str) -> Option<Parsed<'_>> {
    let input = input.trim();
    let rest = input.strip_prefix('/')?;
    let split = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..split];
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
    {
        return None;
    }
    let args = rest[split..].trim();

    Some(match Command::from_name(name) {
        Some(command) => Parsed::Known(Invocation { command, args }),
        None => Parsed::Unknown(name),
    })
}

pub fn help(lang: Lang) -> String {
    let title = match lang {
        Lang::Zh => "可用斜杠命令：",
        Lang::En => "Available slash commands:",
    };
    let lines = SPECS
        .iter()
        .map(|spec| {
            let (usage, description) = match lang {
                Lang::Zh => (spec.usage_zh, spec.description_zh),
                Lang::En => (spec.usage_en, spec.description_en),
            };
            format!("  {usage} — {description}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{title}\n{lines}")
}

pub fn unknown(lang: Lang, name: &str) -> String {
    match lang {
        Lang::Zh => format!("未知命令：/{name}；输入 /help 查看可用命令。"),
        Lang::En => format!("Unknown command: /{name}. Enter /help for available commands."),
    }
}
pub fn usage(lang: Lang, command: Command) -> String {
    match lang {
        Lang::Zh => format!("用法：{}", command.usage(lang)),
        Lang::En => format!("Usage: {}", command.usage(lang)),
    }
}

pub fn unavailable(lang: Lang, command: Command) -> String {
    match lang {
        Lang::Zh => format!("命令 {} 仅支持交互式 TUI。", command.usage(lang)),
        Lang::En => format!(
            "Command {} is only available in the interactive TUI.",
            command.usage(lang)
        ),
    }
}

fn spec(command: Command) -> &'static Spec {
    SPECS
        .iter()
        .find(|spec| spec.command == command)
        .expect("every command has a specification")
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, Frontend, Parsed};

    #[test]
    fn 已注册命令保留参数边界() {
        let Parsed::Known(invocation) = parse("/tui fullscreen").expect("命令应被识别")
        else {
            panic!("/tui 应是已注册命令");
        };

        assert_eq!(invocation.command, Command::Tui);
        assert_eq!(invocation.args, "fullscreen");
    }

    #[test]
    fn 未知命令不进入对话回合() {
        assert_eq!(parse("/foo"), Some(Parsed::Unknown("foo")));
    }

    #[test]
    fn 路径和普通文本不被误认为命令() {
        assert_eq!(parse("/tmp/file"), None);
        assert_eq!(parse("解释 /help"), None);
    }

    #[test]
    fn 命令能力按前端声明() {
        assert!(Command::Settings.available_in(Frontend::Tui));
        assert!(!Command::Settings.available_in(Frontend::Headless));
    }
}
