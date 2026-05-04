//! M6.29: `UpdateGoal` tool — model-callable hook to mark the active
//! `/goal` complete or blocked, or to record an audit summary partway
//! through. Mutates the global goal state via `crate::goal_state::apply`
//! which fires the broadcaster (worker subscribes → persists snapshot
//! to session JSONL + auto-stops the loop on terminal status).
//!
//! Approval: not required. The state mutation is small and auditable;
//! the worker validates that a goal is actually active before allowing
//! the call to take effect. If no goal is active the call is a no-op
//! that surfaces an explanatory message — same shape as `KmsRead`
//! against an unknown KMS.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct UpdateGoalTool;

#[async_trait]
impl Tool for UpdateGoalTool {
    fn name(&self) -> &'static str {
        "UpdateGoal"
    }

    fn description(&self) -> &'static str {
        "Update the active /goal state. Status `complete` ends the loop \
         (call ONLY after running the completion audit and verifying \
         every requirement). Status `blocked` pauses the loop and asks \
         the user for input. Status `progress` records an audit summary \
         without ending the loop. The audit summary is carried into \
         future iterations so they don't re-audit from scratch."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["complete", "blocked", "progress"],
                    "description": "complete = goal achieved (loop exits); blocked = need user input (loop pauses); progress = checkpoint update (loop continues)"
                },
                "audit": {
                    "type": "string",
                    "description": "Short summary of the audit: what was checked, what evidence was found. Carried into future iterations as the prior_audit hint."
                },
                "reason": {
                    "type": "string",
                    "description": "For status=blocked or status=complete: short explanation surfaced to the user."
                }
            },
            "required": ["status"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        // Not approval-gated — it mutates ephemeral session state, not
        // disk. The worker still validates before the broadcaster fires.
        false
    }

    async fn call(&self, input: Value) -> Result<String> {
        let status_str = req_str(&input, "status")?;
        let audit = input
            .get("audit")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        let reason = input
            .get("reason")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        let new_status = match status_str {
            "complete" => crate::goal_state::GoalStatus::Complete,
            "blocked" => crate::goal_state::GoalStatus::Blocked,
            "progress" => crate::goal_state::GoalStatus::Active,
            other => {
                return Err(Error::Tool(format!(
                    "invalid status '{other}' — must be complete | blocked | progress"
                )))
            }
        };

        // Validate a goal is actually active before mutating.
        if crate::goal_state::current().is_none() {
            return Err(Error::Tool(
                "no active goal — call /goal start first".into(),
            ));
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let changed = crate::goal_state::apply(|g| {
            g.status = new_status;
            if let Some(a) = &audit {
                g.last_audit = Some(a.clone());
            }
            if let Some(r) = &reason {
                g.last_message = Some(r.clone());
            }
            if new_status != crate::goal_state::GoalStatus::Active {
                g.completed_at = Some(now);
            }
            true
        });
        if !changed {
            return Err(Error::Tool("goal state apply failed".into()));
        }

        Ok(format!(
            "goal {} ({}{})",
            status_str,
            if audit.is_some() {
                "audit recorded"
            } else {
                "no audit"
            },
            if reason.is_some() {
                ", reason recorded"
            } else {
                ""
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal_state::{self, GoalState, GoalStatus};

    /// Tests serialize on the global goal state — share a mutex.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset() {
        goal_state::set(None);
    }

    #[tokio::test]
    async fn update_complete_marks_terminal() {
        let _g = lock();
        reset();
        goal_state::set(Some(GoalState::new("ship X".into(), None, None)));

        let result = UpdateGoalTool
            .call(json!({"status": "complete", "audit": "all tests green; spec items checked"}))
            .await
            .unwrap();
        assert!(result.contains("complete"));
        let g = goal_state::current().unwrap();
        assert_eq!(g.status, GoalStatus::Complete);
        assert!(g.completed_at.is_some());
        assert_eq!(
            g.last_audit.as_deref(),
            Some("all tests green; spec items checked")
        );
        reset();
    }

    #[tokio::test]
    async fn update_blocked_records_reason() {
        let _g = lock();
        reset();
        goal_state::set(Some(GoalState::new("ship X".into(), None, None)));

        UpdateGoalTool
            .call(json!({"status": "blocked", "reason": "need API key"}))
            .await
            .unwrap();
        let g = goal_state::current().unwrap();
        assert_eq!(g.status, GoalStatus::Blocked);
        assert_eq!(g.last_message.as_deref(), Some("need API key"));
        reset();
    }

    #[tokio::test]
    async fn update_progress_keeps_active_but_records_audit() {
        let _g = lock();
        reset();
        goal_state::set(Some(GoalState::new("ship X".into(), None, None)));

        UpdateGoalTool
            .call(json!({"status": "progress", "audit": "halfway: parser done, type-checker pending"}))
            .await
            .unwrap();
        let g = goal_state::current().unwrap();
        assert_eq!(g.status, GoalStatus::Active);
        assert_eq!(
            g.last_audit.as_deref(),
            Some("halfway: parser done, type-checker pending")
        );
        // Active status leaves completed_at None.
        assert!(g.completed_at.is_none());
        reset();
    }

    #[tokio::test]
    async fn update_with_no_active_goal_errors() {
        let _g = lock();
        reset();
        let err = UpdateGoalTool
            .call(json!({"status": "complete"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("no active goal"));
    }

    #[tokio::test]
    async fn update_with_invalid_status_errors() {
        let _g = lock();
        reset();
        goal_state::set(Some(GoalState::new("x".into(), None, None)));
        let err = UpdateGoalTool
            .call(json!({"status": "wat"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("invalid status"));
        reset();
    }

    #[test]
    fn does_not_require_approval() {
        assert!(!UpdateGoalTool.requires_approval(&json!({})));
    }
}
