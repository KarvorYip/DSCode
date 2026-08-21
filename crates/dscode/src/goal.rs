//! Goal subsystem (goal.zh.md): a cross-turn completion commitment driven by the goal-round-driver.
//! Domain layer (pure state, dsh goal-package lineage) + process-local runtime (turn flags, host
//! proof, pending event queue). State machine: active ⇄ paused → complete | blocked. Every
//! transition is CAS-guarded by `revision` (stale edits are rejected and the current state returned)
//! and emitted as one append-only `goal/change` event carrying the full snapshot.
//! Arming (续行启用) is process-local state and NEVER enters events: after resume/fork the goal
//! exists, is visible and editable, but drives no rounds until the user runs `/goal resume`.

use crate::i18n::{tr, Lang, StrKey};
use serde_json::{json, Value};

/// Hard floor from dsh: this many consecutive goal rounds without progress force `blocked`.
pub const BLOCKED_AFTER_CONSECUTIVE_ROUNDS: u32 = 3;

/// Soft-warning threshold: remaining budget below 20% of the budget injects a warning
/// into the next continuation prompt (hard stop happens only at zero).
pub const SOFT_WARNING_RATIO: f64 = 0.2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GoalStatus {
    Active,
    Paused,
    Complete,
    Blocked,
}

impl GoalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::Complete => "complete",
            GoalStatus::Blocked => "blocked",
        }
    }
    fn from_event(s: &str) -> Option<Self> {
        match s {
            "active" => Some(GoalStatus::Active),
            "paused" => Some(GoalStatus::Paused),
            "complete" => Some(GoalStatus::Complete),
            "blocked" => Some(GoalStatus::Blocked),
            _ => None,
        }
    }
}

/// The single current goal. Budgets are optional: None = unlimited.
#[derive(Clone, Debug)]
pub struct Goal {
    pub objective: String,
    pub status: GoalStatus,
    pub revision: u64,
    pub max_goal_rounds: Option<u64>,
    pub token_budget: Option<u64>,
    pub rounds_used: u64,
    pub tokens_used: u64,
    pub blocked_reason: Option<String>,
    /// Consecutive goal rounds with no progress (driver-side, see record_progress).
    pub no_progress_streak: u32,
}

impl Goal {
    /// `goal/change` event data: the action plus the post-transition snapshot (camelCase keys,
    /// dsh-style). Arming is deliberately absent — process-local only.
    fn event_data(&self, action: &str) -> Value {
        json!({
            "action": action,
            "objective": self.objective,
            "status": self.status.as_str(),
            "revision": self.revision,
            "maxGoalRounds": self.max_goal_rounds,
            "tokenBudget": self.token_budget,
            "roundsUsed": self.rounds_used,
            "tokensUsed": self.tokens_used,
            "blockedReason": self.blocked_reason,
        })
    }

    fn compact_json(&self) -> Value {
        json!({
            "objective": self.objective,
            "status": self.status.as_str(),
            "revision": self.revision,
            "maxGoalRounds": self.max_goal_rounds,
            "tokenBudget": self.token_budget,
            "roundsUsed": self.rounds_used,
            "tokensUsed": self.tokens_used,
            "blockedReason": self.blocked_reason,
        })
    }

    /// Rebuild from the latest `goal/change` snapshot (resume/fork replay; arming is lost by design).
    fn from_event_data(data: &Value) -> Option<Self> {
        Some(Self {
            objective: data.get("objective")?.as_str()?.to_string(),
            status: GoalStatus::from_event(data.get("status")?.as_str()?)?,
            revision: data.get("revision")?.as_u64()?,
            max_goal_rounds: data.get("maxGoalRounds").and_then(Value::as_u64),
            token_budget: data.get("tokenBudget").and_then(Value::as_u64),
            rounds_used: data.get("roundsUsed").and_then(Value::as_u64).unwrap_or(0),
            tokens_used: data.get("tokensUsed").and_then(Value::as_u64).unwrap_or(0),
            blocked_reason: data
                .get("blockedReason")
                .and_then(Value::as_str)
                .map(str::to_string),
            no_progress_streak: 0,
        })
    }
}

/// update_goal action set (five model actions; `clear` is a user-command-only action).
#[derive(Clone, Debug)]
pub enum GoalAction {
    Edit(GoalPatch),
    Pause,
    Resume,
    Complete,
    Blocked { reason: Option<String> },
}

impl GoalAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalAction::Edit(_) => "edit",
            GoalAction::Pause => "pause",
            GoalAction::Resume => "resume",
            GoalAction::Complete => "complete",
            GoalAction::Blocked { .. } => "blocked",
        }
    }
}

/// Edit payload: a field present means "change it"; `Some(None)` for a budget field means "set to unlimited".
#[derive(Clone, Debug, Default)]
pub struct GoalPatch {
    pub objective: Option<String>,
    pub max_goal_rounds: Option<Option<u64>>,
    pub token_budget: Option<Option<u64>>,
}

/// Domain errors. StaleRevision always carries the current goal so the caller can return the
/// current state to the model (spec: a rejected change must return the current state).
#[derive(Clone, Debug)]
pub enum GoalError {
    AlreadyExists,
    NoGoal,
    StaleRevision(Box<Goal>),
    InvalidTransition {
        action: &'static str,
        from: &'static str,
    },
    EmptyObjective,
}

