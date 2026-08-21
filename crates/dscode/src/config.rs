//! Two-layer `.dscode` YAML config (config-onboarding.zh.md): two-layer loading
//! (~/.dscode/config.yaml + <cwd>/.dscode/config.yaml), leaf-level merge (project overrides
//! global, lists replace wholesale), fail loud (YAML syntax errors / field type errors
//! reported at startup with file:line), four-layer credential resolution.
//! Phase 1 delivers the minimal read surface for approval-related keys; the wizard and
//! write-back engineering land in phase 3.

use crate::approval::{self, ApprovalRules, BashPatterns, EffectiveMode, Mode, ToolApproval};
use crate::hooks::{self, HooksConfig};
use crate::i18n::Lang;
use crate::llm::WireFormat;
use dirs::home_dir;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// Full set of modelRoles roles (ticket 004 / architecture.zh.md): six fixed roles.
pub const MODEL_ROLES: [&str; 6] = ["default", "vision", "plan", "commit", "advisor", "approver"];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RenderMode {
    Inline,
    Fullscreen,
}

impl Default for RenderMode {
    fn default() -> Self {
        Self::Inline
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api: WireFormat,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    #[serde(default, rename = "apiKey")]
    pub api_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModel {
    pub provider: String,
    pub config: ProviderConfig,
    pub model: String,
}

/// Final config after two-layer merge + default filling.
#[derive(Clone, Debug)]
pub struct Config {
    /// approval.mode; factory default auto.
    pub approval_mode: Mode,
    /// modelRoles: role → concrete model mapping.
    pub model_roles: BTreeMap<String, String>,
    /// providers catalog; DeepSeek is always present as the zero-config default.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Approval rules (per-tool overrides + bash patterns; both layers kept for decision-chain evaluation).
    pub rules: ApprovalRules,
    /// sessions.dir; defaults to ~/.dscode/sessions.
    pub sessions_dir: PathBuf,
    /// compaction.autoThreshold; defaults to 0.8; explicit null (None) disables the automatic track.
    pub compaction_auto_threshold: Option<f64>,
    /// hooks config (declarative triggers).
    pub hooks: HooksConfig,
    /// task.* keys: sub-agent dispatch knobs (tools.zh.md §9).
    pub task: TaskConfig,
    /// browser.endpoint: explicit Chromium CDP HTTP endpoint; None probes localhost:9222.
    pub browser_endpoint: Option<String>,
    /// goal.enabled (goal.zh.md §配置): mounts or removes the whole goal stack. Factory
    /// default true for the interactive TUI; headless -p never mounts goal tools regardless.
    pub goal_enabled: bool,
    /// goal.defaultMaxGoalRounds: deployment default for create_goal omitting max_goal_rounds.
    /// None = unlimited. The dsh snapshot does not pin a concrete number; 50 is this
    /// implementation's chosen default (large enough not to disturb real long tasks).
    pub goal_default_max_rounds: Option<u64>,
    /// autoContinue.enabled (limits.zh.md §配置): auto probe + auto continue after a
    /// limit-class suspension; factory default true (TUI + headless alike).
    pub auto_continue_enabled: bool,
    /// tui.language (config-onboarding.zh.md §TUI 显示语言): display language for
    /// user-visible strings (TUI surfaces, headless stdout, startup errors); zh default.
    pub language: Lang,
    /// tui.renderMode; inline by default.
    pub render_mode: RenderMode,
}

/// task.* keys (tools.zh.md §9): sync-spawn semaphore cap, spawn recursion cap, isolation backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskConfig {
    /// task.maxConcurrency: sync spawn dispatch semaphore cap; None = unbounded.
    pub max_concurrency: Option<usize>,
    /// task.maxRecursionDepth (default 2): at the cap the child's `spawn` tool is stripped.
    pub max_recursion_depth: u8,
    /// task.isolation.mode (default git worktree).
    pub isolation_mode: IsolationMode,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            max_concurrency: None,
            max_recursion_depth: 2,
            isolation_mode: IsolationMode::Git,
        }
    }
}

/// task.isolation.mode: merge-patch-first backend choice; overlayfs/ProjFS/reflink are deferred (tools.zh.md §6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IsolationMode {
    #[default]
    Git,
}

impl Default for Config {
    fn default() -> Self {
        let sessions_dir = home_dir()
            .map(|h| h.join(".dscode").join("sessions"))
            .unwrap_or_else(|| PathBuf::from(".dscode/sessions"));
        Self {
            approval_mode: Mode::Auto,
            model_roles: BTreeMap::new(),
            providers: BTreeMap::from([(
                "deepseek".into(),
                ProviderConfig {
                    api: WireFormat::OpenAiCompletions,
                    base_url: "https://api.deepseek.com".into(),
                    api_key: Some("env:DEEPSEEK_API_KEY".into()),
                },
            )]),
            rules: ApprovalRules::default(),
            sessions_dir,
            compaction_auto_threshold: Some(0.8),
            hooks: HooksConfig::default(),
            task: TaskConfig::default(),
            browser_endpoint: None,
            goal_enabled: true,
            goal_default_max_rounds: Some(50),
            auto_continue_enabled: true,
            language: Lang::default(),
            render_mode: RenderMode::Inline,
        }
    }
}

impl Config {
    /// Load the two-layer config: `~/.dscode/config.yaml` (global) + `<cwd>/.dscode/config.yaml` (project).
    /// A missing file in either layer counts as an empty layer (a fresh machine with zero config goes
    /// straight to the wizard); if present it is parsed — syntax errors / field type errors fail loud and exit.
    pub fn load() -> Result<Self, String> {
        let home = home_dir().ok_or("无法定位用户主目录")?;
        Self::load_dirs(&home.join(".dscode"), Path::new("."))
    }

    /// Load by directories (testable entry point).
    pub fn load_dirs(global_dscode: &Path, project_root: &Path) -> Result<Self, String> {
        let global = parse_layer(&global_dscode.join("config.yaml"))?;
        let project = parse_layer(&project_root.join(".dscode").join("config.yaml"))?;
        let user_dscode = global_dscode.to_path_buf();
        Self::resolve(global, project, &user_dscode)
    }

