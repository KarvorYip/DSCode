//! Goal model-side tools (goal.zh.md §模型侧工具): get_goal / create_goal / update_goal.
//! Thin shells over `GoalRuntime` — the domain (host proof, mutual exclusion, CAS, state
//! machine, budget accounting) lives in `crate::goal`; the tools only parse arguments and
//! render compact-JSON results. Registered only when the goal stack is mounted (TUI +
//! goal.enabled; headless never mounts them), so the schema-level gate is the registry
//! assembly in `Registry::with_goal`.

use crate::goal::{GoalAction, GoalPatch, GoalRuntime};
use parking_lot::Mutex;
use serde_json::Value;
use std::sync::Arc;

/// Lock the shared runtime (parking_lot: no poisoning path).
fn with_runtime<R>(rt: &Arc<Mutex<GoalRuntime>>, f: impl FnOnce(&mut GoalRuntime) -> R) -> R {
    f(&mut rt.lock())
}

pub struct GetGoalTool(pub Arc<Mutex<GoalRuntime>>);

#[async_trait::async_trait]
impl super::Tool for GetGoalTool {
    fn name(&self) -> &str {
        "get_goal"
    }

    fn description(&self) -> &str {
        "查询当前 goal（跨 turn 的完成承诺）状态快照。goal 仅用于需要多轮推进的长任务；\
         例行多步骤工作请用 plan/Task，不要为短任务创建 goal"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }

    fn tier(&self) -> super::Tier {
        super::Tier::Write
    }

    async fn execute(&self, _arguments: &Value) -> super::ToolOutput {
        super::ToolOutput {
            output: serde_json::to_string(&with_runtime(&self.0, |rt| rt.tool_get())).unwrap(),
            exit_code: None,
        }
    }
}

pub struct CreateGoalTool(pub Arc<Mutex<GoalRuntime>>);

#[async_trait::async_trait]
impl super::Tool for CreateGoalTool {
    fn name(&self) -> &str {
        "create_goal"
    }

    fn description(&self) -> &str {
        "创建当前会话的唯一 goal（跨 turn 完成承诺，续行驱动器将自动推进轮次，受 rounds/token \
         双预算封顶）。仅当本 turn 含已接受的人类消息时可用；goal round 与 subagent 内不可。\
         仅长任务才应设置 goal；complete 必须指原始 objective，不得以缩水子集冒充完成"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "objective": { "type": "string", "description": "跨多轮持续推进的目标" },
                "max_goal_rounds": { "type": "integer", "description": "rounds 预算上限；省略取部署默认，null 不限" },
                "token_budget": { "type": "integer", "description": "goal 生命周期内累计 token 预算；null 不限" }
            },
            "required": ["objective"]
        })
    }

    fn tier(&self) -> super::Tier {
        super::Tier::Write
    }

    async fn execute(&self, arguments: &Value) -> super::ToolOutput {
        let objective = arguments
            .get("objective")
            .and_then(Value::as_str)
            .unwrap_or("");
        let max = arguments.get("max_goal_rounds").and_then(Value::as_u64);
        let budget = arguments.get("token_budget").and_then(Value::as_u64);
        let v = with_runtime(&self.0, |rt| rt.tool_create(objective, max, budget));
        super::ToolOutput {
            output: serde_json::to_string(&v).unwrap(),
            exit_code: None,
        }
    }
}

pub struct UpdateGoalTool(pub Arc<Mutex<GoalRuntime>>);

#[async_trait::async_trait]
impl super::Tool for UpdateGoalTool {
    fn name(&self) -> &str {
        "update_goal"
    }