impl GoalError {
    /// Compact-JSON error body for tool output.
    fn to_json(&self) -> Value {
        let (code, message): (&str, String) = match self {
            GoalError::AlreadyExists => ("goal_exists", "已有活跃 goal，请改用 update_goal".into()),
            GoalError::NoGoal => ("no_goal", "当前没有 goal".into()),
            GoalError::StaleRevision(_) => (
                "stale_revision",
                "revision 已过期（他人已先行修改），请以返回的当前状态为准".into(),
            ),
            GoalError::InvalidTransition { action, from } => (
                "invalid_transition",
                format!("不允许的转换：{from} 状态下执行 {action}"),
            ),
            GoalError::EmptyObjective => ("empty_objective", "objective 不能为空".into()),
        };
        json!({ "ok": false, "error": code, "message": message })
    }
}

/// Pure goal state: one current goal + the process-local arming flag.
pub struct GoalState {
    goal: Option<Goal>,
    armed: bool,
}

impl Default for GoalState {
    fn default() -> Self {
        Self {
            goal: None,
            armed: false,
        }
    }
}

impl GoalState {
    pub fn get(&self) -> Option<&Goal> {
        self.goal.as_ref()
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// A goal occupies the single slot while active or paused; terminal goals stay visible
    /// for `get_goal` but free the slot for a new create (dsh single-current-goal semantics).
    fn slot_busy(&self) -> bool {
        matches!(
            self.goal.as_ref().map(|g| g.status),
            Some(GoalStatus::Active) | Some(GoalStatus::Paused)
        )
    }

    /// Create: only when the slot is free. Arming is set — creation anchors on a
    /// human-authorized turn (host proof checked by the runtime layer) and the driver
    /// takes over when that turn naturally ends.
    pub fn create(
        &mut self,
        objective: &str,
        max_goal_rounds: Option<u64>,
        token_budget: Option<u64>,
    ) -> Result<Goal, GoalError> {
        if self.slot_busy() {
            return Err(GoalError::AlreadyExists);
        }
        let obj = objective.trim();
        if obj.is_empty() {
            return Err(GoalError::EmptyObjective);
        }
        let goal = Goal {
            objective: obj.to_string(),
            status: GoalStatus::Active,
            revision: 1,
            max_goal_rounds,
            token_budget,
            rounds_used: 0,
            tokens_used: 0,
            blocked_reason: None,
            no_progress_streak: 0,
        };
        self.armed = true;
        self.goal = Some(goal.clone());
        Ok(goal)
    }

    /// Compare-and-set update. A revision mismatch is rejected with the current state;
    /// invalid transitions are rejected by the state machine (active ⇄ paused → complete|blocked).
    pub fn update(&mut self, revision: u64, action: GoalAction) -> Result<Goal, GoalError> {
        let Some(goal) = self.goal.as_mut() else {
            return Err(GoalError::NoGoal);
        };
        if revision != goal.revision {
            let current = goal.clone();
            return Err(GoalError::StaleRevision(Box::new(current)));
        }
        // State machine: pause only from active; resume only from paused; terminal states final.
        match (&action, goal.status) {
            (GoalAction::Pause, GoalStatus::Active) => {
                goal.status = GoalStatus::Paused;
                // A paused goal drives no rounds and burns no budget.
                self.armed = false;
            }
            (GoalAction::Resume, GoalStatus::Paused) => {
                goal.status = GoalStatus::Active;
                // Model-side resume un-pauses but never re-arms: rearming requires the
                // explicit user action `/goal resume` (spec disarm/rearm).
            }
            (GoalAction::Complete, GoalStatus::Active)
            | (GoalAction::Complete, GoalStatus::Paused)
            | (GoalAction::Blocked { .. }, GoalStatus::Active)
            | (GoalAction::Blocked { .. }, GoalStatus::Paused) => {
                if let GoalAction::Blocked { reason } = &action {
                    goal.blocked_reason =
                        Some(reason.clone().unwrap_or_else(|| "unspecified".into()));
                    goal.status = GoalStatus::Blocked;
                } else {
                    goal.status = GoalStatus::Complete;
                }
                self.armed = false;
            }
            (GoalAction::Edit(_), GoalStatus::Active)
            | (GoalAction::Edit(_), GoalStatus::Paused) => {
                if let GoalAction::Edit(patch) = &action {
                    if let Some(o) = &patch.objective {
                        let o = o.trim();
                        if o.is_empty() {
                            return Err(GoalError::EmptyObjective);
                        }
                        goal.objective = o.to_string();
                    }
                    if let Some(m) = &patch.max_goal_rounds {
                        goal.max_goal_rounds = *m;
                    }
                    if let Some(t) = &patch.token_budget {
                        goal.token_budget = *t;
                    }
                    // Editing budgets clears the stop condition bookkeeping only through new values;
                    // rounds/tokens already consumed stay consumed (lifecycle accounting).
                }
            }
            (action, status) => {
                return Err(GoalError::InvalidTransition {
                    action: action.as_str(),
                    from: status.as_str(),
                });
            }
        }
        goal.revision += 1;
        goal.no_progress_streak = 0;
        let snapshot = goal.clone();
        Ok(snapshot)
    }

    /// User-side clear (`/goal clear`): terminate and drop the current goal. The event
    /// carries the pre-clear snapshot with status "cleared".
    pub fn clear(&mut self) -> Option<Goal> {
        let gone = self.goal.take()?;
        self.armed = false;
        Some(gone)
    }

    // —— driver-side accounting ——

    /// Whether the continuation driver should keep driving: an active, armed goal.
    pub fn should_drive(&self) -> bool {
        matches!(
            self.goal.as_ref().map(|g| g.status),
            Some(GoalStatus::Active)
        ) && self.armed
    }

    /// Reserve one goal round (hard stop: never reserve past the rounds budget).
    /// Returns false when the rounds budget leaves no room.
    pub fn charge_round(&mut self) -> bool {
        let Some(goal) = self.goal.as_mut() else {
            return false;
        };
        if goal.status != GoalStatus::Active {
            return false;
        }
        if let Some(max) = goal.max_goal_rounds {
            if goal.rounds_used >= max {
                return false;
            }
        }
        goal.rounds_used += 1;
        true
    }

    /// Accumulate model-request usage into the token budget. Only charged while active —
    /// the pause window is excluded by definition (create → terminal minus pause windows).
    pub fn charge_tokens(&mut self, total_tokens: u64) {
        if let Some(goal) = self.goal.as_mut() {
            if goal.status == GoalStatus::Active {
                goal.tokens_used += total_tokens;
            }
        }
    }

    pub fn tokens_exhausted(&self) -> bool {
        self.goal
            .as_ref()
            .is_some_and(|g| g.token_budget.is_some_and(|b| g.tokens_used >= b))
    }

    /// Why the driver must stop before reserving the next round (hard stop explanation;
    /// the goal stays active-but-stopped, spec threshold behavior).
    pub fn stop_reason(&self) -> Option<&'static str> {
        let Some(goal) = self.goal.as_ref() else {
            return None;
        };
        if goal.status != GoalStatus::Active || !self.armed {
            return None;
        }
        if self.tokens_exhausted() {
            return Some("token 预算已耗尽（goal 保持 active，可 /goal resume 后 edit 加预算）");
        }
        if goal.max_goal_rounds.is_some_and(|m| goal.rounds_used >= m) {
            return Some("rounds 预算已耗尽（goal 保持 active，可 /goal resume 后 edit 加预算）");
        }
        None
    }