    /// Construct directly from YAML text (test entry: global text + project text).
    pub fn from_str_layers(global_yaml: &str, project_yaml: &str) -> Result<Self, String> {
        let global = parse_layer_text(global_yaml, Path::new("<global>"))?;
        let project = parse_layer_text(project_yaml, Path::new("<project>"))?;
        let user_dscode = home_dir()
            .map(|h| h.join(".dscode"))
            .unwrap_or_else(|| PathBuf::from(".dscode"));
        Self::resolve(global, project, &user_dscode)
    }

    /// Two-layer merge + validation + default filling. Leaf-level merge: project values override
    /// global values key by key; lists replace wholesale; approval deny-class semantics (union)
    /// are handled uniformly by the decision chain at evaluation time.
    fn resolve(
        global: ConfigLayer,
        project: ConfigLayer,
        user_dscode: &Path,
    ) -> Result<Self, String> {
        let mut config = Config::default();

        // approval.mode: project overrides global; invalid values already fail loud at deserialization.
        config.approval_mode = project
            .approval
            .and_then(|a| a.mode)
            .or_else(|| global.approval.and_then(|a| a.mode))
            .unwrap_or(config.approval_mode);

        // modelRoles: key-by-key override; the role set is fixed at six; unknown roles fail loud.
        if let Some(roles) = global.model_roles {
            config.model_roles.extend(roles);
        }
        if let Some(roles) = project.model_roles {
            config.model_roles.extend(roles);
        }
        for role in config.model_roles.keys() {
            if !MODEL_ROLES.contains(&role.as_str()) {
                return Err(format!(
                    "未知 modelRoles 角色「{role}」（合法值：{})",
                    MODEL_ROLES.join("/")
                ));
            }
        }

        let mut merge_providers = |layers: Option<BTreeMap<String, ProviderLayer>>| {
            if let Some(layers) = layers {
                for (id, layer) in layers {
                    let provider = config
                        .providers
                        .entry(id)
                        .or_insert_with(|| ProviderConfig {
                            api: WireFormat::OpenAiCompletions,
                            base_url: String::new(),
                            api_key: None,
                        });
                    if let Some(api) = layer.api {
                        provider.api = api;
                    }
                    if let Some(base_url) = layer.base_url {
                        provider.base_url = base_url;
                    }
                    if let Some(api_key) = layer.api_key {
                        provider.api_key = Some(api_key);
                    }
                }
            }
        };
        merge_providers(global.providers);
        merge_providers(project.providers);
        for (id, provider) in &config.providers {
            if id.trim().is_empty() {
                return Err("providers id 不得为空".into());
            }
            if !provider.base_url.starts_with("http://")
                && !provider.base_url.starts_with("https://")
            {
                return Err(format!("providers.{id}.baseUrl 必须是 http(s) URL"));
            }
            if provider
                .api_key
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(format!("providers.{id}.apiKey 引用不得为空"));
            }
        }

        // tools.approval.<tool>: both layers kept (deny union evaluated in the decision chain).
        if let Some(t) = global.tools.and_then(|t| t.approval) {
            config.rules.tools_global = t;
        }
        if let Some(t) = project.tools.and_then(|t| t.approval) {
            config.rules.tools_project = t;
        }

        // bash.patterns: both layers kept.
        if let Some(p) = global.bash.and_then(|b| b.patterns) {
            config.rules.bash_global = p.into();
        }
        if let Some(p) = project.bash.and_then(|b| b.patterns) {
            config.rules.bash_project = p.into();
        }
        // Pattern validity is reported at startup: an invalid glob must not fail silently.
        for (layer, pats) in [
            ("global", &config.rules.bash_global),
            ("project", &config.rules.bash_project),
        ] {
            for kind in ["allow", "deny", "prompt"] {
                let list = match kind {
                    "allow" => &pats.allow,
                    "deny" => &pats.deny,
                    _ => &pats.prompt,
                };
                for p in list {
                    if globset::Glob::new(p).is_err() {
                        return Err(format!(
                            "bash.patterns（{layer} 层 {kind}）非法 glob：「{p}」"
                        ));
                    }
                }
            }
        }

        // sessions.dir: project overrides global; defaults to ~/.dscode/sessions.
        config.sessions_dir = project
            .sessions
            .and_then(|s| s.dir)
            .or_else(|| global.sessions.and_then(|s| s.dir))
            .map(PathBuf::from)
            .unwrap_or_else(|| user_dscode.join("sessions"));

        // compaction.autoThreshold: project overrides global; defaults to 0.8; value domain (0,1].
        // Explicit null disables the automatic track (Some(None)); unset falls back to the next layer; both layers missing defaults to 0.8.
        let th = project
            .compaction
            .and_then(|c| c.auto_threshold)
            .or_else(|| global.compaction.and_then(|c| c.auto_threshold));
        config.compaction_auto_threshold = match th {
            Some(v) => v,
            None => Some(0.8),
        };
        if let Some(t) = config.compaction_auto_threshold {
            if !(0.0..=1.0).contains(&t) || t == 0.0 {
                return Err(format!(
                    "compaction.autoThreshold 必须在 (0,1] 内，实际 {t}"
                ));
            }
        }

        // hooks: leaf-level merge by event key (project overrides global).
        if let Some(h) = global.hooks {
            config.hooks = hooks::merge_hooks(&config.hooks, &h);
        }
        if let Some(h) = project.hooks {
            config.hooks = hooks::merge_hooks(&config.hooks, &h);
        }

        // task.*: leaf-level merge (project overrides global); recursion depth domain >= 1; isolation mode git only.
        let merge_task = |layer: Option<TaskLayer>, cfg: &mut TaskConfig| {
            if let Some(t) = layer {
                if let Some(v) = t.max_concurrency {
                    cfg.max_concurrency = Some(v);
                }
                if let Some(v) = t.max_recursion_depth {
                    cfg.max_recursion_depth = v;
                }
                if let Some(mode) = t.isolation.and_then(|i| i.mode) {
                    match mode.as_str() {
                        "git" => cfg.isolation_mode = IsolationMode::Git,
                        other => {
                            return Err(format!(
                                "task.isolation.mode「{other}」未知（首发仅 git worktree）"
                            ))
                        }
                    }
                }
            }
            Ok(())
        };
        merge_task(global.task, &mut config.task)?;
        merge_task(project.task, &mut config.task)?;
        if config.task.max_recursion_depth < 1 {
            return Err(format!(
                "task.maxRecursionDepth 必须 >= 1，实际 {}",
                config.task.max_recursion_depth
            ));
        }