    fn description(&self) -> &str {
        "更新当前 goal：edit（改 objective/预算）/ pause / resume / complete / blocked。\
         携带当前 revision（CAS：过期即拒并返回当前状态）；complete/blocked 必须在当前 \
         goal round 内调用"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "revision": { "type": "integer", "description": "get_goal 返回的当前 revision" },
                "action": { "type": "string", "enum": ["edit", "pause", "resume", "complete", "blocked"] },
                "objective": { "type": "string", "description": "edit：新 objective" },
                "max_goal_rounds": { "type": "integer", "description": "edit：新 rounds 预算" },
                "token_budget": { "type": "integer", "description": "edit：新 token 预算" },
                "reason": { "type": "string", "description": "blocked：原因码" }
            },
            "required": ["revision", "action"]
        })
    }

    fn tier(&self) -> super::Tier {
        super::Tier::Write
    }

    async fn execute(&self, arguments: &Value) -> super::ToolOutput {
        let Some(revision) = arguments.get("revision").and_then(Value::as_u64) else {
            return super::ToolOutput {
                output: r#"{"ok":false,"error":"missing_revision","message":"缺少 revision 参数"}"#
                    .into(),
                exit_code: None,
            };
        };
        let action = match arguments.get("action").and_then(Value::as_str).unwrap_or("") {
            "edit" => GoalAction::Edit(GoalPatch {
                objective: arguments.get("objective").and_then(Value::as_str).map(str::to_string),
                max_goal_rounds: Some(arguments.get("max_goal_rounds").and_then(Value::as_u64)),
                token_budget: Some(arguments.get("token_budget").and_then(Value::as_u64)),
            }),
            "pause" => GoalAction::Pause,
            "resume" => GoalAction::Resume,
            "complete" => GoalAction::Complete,
            "blocked" => GoalAction::Blocked {
                reason: arguments.get("reason").and_then(Value::as_str).map(str::to_string),
            },
            _ => {
                return super::ToolOutput {
                    output: r#"{"ok":false,"error":"bad_action","message":"action 必须是 edit/pause/resume/complete/blocked"}"#.into(),
                    exit_code: None,
                }
            }
        };
        let v = with_runtime(&self.0, |rt| rt.tool_update(revision, action));
        super::ToolOutput {
            output: serde_json::to_string(&v).unwrap(),
            exit_code: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{Registry, Tool};

    fn rt() -> Arc<Mutex<GoalRuntime>> {
        let mut g = GoalRuntime::new(None);
        g.begin_turn(true, false);
        Arc::new(Mutex::new(g))
    }

    #[tokio::test]
    async fn get_goal无goal时返回null() {
        let t = GetGoalTool(rt());
        let out = t.execute(&serde_json::json!({})).await;
        assert_eq!(out.output, r#"{"goal":null}"#);
    }

    #[tokio::test]
    async fn create输出紧凑json且事件入队() {
        let handle = rt();
        let t = CreateGoalTool(handle.clone());
        let out = t
            .execute(&serde_json::json!({ "objective": "重构发布", "max_goal_rounds": 3 }))
            .await;
        assert!(
            out.output.contains(r#""ok":true"#),
            "紧凑 JSON：{}",
            out.output
        );
        assert!(
            out.output.contains(r#""maxGoalRounds":3"#),
            "{}",
            out.output
        );
        let queued = handle.lock().drain_events();
        assert_eq!(queued.len(), 1, "goal/change 事件随变更入队待 flush");
    }

    #[tokio::test]
    async fn update走cas拒过期() {
        let handle = rt();
        CreateGoalTool(handle.clone())
            .execute(&serde_json::json!({ "objective": "目标" }))
            .await;
        // Cross into the next turn: mutual exclusion is per-turn (one goal tool call per turn).
        handle.lock().begin_turn(true, false);
        let t = UpdateGoalTool(handle.clone());
        let out = t
            .execute(&serde_json::json!({ "revision": 99, "action": "pause" }))
            .await;
        assert!(out.output.contains("stale_revision"), "{}", out.output);
        assert!(
            out.output.contains(r#""revision":1"#),
            "拒绝时返回当前状态：{}",
            out.output
        );
        // same-turn mutual exclusion: a second goal call in this turn is locked out
        let out = t
            .execute(&serde_json::json!({ "revision": 1, "action": "pause" }))
            .await;
        assert!(out.output.contains("goal_tool_busy"), "{}", out.output);
    }

    #[test]
    fn headless无goal工具_tui挂载有三件() {
        // The schema-level gate: builtin() (headless / goal.enabled=false) never exposes
        // goal tools; with_goal mounts all three.
        let bare = Registry::builtin();
        for name in ["get_goal", "create_goal", "update_goal"] {
            assert!(bare.get(name).is_none(), "builtin 不应含 {name}");
        }
        assert!(!bare
            .definitions()
            .iter()
            .any(|d| d["function"]["name"].as_str().unwrap().contains("goal")));

        let full = Registry::builtin().and_goal(rt());
        for name in ["get_goal", "create_goal", "update_goal"] {
            assert!(full.get(name).is_some(), "with_goal 应含 {name}");
        }
        let defs = full.definitions();
        let names: Vec<&str> = defs
            .iter()
            .map(|d| d["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"bash"), "原有六工具保留");
    }
}