    /// Soft warning for the next continuation prompt: any budget with remaining < 20%
    /// (unlimited budgets never warn; exhaustion is the driver's hard stop, not a warning).
    pub fn soft_warning(&self) -> Option<String> {
        let goal = self.goal.as_ref()?;
        if goal.status != GoalStatus::Active {
            return None;
        }
        let rounds_warn = goal
            .max_goal_rounds
            .filter(|&m| goal.rounds_used < m)
            .and_then(|m| {
                let left = m - goal.rounds_used;
                ((left as f64) < SOFT_WARNING_RATIO * m as f64)
                    .then(|| format!("剩余 goal round 预算 {left}/{m}"))
            });
        let tokens_warn = goal
            .token_budget
            .filter(|&b| goal.tokens_used < b)
            .and_then(|b| {
                let left = b - goal.tokens_used;
                ((left as f64) < SOFT_WARNING_RATIO * b as f64)
                    .then(|| format!("剩余 token 预算 {left}/{b}"))
            });
        match (rounds_warn, tokens_warn) {
            (Some(r), Some(t)) => Some(format!("{r}；{t}")),
            (Some(r), None) | (None, Some(r)) => Some(r),
            (None, None) => None,
        }
    }

    /// Progress bookkeeping after one goal round. Implementation interpretation (testable
    /// definition): a round counts as progress when the model made at least one tool call
    /// OR produced non-empty assistant text. Three consecutive rounds without progress
    /// force `blocked` (dsh hard floor); returns the forced-blocked snapshot + event when it fires.
    pub fn record_round_progress(&mut self, had_progress: bool) -> Option<(Goal, Value)> {
        let goal = self.goal.as_mut()?;
        if goal.status != GoalStatus::Active {
            return None;
        }
        if had_progress {
            goal.no_progress_streak = 0;
            return None;
        }
        goal.no_progress_streak += 1;
        if goal.no_progress_streak < BLOCKED_AFTER_CONSECUTIVE_ROUNDS {
            return None;
        }
        goal.status = GoalStatus::Blocked;
        goal.blocked_reason = Some("consecutive-no-progress".into());
        goal.revision += 1;
        goal.no_progress_streak = 0;
        self.armed = false;
        let snapshot = goal.clone();
        Some((snapshot.clone(), snapshot.event_data("blocked")))
    }
}

/// Process-local goal runtime: turn flags (host proof, goal-round marking, subagent marker,
/// per-turn tool mutual exclusion) plus the pending `goal/change` event queue. The chat loop
/// flushes the queue into the session log after tool execution; the TUI flushes after slash
/// commands. Arming lives in GoalState and never crosses a process boundary.
pub struct GoalRuntime {
    pub state: GoalState,
    /// Deployment default for `create_goal` omitting max_goal_rounds (config goal.defaultMaxGoalRounds).
    pub default_max_rounds: Option<u64>,
    /// Host proof: the current turn contains an accepted {kind:'user'} message.
    pub host_proof: bool,
    /// The current turn is a goal round (blocks create_goal; gates complete/blocked).
    pub in_goal_round: bool,
    /// This execution context belongs to a subagent (spawn agent wires this flag).
    pub is_subagent: bool,
    /// Per-turn goal-tool mutual exclusion (one goal tool call per turn).
    goal_tool_used_this_turn: bool,
    pending_events: Vec<Value>,
}