        // goal.*: leaf-level merge (project overrides global); enabled defaults true (TUI;
        // headless mounting is decided at assembly, not here); defaultMaxGoalRounds defaults
        // to 50, explicit null = unlimited; positive integers only.
        let merge_goal = |layer: Option<GoalLayer>, cfg: &mut Config| {
            if let Some(g) = layer {
                if let Some(v) = g.enabled {
                    cfg.goal_enabled = v;
                }
                if let Some(v) = g.default_max_goal_rounds {
                    cfg.goal_default_max_rounds = v;
                }
            }
        };
        merge_goal(global.goal, &mut config);
        merge_goal(project.goal, &mut config);
        if config.goal_default_max_rounds.is_some_and(|m| m == 0) {
            return Err("goal.defaultMaxGoalRounds 必须为正整数（null 表示不限）".to_string());
        }

        // autoContinue.enabled (limits.zh.md §配置): project overrides global; default true.
        config.auto_continue_enabled = project
            .auto_continue
            .and_then(|a| a.enabled)
            .or_else(|| global.auto_continue.and_then(|a| a.enabled))
            .unwrap_or(true);

        // browser.endpoint: project overrides global; absent probes localhost:9222 in the tool.
        config.browser_endpoint = project
            .browser
            .and_then(|b| b.endpoint)
            .or_else(|| global.browser.and_then(|b| b.endpoint));
        if config.browser_endpoint.as_ref().is_some_and(|endpoint| {
            !endpoint.starts_with("http://") && !endpoint.starts_with("https://")
        }) {
            return Err("browser.endpoint 必须是 http:// 或 https:// CDP 地址".to_string());
        }

        // tui.language (config-onboarding.zh.md §TUI 显示语言): project overrides global;
        // factory default zh. Invalid values already fail loud at deserialization (file:line).
        config.language = project
            .tui
            .as_ref()
            .and_then(|t| t.language)
            .or_else(|| global.tui.as_ref().and_then(|t| t.language))
            .unwrap_or(config.language);
        config.render_mode = project
            .tui
            .as_ref()
            .and_then(|t| t.render_mode.clone())
            .or_else(|| global.tui.as_ref().and_then(|t| t.render_mode.clone()))
            .unwrap_or_default();

        Ok(config)
    }

    /// Whether modelRoles.approver is configured (the precondition for auto to take effect).
    pub fn approver_configured(&self) -> bool {
        self.model_roles
            .get("approver")
            .is_some_and(|m| !m.trim().is_empty())
    }

    /// Effective mode (§2.1): without an approver configured, auto effectively lands on yolo + a one-time onboarding flag.
    pub fn effective_mode(&self) -> EffectiveMode {
        approval::effective_mode(self.approval_mode, self.approver_configured())
    }

    pub fn resolve_model(&self, role: &str, fallback: &str) -> Result<ResolvedModel, String> {
        let configured = self
            .model_roles
            .get(role)
            .map(String::as_str)
            .unwrap_or(fallback);
        self.resolve_model_value(configured, &format!("modelRoles.{role}"))
    }

    pub fn resolve_model_value(
        &self,
        configured: &str,
        source: &str,
    ) -> Result<ResolvedModel, String> {
        let (provider, model) = configured
            .split_once('/')
            .unwrap_or(("deepseek", configured));
        let config = self
            .providers
            .get(provider)
            .cloned()
            .ok_or_else(|| format!("{source} 引用了未配置 provider「{provider}」"))?;
        if model.trim().is_empty() {
            return Err(format!("{source} 缺少模型名"));
        }
        Ok(ResolvedModel {
            provider: provider.to_string(),
            config,
            model: model.to_string(),
        })
    }
}

pub fn config_fingerprint() -> u64 {
    let user_dscode = home_dir()
        .map(|home| home.join(".dscode"))
        .unwrap_or_else(|| PathBuf::from(".dscode"));
    config_fingerprint_in(&user_dscode, Path::new("."))
}