impl GoalRuntime {
    pub fn new(default_max_rounds: Option<u64>) -> Self {
        Self {
            state: GoalState::default(),
            default_max_rounds,
            host_proof: false,
            in_goal_round: false,
            is_subagent: false,
            goal_tool_used_this_turn: false,
            pending_events: Vec::new(),
        }
    }

    /// Rebuild state from a session log (resume/fork): fold to the latest goal/change
    /// snapshot. The result is disarmed by construction — arming is never persisted.
    pub fn replay(events: &[crate::session::Event], default_max_rounds: Option<u64>) -> Self {
        let mut rt = Self::new(default_max_rounds);
        if let Some(ev) = events.iter().rev().find(|e| e.kind == "goal/change") {
            if ev.data.get("action").and_then(Value::as_str) != Some("clear") {
                rt.state.goal = Goal::from_event_data(&ev.data);
            }
        }
        rt
    }

    /// Reset turn-scoped flags at turn start. Human turns set host proof; goal rounds
    /// explicitly do not (they must not grant creation rights).
    pub fn begin_turn(&mut self, host_proof: bool, in_goal_round: bool) {
        self.host_proof = host_proof;
        self.in_goal_round = in_goal_round;
        self.goal_tool_used_this_turn = false;
    }

    /// Take pending goal/change event payloads (the caller writes them to the log).
    pub fn drain_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.pending_events)
    }

    fn emit(&mut self, goal: &Goal, action: &str) {
        self.pending_events.push(goal.event_data(action));
    }

    // —— model-side tool entry points (host proof + mutual exclusion enforced here) ——

    /// get_goal: current goal or none (compact JSON).
    pub fn tool_get(&self) -> Value {
        json!({ "goal": self.state.get().map(Goal::compact_json) })
    }

    /// create_goal. Three host-proof gates: no goal in the slot, the current turn carries an
    /// accepted user message (goal rounds and subagent-only turns never do), not a subagent.
    /// Mutual exclusion: one goal tool call per turn.
    pub fn tool_create(
        &mut self,
        objective: &str,
        max_goal_rounds: Option<u64>,
        token_budget: Option<u64>,
    ) -> Value {
        if self.goal_tool_used_this_turn {
            return self.tool_locked();
        }
        if self.is_subagent {
            return self.tool_denied("subagent_denied", "subagent 不可创建 goal");
        }
        if self.in_goal_round {
            return self.tool_denied("goal_round_denied", "goal round 内不可创建 goal");
        }
        if !self.host_proof {
            return self.tool_denied("host_proof_denied", "仅含已接受用户消息的轮次可创建 goal");
        }
        // Omitted/null rounds budget falls to the deployment default (config goal.defaultMaxGoalRounds).
        let max = max_goal_rounds.or(self.default_max_rounds);
        self.goal_tool_used_this_turn = true;
        match self.state.create(objective, max, token_budget) {
            Ok(goal) => {
                self.emit(&goal, "create");
                json!({ "ok": true, "goal": goal.compact_json() })
            }
            Err(e) => self.tool_error(e),
        }
    }

    /// update_goal. complete/blocked must be called inside the current goal round (the model
    /// must actually be driving toward the goal, not closing it from a human turn in passing).
    pub fn tool_update(&mut self, revision: u64, action: GoalAction) -> Value {
        if self.goal_tool_used_this_turn {
            return self.tool_locked();
        }
        if matches!(action, GoalAction::Complete | GoalAction::Blocked { .. })
            && !self.in_goal_round
        {
            return self.tool_denied(
                "not_in_goal_round",
                "complete/blocked 必须在当前 goal round 内调用",
            );
        }
        self.goal_tool_used_this_turn = true;
        let action_str = action.as_str();
        match self.state.update(revision, action) {
            Ok(goal) => {
                self.emit(&goal, action_str);
                json!({ "ok": true, "goal": goal.compact_json() })
            }
            Err(e) => self.tool_error(e),
        }
    }

    /// Driver-side forced blocked (consecutive no-progress) — bypasses every tool gate and
    /// emits the event directly.
    pub fn force_blocked_event(&mut self, event: Value) {
        self.pending_events.push(event);
    }

    /// Driver-side token charge (Usage events from the provider; pause windows excluded in state).
    pub fn charge_tokens(&mut self, total_tokens: u64) {
        self.state.charge_tokens(total_tokens);
    }

    fn tool_denied(&self, code: &str, message: &str) -> Value {
        json!({ "ok": false, "error": code, "message": message, "goal": self.state.get().map(Goal::compact_json) })
    }

    fn tool_locked(&self) -> Value {
        json!({ "ok": false, "error": "goal_tool_busy", "message": "同一 turn 内不允许并发 goal 工具调用" })
    }

    fn tool_error(&self, e: GoalError) -> Value {
        let mut v = e.to_json();
        // Stale rejection (and every error) returns the current state alongside.
        if let GoalError::StaleRevision(current) = &e {
            v["goal"] = current.compact_json();
        } else if let Some(g) = self.state.get() {
            v["goal"] = g.compact_json();
        }
        v
    }

    // —— user-side entry points (`/goal` command forms; no host proof, no tool gates) ——

    /// `/goal <objective>`: the user is the authority; creation is direct. Arming is set
    /// (a user-created goal drives rounds when the turn ends? No — slash commands run while
    /// idle; arming still only matters for the driver after the next turn ends).
    pub fn user_create(&mut self, objective: &str) -> Value {
        match self.state.create(objective, self.default_max_rounds, None) {
            Ok(goal) => {
                self.emit(&goal, "create");
                goal.compact_json()
            }
            Err(e) => e.to_json(),
        }
    }

    /// `/goal edit <objective>`: rewrites the objective using the current revision (the
    /// user's edit immediately overrides any in-flight model edit — CAS with a fresh revision).
    pub fn user_edit(&mut self, objective: &str) -> Value {
        let Some(rev) = self.state.get().map(|g| g.revision) else {
            return GoalError::NoGoal.to_json();
        };
        let patch = GoalPatch {
            objective: Some(objective.to_string()),
            ..Default::default()
        };
        match self.state.update(rev, GoalAction::Edit(patch)) {
            Ok(goal) => {
                self.emit(&goal, "edit");
                goal.compact_json()
            }
            Err(e) => e.to_json(),
        }
    }

    /// `/goal pause`: user pause disarms immediately (no further rounds run).
    pub fn user_pause(&mut self) -> Value {
        let Some(rev) = self.state.get().map(|g| g.revision) else {
            return GoalError::NoGoal.to_json();
        };
        match self.state.update(rev, GoalAction::Pause) {
            Ok(goal) => {
                self.emit(&goal, "pause");
                goal.compact_json()
            }
            Err(e) => e.to_json(),
        }
    }

    /// `/goal resume`: the single explicit re-arm path (active + armed).
    pub fn user_resume(&mut self) -> Value {
        let Some(rev) = self.state.get().map(|g| g.revision) else {
            return GoalError::NoGoal.to_json();
        };
        match self.state.update(rev, GoalAction::Resume) {
            Ok(goal) => {
                self.state.armed = true;
                let goal = self.state.get().cloned().unwrap_or(goal);
                self.emit(&goal, "resume");
                goal.compact_json()
            }
            Err(e) => e.to_json(),
        }
    }

    /// Limit-recovery rearm (limits.zh.md §goal rearm): the auto-continue path re-arms a
    /// disarmed *active* goal in place — no state transition, no goal/change event (arming
    /// is process-local; the caller writes the separate `goal/rearm` audit). Returns the
    /// snapshot when a rearm actually happened (active + disarmed); None = nothing to rearm.
    /// The manual 「立即重试」 path never calls this (goal.zh.md §disarm/rearm).
    pub fn rearm_for_continue(&mut self) -> Option<Goal> {
        let active = matches!(self.state.get().map(|g| g.status), Some(GoalStatus::Active));
        if active && !self.state.armed {
            self.state.armed = true;
            self.state.get().cloned()
        } else {
            None
        }
    }

    /// `/goal clear`: terminate and drop the current goal (event snapshot status: cleared).
    pub fn user_clear(&mut self) -> Value {
        match self.state.clear() {
            Some(gone) => {
                let mut data = gone.event_data("clear");
                data["status"] = json!("cleared");
                self.pending_events.push(data);
                json!({ "ok": true })
            }
            None => GoalError::NoGoal.to_json(),
        }
    }

    /// Status-bar badge text: truncated objective + round N/M + budget awareness + blocked reason.
    pub fn badge(&self, lang: Lang) -> Option<String> {
        let goal = self.state.get()?;
        let obj: String = goal.objective.chars().take(12).collect();
        match goal.status {
            GoalStatus::Active | GoalStatus::Paused => {
                let mut s = match goal.max_goal_rounds {
                    Some(m) => format!("🎯{obj} r{}/{}", goal.rounds_used, m),
                    None => format!("🎯{obj} r{}", goal.rounds_used),
                };
                if let Some(b) = goal.token_budget {
                    s.push_str(&format!(" tok{}/{}", goal.tokens_used, b));
                }
                if goal.status == GoalStatus::Paused {
                    s.push_str(tr(lang, StrKey::BadgePausedSuffix));
                } else if self.state.stop_reason().is_some() {
                    s.push_str(tr(lang, StrKey::BadgeStoppedSuffix));
                }
                Some(s)
            }
            GoalStatus::Complete => {
                Some(format!("🎯{obj}{}", tr(lang, StrKey::BadgeCompleteSuffix)))
            }
            GoalStatus::Blocked => Some(format!(
                "🎯{obj}（blocked:{}）",
                goal.blocked_reason.as_deref().unwrap_or("unspecified")
            )),
        }
    }

    /// Continuation prompt for one goal round (dsh continuation-template lineage).
    /// `round_no` is the round number being driven (1-based among consumed rounds).
    pub fn goal_round_prompt(&self, round_no: u64, warning: Option<&str>) -> Option<String> {
        let goal = self.state.get()?;
        if goal.status != GoalStatus::Active {
            return None;
        }
        let mut p = format!(
            "<goal_round>\n继续推进当前 goal：{}\n（第 {round_no} 个 goal round）\n\
             请继续朝 objective 推进；routine 多步骤工作仍用 plan/Task 工具，不要把 goal 当任务清单。\n\
             若原始 objective 已完整达成（不得以缩水的子集冒充完成），调用 update_goal complete；\n\
             若确认无法达成，调用 update_goal blocked 并给出 reason。",
            goal.objective
        );
        if let Some(w) = warning {
            p.push_str(&format!(
                "\n⚠ 预算告警：{w}。请收敛收尾或主动 complete/blocked。"
            ));
        }
        p.push_str("\n</goal_round>");
        Some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_goal(max: Option<u64>, tok: Option<u64>) -> GoalState {
        let mut s = GoalState::default();
        s.create("发布 v2 版本", max, tok).unwrap();
        s
    }

    // —— CAS ——

    #[test]
    fn cas拒绝过期revision并返回当前状态() {
        let mut s = active_goal(Some(5), None);
        // Simulate someone else editing between the model's get and update.
        s.update(
            1,
            GoalAction::Edit(GoalPatch {
                objective: Some("用户已改".into()),
                ..Default::default()
            }),
        )
        .unwrap();
        let err = s.update(1, GoalAction::Pause).unwrap_err();
        match err {
            GoalError::StaleRevision(g) => {
                assert_eq!(g.revision, 2);
                assert_eq!(g.objective, "用户已改");
            }
            other => panic!("应为 StaleRevision，实际 {other:?}"),
        }
    }

    // —— state machine ——

    #[test]
    fn 状态机非法转换被拒() {
        let mut s = active_goal(None, None);
        assert!(matches!(
            s.update(1, GoalAction::Resume),
            Err(GoalError::InvalidTransition {
                action: "resume",
                from: "active"
            })
        ));
        s.update(1, GoalAction::Pause).unwrap();
        assert!(matches!(
            s.update(2, GoalAction::Pause),
            Err(GoalError::InvalidTransition {
                action: "pause",
                from: "paused"
            })
        ));
        s.update(2, GoalAction::Complete).unwrap();
        assert!(matches!(
            s.update(3, GoalAction::Edit(Default::default())),
            Err(GoalError::InvalidTransition {
                action: "edit",
                from: "complete"
            })
        ));
        assert!(matches!(
            s.update(4, GoalAction::Pause),
            Err(GoalError::StaleRevision(_))
        ));
    }

    #[test]
    fn 终态后槽位释放可再创建() {
        let mut s = active_goal(None, None);
        s.update(1, GoalAction::Complete).unwrap();
        assert!(s.create("新目标", None, None).is_ok());
    }

    #[test]
    fn 活跃时重复创建被拒() {
        let mut s = active_goal(None, None);
        assert!(matches!(
            s.create("另一个", None, None),
            Err(GoalError::AlreadyExists)
        ));
    }

    #[test]
    fn pause后resume回到active且可逆() {
        let mut s = active_goal(None, None);
        s.update(1, GoalAction::Pause).unwrap();
        assert_eq!(s.get().unwrap().status, GoalStatus::Paused);
        s.update(2, GoalAction::Resume).unwrap();
        assert_eq!(s.get().unwrap().status, GoalStatus::Active);
    }

    // —— budgets ——

    #[test]
    fn rounds预算耗尽硬停且不预留新轮() {
        let mut s = active_goal(Some(2), None);
        assert!(s.charge_round());
        assert!(s.charge_round());
        assert!(!s.charge_round(), "预算外不得预留新 Round");
        assert_eq!(s.get().unwrap().rounds_used, 2);
        assert!(s.stop_reason().is_some());
        assert_eq!(
            s.get().unwrap().status,
            GoalStatus::Active,
            "硬停后保持 active-but-stopped"
        );
    }

    #[test]
    fn token预算累计与耗尽() {
        let mut s = active_goal(None, Some(100));
        s.charge_tokens(60);
        s.charge_tokens(40);
        assert!(s.tokens_exhausted());
        assert!(s.stop_reason().is_some());
    }

    #[test]
    fn pause窗口内token不累计() {
        let mut s = active_goal(None, Some(100));
        s.update(1, GoalAction::Pause).unwrap();
        s.charge_tokens(50);
        assert_eq!(
            s.get().unwrap().tokens_used,
            0,
            "暂停窗口排除在生命周期计量外"
        );
        s.update(2, GoalAction::Resume).unwrap();
        s.charge_tokens(10);
        assert_eq!(s.get().unwrap().tokens_used, 10);
    }

    #[test]
    fn 软告警在剩余低于两成时触发() {
        let mut s = active_goal(Some(10), Some(100));
        assert!(s.soft_warning().is_none(), "满额不告警");
        s.charge_tokens(85);
        let w = s.soft_warning().unwrap();
        assert!(w.contains("token"), "token 剩 15% 应告警：{w}");
        // rounds: 9/10 used → 10% remaining
        for _ in 0..9 {
            s.charge_round();
        }
        let w = s.soft_warning().unwrap();
        assert!(
            w.contains("round") && w.contains("token"),
            "双预算同时触发都注入：{w}"
        );
    }

    #[test]
    fn 软告警不打扰耗尽态与不限预算() {
        let mut s = active_goal(None, None);
        for _ in 0..100 {
            s.charge_round();
            s.charge_tokens(1000);
        }
        assert!(s.soft_warning().is_none(), "不限预算永不软告警");
    }

    #[test]
    fn 双预算独立先耗尽者停() {
        // rounds exhausts first while tokens still have room.
        let mut s = active_goal(Some(1), Some(1_000_000));
        assert!(s.charge_round());
        assert!(!s.charge_round());
        assert!(!s.tokens_exhausted(), "token 未耗尽");
        assert!(s.stop_reason().is_some());
        // tokens exhausts first while rounds still have room.
        let mut s = active_goal(Some(1_000_000), Some(5));
        s.charge_tokens(5);
        assert!(s.tokens_exhausted());
        assert!(s.stop_reason().is_some());
        assert_eq!(s.get().unwrap().rounds_used, 0, "rounds 未耗尽");
    }

    // —— no-progress streak ——

    #[test]
    fn 连续三轮无进展强制blocked() {
        let mut s = active_goal(None, None);
        assert!(s.record_round_progress(true).is_none());
        assert!(
            s.record_round_progress(false).is_none(),
            "第 1 轮无进展不触发"
        );
        assert!(
            s.record_round_progress(false).is_none(),
            "第 2 轮无进展不触发"
        );
        let (goal, event) = s.record_round_progress(false).expect("第 3 轮触发");
        assert_eq!(goal.status, GoalStatus::Blocked);
        assert_eq!(
            goal.blocked_reason.as_deref(),
            Some("consecutive-no-progress")
        );
        assert_eq!(event["action"], "blocked");
        assert_eq!(event["blockedReason"], "consecutive-no-progress");
        assert!(!s.is_armed());
    }

    #[test]
    fn 进展重置连续计数() {
        let mut s = active_goal(None, None);
        s.record_round_progress(false);
        s.record_round_progress(false);
        s.record_round_progress(true);
        assert!(s.record_round_progress(false).is_none(), "进展后重新计数");
    }

    // —— driver gating ——

    #[test]
    fn disarm后不驱动() {
        let mut s = active_goal(None, None);
        assert!(s.should_drive());
        s.update(1, GoalAction::Pause).unwrap();
        assert!(!s.should_drive(), "paused 不驱动");
        s.update(2, GoalAction::Resume).unwrap();
        assert!(!s.should_drive(), "模型侧 resume 不 rearm");
    }

    // —— host proof ——

    #[test]
    fn 宿主证明三路拒绝() {
        let mut rt = GoalRuntime::new(None);
        // 1) no accepted user message in the turn
        let v = rt.tool_create("目标", None, None);
        assert_eq!(v["error"], "host_proof_denied");
        // 2) goal round
        rt.begin_turn(true, true);
        let v = rt.tool_create("目标", None, None);
        assert_eq!(v["error"], "goal_round_denied");
        // 3) subagent
        rt.begin_turn(true, false);
        rt.is_subagent = true;
        let v = rt.tool_create("目标", None, None);
        assert_eq!(v["error"], "subagent_denied");
        // happy path: human turn, main context
        rt.begin_turn(true, false);
        rt.is_subagent = false;
        let v = rt.tool_create("目标", None, None);
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn turn内goal工具互斥() {
        let mut rt = GoalRuntime::new(None);
        rt.begin_turn(true, false);
        let _ = rt.tool_create("目标", None, None);
        let v = rt.tool_get();
        assert_eq!(v["goal"]["status"], "active");
        let v = rt.tool_update(1, GoalAction::Pause);
        assert_eq!(
            v["error"], "goal_tool_busy",
            "同 turn 第二次 goal 工具调用被拒"
        );
        rt.begin_turn(true, false);
        let v = rt.tool_update(1, GoalAction::Pause);
        assert_eq!(v["ok"], true, "下一 turn 恢复可用");
    }

    #[test]
    fn complete与blocked须在goal_round内() {
        let mut rt = GoalRuntime::new(None);
        rt.begin_turn(true, false);
        let _ = rt.tool_create("目标", None, None);
        // Cross into the next turn: mutual exclusion is per-turn (one goal tool call per turn).
        rt.begin_turn(true, false);
        let v = rt.tool_update(1, GoalAction::Complete);
        assert_eq!(v["error"], "not_in_goal_round");
        rt.begin_turn(false, true);
        let v = rt.tool_update(1, GoalAction::Complete);
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn rounds省略取部署默认() {
        let mut rt = GoalRuntime::new(Some(7));
        rt.begin_turn(true, false);
        let v = rt.tool_create("目标", None, None);
        assert_eq!(v["goal"]["maxGoalRounds"], 7);
        let _ = rt.user_clear();
        rt.begin_turn(true, false);
        let v = rt.tool_create("目标2", Some(3), None);
        assert_eq!(v["goal"]["maxGoalRounds"], 3, "显式值优先于部署默认");
    }

    // —— events / replay ——

    #[test]
    fn 事件快照携带全字段且armed不进事件() {
        let mut rt = GoalRuntime::new(None);
        rt.begin_turn(true, false);
        let _ = rt.tool_create("发布版本", Some(4), Some(500));
        let events = rt.drain_events();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev["action"], "create");
        assert_eq!(ev["status"], "active");
        assert_eq!(ev["revision"], 1);
        assert_eq!(ev["maxGoalRounds"], 4);
        assert_eq!(ev["tokenBudget"], 500);
        assert!(ev.get("armed").is_none(), "armed 绝不进事件");
    }

    #[test]
    fn replay后disarmed但可见可编辑() {
        let mut rt = GoalRuntime::new(None);
        rt.begin_turn(true, false);
        let _ = rt.tool_create("跨进程目标", Some(9), None);
        let events: Vec<crate::session::Event> = rt
            .drain_events()
            .into_iter()
            .map(|data| crate::session::Event {
                kind: "goal/change".into(),
                seq: 0,
                time: 0,
                data,
                ignorable: false,
            })
            .collect();
        let mut rt2 = GoalRuntime::replay(&events, None);
        assert_eq!(rt2.state.get().unwrap().objective, "跨进程目标");
        assert!(!rt2.state.is_armed(), "进程重启后一律 disarm");
        let v = rt2.user_edit("改目标");
        assert_eq!(v["objective"], "改目标", "disarm 后仍可编辑");
    }

    #[test]
    fn clear事件状态为cleared且槽位清空() {
        let mut rt = GoalRuntime::new(None);
        rt.user_create("待清除");
        let events = rt.drain_events();
        let v = rt.user_clear();
        assert_eq!(v["ok"], true);
        let ev = &rt.drain_events()[0];
        assert_eq!(ev["action"], "clear");
        assert_eq!(ev["status"], "cleared");
        assert!(rt.state.get().is_none());
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn 续行提示词含objective与软告警() {
        let mut rt = GoalRuntime::new(None);
        rt.begin_turn(true, false);
        let _ = rt.tool_create("完成重构", Some(10), Some(100));
        rt.state.charge_tokens(95);
        let p = rt
            .goal_round_prompt(1, rt.state.soft_warning().as_deref())
            .unwrap();
        assert!(p.starts_with("<goal_round>"));
        assert!(p.contains("完成重构"));
        assert!(p.contains("第 1 个 goal round"));
        assert!(p.contains("token"), "软告警注入续行提示词：{p}");
    }

    #[test]
    fn 徽标包含round与blocked原因() {
        let mut rt = GoalRuntime::new(Some(5));
        rt.begin_turn(true, false);
        let _ = rt.tool_create("一个非常长的目标文本会被截断显示", None, Some(1000));
        rt.state.charge_round();
        let badge = rt.badge(Lang::Zh).unwrap();
        assert!(badge.contains("r1/5"), "徽标显示 round N/M：{badge}");
        assert!(badge.contains("tok0/1000"), "设预算时展示预算感知：{badge}");
        rt.state.charge_round();
        rt.state.charge_round();
        let _ = rt.state.record_round_progress(false);
        let _ = rt.state.record_round_progress(false);
        let _ = rt.state.record_round_progress(false);
        let badge = rt.badge(Lang::Zh).unwrap();
        assert!(
            badge.contains("consecutive-no-progress"),
            "blocked 显示原因码：{badge}"
        );
    }

    #[test]
    fn 用户pause立即disarm且resume是唯一rearm路径() {
        let mut rt = GoalRuntime::new(None);
        rt.user_create("目标");
        assert!(rt.state.is_armed(), "创建即 arm");
        rt.user_pause();
        assert!(!rt.state.is_armed());
        rt.user_resume();
        assert!(rt.state.is_armed(), "/goal resume 是显式 rearm");
    }

    #[test]
    fn 限额恢复rearm仅作用于disarmed活跃goal() {
        // Active + armed (fresh create) → nothing to rearm.
        let mut rt = GoalRuntime::new(None);
        rt.begin_turn(true, false);
        let _ = rt.tool_create("目标", None, None);
        assert!(
            rt.rearm_for_continue().is_none(),
            "已 armed 的 goal 不重复 rearm"
        );

        // Active + disarmed (replay lineage) → rearm in place, no goal/change event.
        let events: Vec<crate::session::Event> = rt
            .drain_events()
            .into_iter()
            .map(|data| crate::session::Event {
                kind: "goal/change".into(),
                seq: 0,
                time: 0,
                data,
                ignorable: false,
            })
            .collect();
        let mut disarmed = GoalRuntime::replay(&events, None);
        let g = disarmed
            .rearm_for_continue()
            .expect("disarmed 活跃 goal 应被 rearm");
        assert_eq!(g.objective, "目标");
        assert!(disarmed.state.is_armed());
        assert!(
            disarmed.drain_events().is_empty(),
            "rearm 不产生 goal/change 事件（arming 是进程本地状态）"
        );

        // Paused → never re-armed by the limit path.
        let rev = disarmed.state.get().unwrap().revision;
        let _ = disarmed.state.update(rev, GoalAction::Pause);
        assert!(
            disarmed.rearm_for_continue().is_none(),
            "paused goal 不被限额路径 rearm"
        );

        // No goal → None.
        assert!(GoalRuntime::new(None).rearm_for_continue().is_none());
    }
}