fn config_fingerprint_in(user_dscode: &Path, project_root: &Path) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for path in [
        user_dscode.join("config.yaml"),
        project_root.join(".dscode/config.yaml"),
    ] {
        path.hash(&mut hasher);
        match std::fs::read(&path) {
            Ok(bytes) => {
                0u8.hash(&mut hasher);
                bytes.hash(&mut hasher);
            }
            Err(error) => {
                1u8.hash(&mut hasher);
                error.kind().hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// Four-layer credential resolve (config-onboarding.zh.md §credential four layers):
/// env > ~/.dscode/.credentials.yaml > <cwd>/.env > ~/.dscode/.env.
/// Resolved per operation at runtime — no key snapshot at startup; a rotation takes effect on the next use.
pub fn resolve_credential(name: &str) -> Option<String> {
    let home = home_dir()?;
    resolve_credential_in(name, &home.join(".dscode"), Path::new("."))
}

/// Resolve a credential reference on every operation. `env:NAME` and bare credential
/// keys both use the four-tier resolver; config never carries a literal secret.
pub fn resolve_credential_ref(reference: &str) -> Result<Option<String>, String> {
    let name = reference.strip_prefix("env:").unwrap_or(reference).trim();
    if name.is_empty() {
        return Err("凭据引用不得为空".into());
    }
    Ok(resolve_credential(name))
}

/// Testable entry point: the caller supplies the user .dscode dir and the project root.
pub fn resolve_credential_in(
    name: &str,
    user_dscode: &Path,
    project_root: &Path,
) -> Option<String> {
    // Layer 1: env vars (highest priority).
    if let Ok(v) = std::env::var(name) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    // Layer 2: ~/.dscode/.credentials.yaml (key-value map).
    if let Ok(text) = std::fs::read_to_string(user_dscode.join(".credentials.yaml")) {
        if let Ok(map) = serde_yaml::from_str::<BTreeMap<String, String>>(&text) {
            if let Some(v) = map.get(name) {
                if !v.trim().is_empty() {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    // Layer 3: the project .env.
    if let Some(v) = read_env_file(&project_root.join(".env"), name) {
        return Some(v);
    }
    // Layer 4: ~/.dscode/.env (lowest priority).
    read_env_file(&user_dscode.join(".env"), name)
}

/// Parse `KEY=VALUE` from a .env file (ignores comments and blank lines, tolerates quoted values).
fn read_env_file(path: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().find_map(|line| {
        let line = line.trim();
        if !line.starts_with(name) {
            return None;
        }
        let rest = &line[name.len()..];
        let value = rest.strip_prefix('=')?;
        let value = value.trim().trim_matches('"').trim_matches('\'');
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// Write an always-tier rule into the project config layer (approval.zh.md §2.6): bash first-word
/// patterns go into bash.patterns.allow; other tools go into tools.approval.<tool>: allow.
pub fn write_always_rule(rule: &str, tool_name: &str) -> Result<(), String> {
    write_always_rule_in(Path::new(".dscode"), rule, tool_name)
}

fn write_always_rule_in(dir: &Path, rule: &str, tool_name: &str) -> Result<(), String> {
    if tool_name == "bash" {
        let path = dir.join("config.yaml");
        let layer: ConfigLayer = match std::fs::read_to_string(&path) {
            Ok(text) => serde_yaml::from_str(&text)
                .map_err(|error| format!("配置解析失败 {}：{error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ConfigLayer::default(),
            Err(error) => return Err(format!("读取配置失败 {}：{error}", path.display())),
        };
        let mut allow = layer
            .bash
            .and_then(|bash| bash.patterns)
            .and_then(|patterns| patterns.allow)
            .unwrap_or_default();
        if !allow.iter().any(|existing| existing == rule) {
            allow.push(rule.to_string());
        }
        let value = serde_json::to_string(&allow).map_err(|error| error.to_string())?;
        write_scalar_leaves(
            dir,
            &[(
                vec!["bash".into(), "patterns".into(), "allow".into()],
                value,
            )],
        )
    } else {
        write_scalar_leaves(
            dir,
            &[(
                vec!["tools".into(), "approval".into(), tool_name.into()],
                yaml_string("allow"),
            )],
        )
    }
}

/// Write tui.language into the GLOBAL layer (~/.dscode/config.yaml): the display language
/// is a user preference, not a project attribute (config-onboarding.zh.md §TUI 显示语言).
pub fn write_language_global(lang: Lang) -> Result<(), String> {
    let home = home_dir().ok_or("无法定位用户主目录")?;
    write_language_in(&home.join(".dscode"), lang)
}

/// Testable entry point: update only the tui.language leaf, preserving untouched text.
pub fn write_language_in(dir: &Path, lang: Lang) -> Result<(), String> {
    write_scalar_leaves(
        dir,
        &[(
            vec!["tui".into(), "language".into()],
            yaml_string(lang.as_str()),
        )],
    )
}

pub fn write_render_mode_global(mode: RenderMode) -> Result<(), String> {
    let home = home_dir().ok_or("无法定位用户主目录")?;
    write_scalar_leaves(
        &home.join(".dscode"),
        &[(
            vec!["tui".into(), "renderMode".into()],
            yaml_string(match mode {
                RenderMode::Inline => "inline",
                RenderMode::Fullscreen => "fullscreen",
            }),
        )],
    )
}

pub fn needs_wizard() -> bool {
    home_dir()
        .map(|home| home.join(".dscode/config.yaml"))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_none_or(|text| text.trim().is_empty())
}

pub fn run_wizard() -> Result<(), String> {
    let home = home_dir().ok_or("无法定位用户主目录")?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    run_wizard_in(&home.join(".dscode"), &mut stdin.lock(), &mut stdout.lock())
}

fn run_wizard_in<R: BufRead, W: Write>(
    dir: &Path,
    input: &mut R,
    output: &mut W,
) -> Result<(), String> {
    writeln!(output, "DSCode 首次配置（四步）").map_err(|error| error.to_string())?;
    let provider = prompt(input, output, "1/4 provider id [deepseek]: ")?;
    let provider = if provider.is_empty() {
        "deepseek".into()
    } else {
        provider
    };
    let (base_url, api, model, credential_name) = if provider == "deepseek" {
        (
            "https://api.deepseek.com".into(),
            "openai-completions".into(),
            "deepseek-v4-flash".into(),
            "DEEPSEEK_API_KEY".into(),
        )
    } else {
        let base_url = prompt(input, output, "    base URL: ")?;
        let api = prompt(
            input,
            output,
            "    api [openai-completions/openai-responses/anthropic-messages]: ",
        )?;
        let model = prompt(input, output, "    model: ")?;
        let credential = prompt(input, output, "    credential key [API_KEY]: ")?;
        if base_url.is_empty() || api.is_empty() || model.is_empty() {
            return Err("自定义 provider 的 base URL/api/model 不得为空".into());
        }
        (
            base_url,
            api,
            model,
            if credential.is_empty() {
                "API_KEY".into()
            } else {
                credential
            },
        )
    };
    let key = prompt(
        input,
        output,
        "2/4 API key（可填 env:NAME；留空使用默认 env）: ",
    )?;
    let api_key_ref = if key.is_empty() {
        format!("env:{credential_name}")
    } else if key.starts_with("env:") {
        key
    } else {
        write_credential(dir, &credential_name, &key)?;
        credential_name.clone()
    };
    let mode = prompt(input, output, "3/4 审批模式 [auto/ask/yolo，默认 auto]: ")?;
    let mode = if mode.is_empty() { "auto".into() } else { mode };
    if !matches!(mode.as_str(), "auto" | "ask" | "yolo") {
        return Err("审批模式必须是 auto/ask/yolo".into());
    }
    if mode != "ask" {
        writeln!(
            output,
            "警告：未配置独立 approver 时有效模式为 yolo；无审批提示，但 deny 规则仍生效。"
        )
        .map_err(|error| error.to_string())?;
        let confirm = prompt(input, output, "输入 yes 确认继续: ")?;
        if confirm != "yes" && confirm != "是" {
            return Err("未确认 yolo 风险，wizard 已取消".into());
        }
    }
    let auto = prompt(input, output, "4/4 限额恢复后自动续跑？[Y/n]: ")?;
    let auto = !matches!(auto.to_ascii_lowercase().as_str(), "n" | "no");
    write_scalar_leaves(
        dir,
        &[
            (
                vec!["providers".into(), provider.clone(), "api".into()],
                yaml_string(&api),
            ),
            (
                vec!["providers".into(), provider.clone(), "baseUrl".into()],
                yaml_string(&base_url),
            ),
            (
                vec!["providers".into(), provider.clone(), "apiKey".into()],
                yaml_string(&api_key_ref),
            ),
            (
                vec!["modelRoles".into(), "default".into()],
                yaml_string(&format!("{provider}/{model}")),
            ),
            (vec!["approval".into(), "mode".into()], yaml_string(&mode)),
            (
                vec!["autoContinue".into(), "enabled".into()],
                auto.to_string(),
            ),
        ],
    )?;
    writeln!(output, "配置已写入 {}", dir.join("config.yaml").display())
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn prompt<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    message: &str,
) -> Result<String, String> {
    write!(output, "{message}").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())?;
    let mut value = String::new();
    let count = input
        .read_line(&mut value)
        .map_err(|error| format!("读取输入失败：{error}"))?;
    if count == 0 {
        return Err("wizard 输入已关闭".into());
    }
    Ok(value.trim().to_string())
}

fn yaml_string(value: &str) -> String {
    serde_yaml::to_string(&serde_yaml::Value::String(value.into()))
        .unwrap_or_else(|_| value.to_string())
        .trim()
        .trim_start_matches("---")
        .trim()
        .to_string()
}

fn write_credential(dir: &Path, name: &str, value: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("创建凭据目录失败：{error}"))?;
    let path = dir.join(".credentials.yaml");
    let mut credentials: BTreeMap<String, String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_yaml::from_str(&text).ok())
        .unwrap_or_default();
    credentials.insert(name.into(), value.into());
    let text = serde_yaml::to_string(&credentials).map_err(|error| error.to_string())?;
    atomic_replace(dir, &path, text.as_bytes())
}

fn write_scalar_leaves(dir: &Path, leaves: &[(Vec<String>, String)]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("创建配置目录失败：{error}"))?;
    let _lock = ConfigWriteLock::acquire(dir)?;
    let path = dir.join("config.yaml");
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    for (key, value) in leaves {
        text = set_yaml_leaf(&text, key, value);
    }
    atomic_replace(dir, &path, text.as_bytes())
}

fn set_yaml_leaf(text: &str, path: &[String], value: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut parent: Option<(usize, usize, usize)> = None;
    for (index, line) in lines.iter_mut().enumerate() {
        let Some((indent, key, colon)) = yaml_key(line) else {
            continue;
        };
        while stack.last().is_some_and(|(level, _)| *level >= indent) {
            stack.pop();
        }
        let mut current: Vec<String> = stack.iter().map(|(_, key)| key.clone()).collect();
        current.push(key.clone());
        if current == path {
            let suffix = line[colon + 1..]
                .find(" #")
                .map(|offset| &line[colon + 1 + offset..])
                .unwrap_or("");
            line.replace_range(colon + 1.., &format!(" {value}{suffix}"));
            return format!("{}\n", lines.join("\n"));
        }
        if path.starts_with(&current) && current.len() < path.len() {
            parent = Some((index, indent, current.len()));
        }
        let remainder = line[colon + 1..].trim();
        if remainder.is_empty() || remainder.starts_with('#') {
            stack.push((indent, key));
        }
    }
    let (insert_at, base_indent, matched) = if let Some((line, indent, depth)) = parent {
        let end = (line + 1..lines.len())
            .find(|index| {
                yaml_key(&lines[*index]).is_some_and(|(next_indent, _, _)| next_indent <= indent)
            })
            .unwrap_or(lines.len());
        (end, indent + 2, depth)
    } else {
        (lines.len(), 0, 0)
    };
    let mut additions = Vec::new();
    for (offset, key) in path[matched..].iter().enumerate() {
        let indent = " ".repeat(base_indent + offset * 2);
        if matched + offset + 1 == path.len() {
            additions.push(format!("{indent}{key}: {value}"));
        } else {
            additions.push(format!("{indent}{key}:"));
        }
    }
    lines.splice(insert_at..insert_at, additions);
    format!("{}\n", lines.join("\n"))
}

fn yaml_key(line: &str) -> Option<(usize, String, usize)> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with(['#', '-']) {
        return None;
    }
    let relative = trimmed.find(':')?;
    let key = trimmed[..relative].trim().trim_matches(['\'', '"']);
    if key.is_empty() {
        return None;
    }
    Some((
        line.len() - trimmed.len(),
        key.to_string(),
        line.len() - trimmed.len() + relative,
    ))
}

struct ConfigWriteLock(PathBuf);

impl ConfigWriteLock {
    fn acquire(dir: &Path) -> Result<Self, String> {
        let path = dir.join("config.lock");
        for _ in 0..100 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self(path));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => return Err(format!("创建配置写锁失败：{error}")),
            }
        }
        Err("等待配置写锁超时".into())
    }
}

impl Drop for ConfigWriteLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn atomic_replace(dir: &Path, path: &Path, content: &[u8]) -> Result<(), String> {
    let serial = format!(
        "{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let temp = dir.join(format!("config-{serial}.tmp"));
    let backup = dir.join(format!("config-{serial}.bak"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|error| format!("创建配置临时文件失败：{error}"))?;
    file.write_all(content)
        .map_err(|error| format!("写配置失败：{error}"))?;
    file.sync_all()
        .map_err(|error| format!("同步配置失败：{error}"))?;
    if path.exists() {
        std::fs::rename(path, &backup).map_err(|error| format!("备份旧配置失败：{error}"))?;
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        if backup.exists() {
            let _ = std::fs::rename(&backup, path);
        }
        let _ = std::fs::remove_file(&temp);
        return Err(format!("替换配置失败：{error}"));
    }
    if backup.exists() {
        let _ = std::fs::remove_file(backup);
    }
    Ok(())
}

// ---- Deserialization layer (Option fields carry the "was this configured in this layer" semantics) ----

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct ConfigLayer {
    #[serde(default)]
    approval: Option<ApprovalLayer>,
    #[serde(default, rename = "modelRoles")]
    model_roles: Option<BTreeMap<String, String>>,
    #[serde(default)]
    providers: Option<BTreeMap<String, ProviderLayer>>,
    #[serde(default)]
    tools: Option<ToolsLayer>,
    #[serde(default)]
    bash: Option<BashLayer>,
    #[serde(default)]
    sessions: Option<SessionsLayer>,
    #[serde(default)]
    compaction: Option<CompactionLayer>,
    #[serde(default)]
    hooks: Option<HooksConfig>,
    #[serde(default)]
    task: Option<TaskLayer>,
    #[serde(default)]
    browser: Option<BrowserLayer>,
    #[serde(default)]
    goal: Option<GoalLayer>,
    #[serde(default, rename = "autoContinue")]
    auto_continue: Option<AutoContinueLayer>,
    #[serde(default)]
    tui: Option<TuiLayer>,
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct ApprovalLayer {
    #[serde(default)]
    mode: Option<Mode>,
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct ProviderLayer {
    #[serde(default)]
    api: Option<WireFormat>,
    #[serde(default, rename = "baseUrl")]
    base_url: Option<String>,
    #[serde(default, rename = "apiKey")]
    api_key: Option<String>,
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct ToolsLayer {
    #[serde(default)]
    approval: Option<BTreeMap<String, ToolApproval>>,
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct BashLayer {
    #[serde(default)]
    patterns: Option<BashPatternLayer>,
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct BashPatternLayer {
    #[serde(default)]
    allow: Option<Vec<String>>,
    #[serde(default)]
    deny: Option<Vec<String>>,
    #[serde(default)]
    prompt: Option<Vec<String>>,
}

impl From<BashPatternLayer> for BashPatterns {
    fn from(p: BashPatternLayer) -> Self {
        BashPatterns {
            allow: p.allow.unwrap_or_default(),
            deny: p.deny.unwrap_or_default(),
            prompt: p.prompt.unwrap_or_default(),
        }
    }
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct SessionsLayer {
    #[serde(default)]
    dir: Option<String>,
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct CompactionLayer {
    /// Double Option: outer = whether this layer configured it; inner = distinguishes explicit null (disable) from a value.
    #[serde(default, rename = "autoThreshold", deserialize_with = "double_option")]
    auto_threshold: Option<Option<f64>>,
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct TaskLayer {
    #[serde(default, rename = "maxConcurrency")]
    max_concurrency: Option<usize>,
    #[serde(default, rename = "maxRecursionDepth")]
    max_recursion_depth: Option<u8>,
    #[serde(default)]
    isolation: Option<TaskIsolationLayer>,
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct TaskIsolationLayer {
    mode: Option<String>,
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct BrowserLayer {
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct GoalLayer {
    /// goal.enabled: mounts or removes the whole goal stack (TUI only; headless never mounts).
    #[serde(default)]
    enabled: Option<bool>,
    /// goal.defaultMaxGoalRounds: deployment default for create_goal omitting the rounds budget.
    /// Double Option: outer = configured in this layer; inner None = explicit null (unlimited).
    #[serde(
        default,
        rename = "defaultMaxGoalRounds",
        deserialize_with = "double_option_u64"
    )]
    default_max_goal_rounds: Option<Option<u64>>,
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct AutoContinueLayer {
    /// autoContinue.enabled (limits.zh.md §配置): limit-recovery auto continue.
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Deserialize, Serialize, Default, Clone, Debug)]
struct TuiLayer {
    /// tui.language: zh | en (invalid values fail loud at deserialization with file:line).
    #[serde(default)]
    language: Option<Lang>,
    /// tui.renderMode: inline | fullscreen.
    #[serde(default, rename = "renderMode")]
    render_mode: Option<RenderMode>,
}

/// Distinguishing `null` from "key absent": explicit null deserializes to `Some(None)`.
fn double_option<'de, D>(de: D) -> Result<Option<Option<f64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
}

/// u64 flavor of double_option (goal.defaultMaxGoalRounds: explicit null = unlimited).
fn double_option_u64<'de, D>(de: D) -> Result<Option<Option<u64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
}

/// Parse a single config file; a missing file counts as an empty layer (zero config is legal).
fn parse_layer(path: &Path) -> Result<ConfigLayer, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_layer_text(&text, path),
        Err(_) => Ok(ConfigLayer::default()),
    }
}

/// Parse config text: YAML syntax errors and known-key type errors both fail loud with file:line;
/// unknown keys are ignored (lenient), known-key type errors are not.
fn parse_layer_text(text: &str, file: &Path) -> Result<ConfigLayer, String> {
    serde_yaml::from_str(text).map_err(|e| format!("配置解析失败 {}：{e}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 零配置默认值() {
        let c = Config::from_str_layers("", "").unwrap();
        assert_eq!(c.approval_mode, Mode::Auto);
        assert_eq!(c.compaction_auto_threshold, Some(0.8));
        assert!(c.sessions_dir.ends_with("sessions"));
        assert!(!c.approver_configured());
    }

    #[test]
    fn 双层合并_项目覆盖全局() {
        let global = "approval:\n  mode: ask\nmodelRoles:\n  default: deepseek-chat\nsessions:\n  dir: /global/sessions\ncompaction:\n  autoThreshold: 0.5\n";
        let project = "approval:\n  mode: yolo\nsessions:\n  dir: /proj/sessions\n";
        let c = Config::from_str_layers(global, project).unwrap();
        assert_eq!(c.approval_mode, Mode::Yolo);
        assert_eq!(c.model_roles["default"], "deepseek-chat"); // global-only key preserved
        assert_eq!(c.sessions_dir, PathBuf::from("/proj/sessions"));
        assert_eq!(c.compaction_auto_threshold, Some(0.5)); // global value not overridden
    }

    #[test]
    fn yaml语法错_fail_loud带file和line() {
        let bad = "approval:\n  mode: [ask\n";
        let err = Config::from_str_layers(bad, "").unwrap_err();
        assert!(err.contains("配置解析失败"), "实际错误：{err}");
        assert!(err.contains("line"), "错误应带行号：{err}");
        // File-level fail loud (load_dirs path).
        let err2 = parse_layer_text(bad, Path::new("C:/x/.dscode/config.yaml")).unwrap_err();
        assert!(err2.contains("config.yaml"), "实际错误：{err2}");
    }

    #[test]
    fn 已知键类型错_fail_loud带line() {
        let bad = "compaction:\n  autoThreshold: abc\n";
        let err = Config::from_str_layers(bad, "").unwrap_err();
        assert!(err.contains("配置解析失败"), "实际错误：{err}");
        assert!(
            err.contains("autoThreshold") || err.contains("line"),
            "实际错误：{err}"
        );
    }

    #[test]
    fn 未知审批模式_fail_loud() {
        let err = Config::from_str_layers("approval:\n  mode: aggressive\n", "").unwrap_err();
        assert!(
            err.contains("aggressive") || err.contains("ask"),
            "实际错误：{err}"
        );
    }

    #[test]
    fn 未知键忽略_宽松() {
        let c = Config::from_str_layers("某个未来键:\n  nested: 1\napproval:\n  mode: yolo\n", "")
            .unwrap();
        assert_eq!(c.approval_mode, Mode::Yolo);
    }

    #[test]
    fn 未知模型角色_fail_loud() {
        let err = Config::from_str_layers("modelRoles:\n  smol: x\n", "").unwrap_err();
        assert!(err.contains("smol"), "实际错误：{err}");
    }

    #[test]
    fn modelRoles缺失provider首次解析失败() {
        let c = Config::from_str_layers("modelRoles:\n  default: missing/foo\n", "").unwrap();
        let err = c.resolve_model("default", "fallback").unwrap_err();
        assert!(err.contains("missing"), "实际错误：{err}");
    }

    #[test]
    fn 每工具覆盖与bash_pattern进规则() {
        let yaml = "tools:\n  approval:\n    write: prompt\n    bash: deny\nbash:\n  patterns:\n    allow:\n      - git status\n    deny:\n      - rm -rf *\n";
        let c = Config::from_str_layers(yaml, "").unwrap();
        assert_eq!(c.rules.tools_global["write"], ToolApproval::Prompt);
        assert_eq!(c.rules.tools_global["bash"], ToolApproval::Deny);
        assert_eq!(c.rules.bash_global.allow, vec!["git status".to_string()]);
        assert_eq!(c.rules.bash_global.deny, vec!["rm -rf *".to_string()]);
    }

    #[test]
    fn 非法glob_pattern_fail_loud() {
        let err =
            Config::from_str_layers("bash:\n  patterns:\n    deny:\n      - '[unclosed'\n", "")
                .unwrap_err();
        assert!(err.contains("非法 glob"), "实际错误：{err}");
    }

    #[test]
    fn threshold越界_fail_loud() {
        let err = Config::from_str_layers("compaction:\n  autoThreshold: 1.5\n", "").unwrap_err();
        assert!(err.contains("(0,1]"), "实际错误：{err}");
    }

    #[test]
    fn approver配置与有效模式() {
        let c = Config::from_str_layers("modelRoles:\n  approver: deepseek-chat\n", "").unwrap();
        assert!(c.approver_configured());
        assert_eq!(c.effective_mode().mode, Mode::Auto);
        // No approver configured: auto effectively lands on yolo + the onboarding flag.
        let c2 = Config::from_str_layers("", "").unwrap();
        let e = c2.effective_mode();
        assert_eq!(e.mode, Mode::Yolo);
        assert!(e.onboarding_needed);
    }

    #[test]
    fn 凭据四层_优先级与缺失() {
        let user = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let user_dscode = user.path().join(".dscode");
        std::fs::create_dir_all(&user_dscode).unwrap();

        // All missing → None (unique key name, avoids interference from real environment variables).
        let name = "DSCODE_TEST_CRED_KEY_AB12";
        std::env::remove_var(name);
        assert_eq!(resolve_credential_in(name, &user_dscode, proj.path()), None);

        // Layer 4: the user .env.
        std::fs::write(user_dscode.join(".env"), format!("{name}=from-user-env\n")).unwrap();
        assert_eq!(
            resolve_credential_in(name, &user_dscode, proj.path()).as_deref(),
            Some("from-user-env")
        );

        // Layer 3: the project .env overrides the user .env.
        std::fs::write(
            proj.path().join(".env"),
            format!("{name}=\"from-proj-env\"\n"),
        )
        .unwrap();
        assert_eq!(
            resolve_credential_in(name, &user_dscode, proj.path()).as_deref(),
            Some("from-proj-env")
        );

        // Layer 2: the credentials file overrides .env.
        std::fs::write(
            user_dscode.join(".credentials.yaml"),
            format!("{name}: from-cred-yaml\n"),
        )
        .unwrap();
        assert_eq!(
            resolve_credential_in(name, &user_dscode, proj.path()).as_deref(),
            Some("from-cred-yaml")
        );

        // Layer 1: env is highest priority.
        std::env::set_var(name, "from-env");
        assert_eq!(
            resolve_credential_in(name, &user_dscode, proj.path()).as_deref(),
            Some("from-env")
        );
        std::env::remove_var(name);
    }

    #[test]
    fn env文件解析容忍注释与空值() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "# 注释\nDEEPSEEK_API_KEY=  \"sk-test\"  \nEMPTY=\n",
        )
        .unwrap();
        assert_eq!(
            read_env_file(&dir.path().join(".env"), "DEEPSEEK_API_KEY").as_deref(),
            Some("sk-test")
        );
        assert_eq!(read_env_file(&dir.path().join(".env"), "EMPTY"), None);
    }

    #[test]
    fn goal键双层合并与默认() {
        // Defaults: enabled=true (TUI mounting), defaultMaxGoalRounds=50.
        let c = Config::from_str_layers("", "").unwrap();
        assert!(c.goal_enabled);
        assert_eq!(c.goal_default_max_rounds, Some(50));

        // Project overrides global; explicit null = unlimited.
        let c = Config::from_str_layers(
            "goal:\n  enabled: false\n  defaultMaxGoalRounds: 30\n",
            "goal:\n  defaultMaxGoalRounds: null\n",
        )
        .unwrap();
        assert!(!c.goal_enabled, "project 未设置时 global 生效");
        assert_eq!(
            c.goal_default_max_rounds, None,
            "project 显式 null 覆盖为不限"
        );

        // Project overrides global enabled too.
        let c = Config::from_str_layers("goal:\n  enabled: false\n", "goal:\n  enabled: true\n")
            .unwrap();
        assert!(c.goal_enabled);
    }

    #[test]
    fn goal默认轮数非法值报错() {
        let err = Config::from_str_layers("goal:\n  defaultMaxGoalRounds: 0\n", "").unwrap_err();
        assert!(err.contains("defaultMaxGoalRounds"), "报错点名键：{err}");
    }

    #[test]
    fn autoContinue双层合并与默认() {
        // Factory default: on (limits.zh.md §恢复触发 — auto continue ships enabled).
        assert!(
            Config::from_str_layers("", "")
                .unwrap()
                .auto_continue_enabled
        );

        // Global off.
        assert!(
            !Config::from_str_layers("autoContinue:\n  enabled: false\n", "")
                .unwrap()
                .auto_continue_enabled
        );

        // Project overrides global (re-enable).
        assert!(
            Config::from_str_layers(
                "autoContinue:\n  enabled: false\n",
                "autoContinue:\n  enabled: true\n"
            )
            .unwrap()
            .auto_continue_enabled
        );

        // Project-only off.
        assert!(
            !Config::from_str_layers("", "autoContinue:\n  enabled: false\n")
                .unwrap()
                .auto_continue_enabled
        );
    }
    #[test]
    fn tui_language双层合并与非法值() {
        // Factory default zh.
        assert_eq!(Config::from_str_layers("", "").unwrap().language, Lang::Zh);
        // Global en, project zh → project wins; project-only en also wins.
        let c =
            Config::from_str_layers("tui:\n  language: en\n", "tui:\n  language: zh\n").unwrap();
        assert_eq!(c.language, Lang::Zh);
        assert_eq!(
            Config::from_str_layers("", "tui:\n  language: en\n")
                .unwrap()
                .language,
            Lang::En
        );
        // Invalid value fails loud at parse time with file:line.
        let err = Config::from_str_layers("tui:\n  language: fr\n", "").unwrap_err();
        assert!(
            err.contains("language") || err.contains("fr"),
            "报错点名键值：{err}"
        );
        assert!(err.contains("line"), "错误应带行号：{err}");
    }

    #[test]
    fn language写回全局层往返() {
        let user = tempfile::tempdir().unwrap();
        let dscode = user.path().join(".dscode");
        std::fs::create_dir_all(&dscode).unwrap();
        // Pre-existing unknown key survives the whole-tree write-back.
        std::fs::write(dscode.join("config.yaml"), "futureKey: 1\n").unwrap();
        write_language_in(&dscode, Lang::En).unwrap();
        let text = std::fs::read_to_string(dscode.join("config.yaml")).unwrap();
        assert!(text.contains("futureKey: 1"), "未知键保留：{text}");
        assert!(text.contains("language: en"), "写入语言叶子：{text}");
        let proj = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load_dirs(&dscode, proj.path()).unwrap().language,
            Lang::En
        );
        // Overwrite round-trips.
        write_language_in(&dscode, Lang::Zh).unwrap();
        assert_eq!(
            Config::load_dirs(&dscode, proj.path()).unwrap().language,
            Lang::Zh
        );
    }
    #[test]
    fn browser_endpoint双层合并与校验() {
        assert_eq!(
            Config::from_str_layers("", "").unwrap().browser_endpoint,
            None
        );
        let config = Config::from_str_layers(
            "browser:\n  endpoint: http://127.0.0.1:9222\n",
            "browser:\n  endpoint: http://127.0.0.1:9333\n",
        )
        .unwrap();
        assert_eq!(
            config.browser_endpoint.as_deref(),
            Some("http://127.0.0.1:9333")
        );
        let error =
            Config::from_str_layers("browser:\n  endpoint: ws://127.0.0.1:9222\n", "").unwrap_err();
        assert!(error.contains("browser.endpoint"), "{error}");
    }
    #[test]
    fn wizard零配置写回安全凭据且重复运行保留注释() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".dscode");
        let mut input = std::io::Cursor::new("\nraw-secret\nask\nn\n".as_bytes());
        let mut output = Vec::new();
        run_wizard_in(&dir, &mut input, &mut output).unwrap();

        let config_path = dir.join("config.yaml");
        let config = std::fs::read_to_string(&config_path).unwrap();
        let credentials = std::fs::read_to_string(dir.join(".credentials.yaml")).unwrap();
        assert!(config.contains("deepseek-v4-flash"), "{config}");
        assert!(config.contains("mode: ask"), "{config}");
        assert!(config.contains("enabled: false"), "{config}");
        assert!(
            !config.contains("raw-secret"),
            "secret 不得写入 config：{config}"
        );
        assert!(credentials.contains("raw-secret"), "{credentials}");
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("1/4") && output.contains("4/4"), "{output}");

        std::fs::write(&config_path, format!("# keep-comment\n{config}")).unwrap();
        let mut input = std::io::Cursor::new("\nenv:ROTATED\nask\ny\n".as_bytes());
        run_wizard_in(&dir, &mut input, &mut Vec::new()).unwrap();
        let rerun = std::fs::read_to_string(&config_path).unwrap();
        assert!(rerun.contains("# keep-comment"), "{rerun}");
        assert!(rerun.contains("env:ROTATED"), "{rerun}");
        assert!(rerun.contains("enabled: true"), "{rerun}");
    }
    #[test]
    fn always规则叶子写回保留注释且幂等() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".dscode");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(
            &path,
            "# keep-top\nfutureKey: 1\nbash:\n  patterns:\n    allow: [\"git status\"] # keep-allow\ntools:\n  approval:\n    read: deny\n",
        )
        .unwrap();

        write_always_rule_in(&dir, "cargo test", "bash").unwrap();
        write_always_rule_in(&dir, "cargo test", "bash").unwrap();
        write_always_rule_in(&dir, "", "write").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# keep-top") && text.contains("# keep-allow"),
            "{text}"
        );
        assert!(
            text.contains("futureKey: 1") && text.contains("read: deny"),
            "{text}"
        );
        assert!(text.contains("write: allow"), "{text}");
        assert_eq!(text.matches("cargo test").count(), 1, "{text}");
        serde_yaml::from_str::<ConfigLayer>(&text).unwrap();
        assert_eq!(
            std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name() != "config.yaml")
                .count(),
            0,
            "原子写回不得遗留 lock/tmp/bak"
        );
    }
    #[test]
    fn 配置指纹随任一层内容变化() {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let user_dscode = user.path().join(".dscode");
        std::fs::create_dir_all(&user_dscode).unwrap();
        let initial = config_fingerprint_in(&user_dscode, project.path());
        std::fs::write(user_dscode.join("config.yaml"), "approval:\n  mode: ask\n").unwrap();
        let global_changed = config_fingerprint_in(&user_dscode, project.path());
        assert_ne!(initial, global_changed);
        std::fs::create_dir_all(project.path().join(".dscode")).unwrap();
        std::fs::write(
            project.path().join(".dscode/config.yaml"),
            "tools:\n  approval:\n    bash: deny\n",
        )
        .unwrap();
        assert_ne!(
            global_changed,
            config_fingerprint_in(&user_dscode, project.path())
        );
    }
}
