use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::CoreError;

pub mod gate;
pub use gate::BuildGate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Working,
    Completed,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, TaskState::Working)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskSnapshot {
    pub id: String,
    pub state: TaskState,
    pub status_message: Option<String>,
    pub created_at: SystemTime,
    pub last_updated_at: SystemTime,
    pub ttl: Duration,
}

struct Record {
    state: TaskState,
    status_message: Option<String>,
    created_at: SystemTime,
    last_updated_at: SystemTime,
    ttl: Duration,
    /// Retained until TTL expiry. Reading it does NOT remove it.
    result: Option<serde_json::Value>,
    cancel: CancellationToken,
    state_tx: watch::Sender<TaskState>,
}

impl Record {
    fn snapshot(&self, id: &str) -> TaskSnapshot {
        TaskSnapshot {
            id: id.to_string(),
            state: self.state,
            status_message: self.status_message.clone(),
            created_at: self.created_at,
            last_updated_at: self.last_updated_at,
            ttl: self.ttl,
        }
    }
}

/// In-memory task registry. `ttl` is a RETENTION window (how long a finished
/// task's result is kept), not an execution timeout — those are separate knobs.
pub struct TaskStore {
    inner: Mutex<HashMap<String, Record>>,
    default_ttl: Duration,
    max_ttl: Duration,
}

impl TaskStore {
    pub fn new(default_ttl: Duration, max_ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            default_ttl,
            max_ttl,
        }
    }

    fn clamp(&self, requested: Option<Duration>) -> Duration {
        requested.unwrap_or(self.default_ttl).min(self.max_ttl)
    }

    /// Returns the id, the token the running future must observe, and the
    /// ACTUAL ttl — which the caller MUST report back to the client.
    pub fn create(&self, requested_ttl: Option<Duration>) -> (String, CancellationToken, Duration) {
        let id = uuid::Uuid::new_v4().to_string();
        let ttl = self.clamp(requested_ttl);
        let cancel = CancellationToken::new();
        let (state_tx, _) = watch::channel(TaskState::Working);
        let now = SystemTime::now();

        self.inner.lock().unwrap().insert(
            id.clone(),
            Record {
                state: TaskState::Working,
                status_message: Some("queued".to_string()),
                created_at: now,
                last_updated_at: now,
                ttl,
                result: None,
                cancel: cancel.clone(),
                state_tx,
            },
        );
        (id, cancel, ttl)
    }

    pub fn set_status(&self, id: &str, msg: &str) {
        if let Some(r) = self.inner.lock().unwrap().get_mut(id) {
            // A late progress heartbeat must not clobber a terminal task's
            // final status or bump its retention clock (which now runs from
            // `last_updated_at`). Mirror the terminal guard finish/cancel use.
            if r.state.is_terminal() {
                return;
            }
            r.status_message = Some(msg.to_string());
            r.last_updated_at = SystemTime::now();
        }
    }

    pub fn finish(&self, id: &str, result: serde_json::Value, failed: bool) {
        let mut g = self.inner.lock().unwrap();
        let Some(r) = g.get_mut(id) else { return };
        if r.state.is_terminal() {
            return;
        }
        r.state = if failed {
            TaskState::Failed
        } else {
            TaskState::Completed
        };
        r.status_message = Some(if failed { "failed" } else { "completed" }.to_string());
        r.last_updated_at = SystemTime::now();
        r.result = Some(result);
        // `send_replace`, not `send`: `send` fails (and leaves the stored value
        // untouched) when no receiver is currently subscribed, which would leave
        // a client that subscribes AFTER completion reading a stale `Working`.
        // `send_replace` updates the value unconditionally and still wakes any
        // active receivers.
        r.state_tx.send_replace(r.state);
    }

    pub fn get(&self, id: &str) -> Result<TaskSnapshot, CoreError> {
        self.inner
            .lock()
            .unwrap()
            .get(id)
            .map(|r| r.snapshot(id))
            .ok_or_else(|| CoreError::TaskNotFound(id.to_string()))
    }

    /// The result payload, non-consuming (call it as often as you like).
    /// `Err(TaskNotFound)` means the id is unknown — a hard error. `Ok(None)`
    /// means the task EXISTS but carries no result payload: it is still working,
    /// or it was cancelled. Distinguishing those two is the caller's job via
    /// [`get`](Self::get) — folding "not finished" into `TaskNotFound` would
    /// mislead a caller that needs to tell an unknown id from a pending one.
    pub fn result(&self, id: &str) -> Result<Option<serde_json::Value>, CoreError> {
        let g = self.inner.lock().unwrap();
        let r = g
            .get(id)
            .ok_or_else(|| CoreError::TaskNotFound(id.to_string()))?;
        Ok(r.result.clone())
    }

    pub fn subscribe(&self, id: &str) -> Result<watch::Receiver<TaskState>, CoreError> {
        self.inner
            .lock()
            .unwrap()
            .get(id)
            .map(|r| r.state_tx.subscribe())
            .ok_or_else(|| CoreError::TaskNotFound(id.to_string()))
    }

    pub fn cancel(&self, id: &str) -> Result<TaskSnapshot, CoreError> {
        let mut g = self.inner.lock().unwrap();
        let r = g
            .get_mut(id)
            .ok_or_else(|| CoreError::TaskNotFound(id.to_string()))?;
        if r.state.is_terminal() {
            return Err(CoreError::TaskTerminal(id.to_string()));
        }
        r.cancel.cancel();
        r.state = TaskState::Cancelled;
        r.status_message = Some("cancelled".to_string());
        r.last_updated_at = SystemTime::now();
        // See `finish`: `send_replace` so a late subscriber sees the terminal
        // value even when no receiver was active at cancel time.
        r.state_tx.send_replace(TaskState::Cancelled);
        Ok(r.snapshot(id))
    }

    /// Every task in the store. This has NO per-caller scoping, so it is only
    /// exposed on the single-trusted-client stdio transport — the streamable
    /// HTTP transport (where the task id is the only secret) deliberately omits
    /// `tasks/list` rather than let an unauthenticated caller enumerate ids.
    pub fn list(&self) -> Vec<TaskSnapshot> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(id, r)| r.snapshot(id))
            .collect()
    }

    /// Evict TERMINAL tasks whose retention window (measured from the terminal
    /// transition) has closed. A still-`Working` task is never evicted by age:
    /// that would orphan its running future and drop its cancel token while it
    /// keeps consuming a build slot — capping a runaway task's lifetime is a
    /// distinct concern that must transition it to a terminal state first, not a
    /// silent map eviction. `now` is injected so this is deterministic in tests.
    pub fn gc(&self, now: SystemTime) -> usize {
        let mut g = self.inner.lock().unwrap();
        let before = g.len();
        g.retain(|_, r| {
            !r.state.is_terminal()
                || now
                    .duration_since(r.last_updated_at)
                    .map(|age| age < r.ttl)
                    .unwrap_or(true)
        });
        before - g.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> TaskStore {
        TaskStore::new(Duration::from_secs(900), Duration::from_secs(3600))
    }

    #[test]
    fn ttl_is_clamped_and_defaulted() {
        let s = store();
        let (_, _, a) = s.create(None);
        assert_eq!(a, Duration::from_secs(900), "omitted ttl uses the default");

        let (_, _, b) = s.create(Some(Duration::from_secs(60)));
        assert_eq!(b, Duration::from_secs(60), "a reasonable ttl is honoured");

        let (_, _, c) = s.create(Some(Duration::from_secs(999_999)));
        assert_eq!(
            c,
            Duration::from_secs(3600),
            "an excessive ttl is clamped to max"
        );
    }

    #[test]
    fn result_is_not_consumed_by_reading_it() {
        // rmcp's default `take_completed_result` removes it; the spec allows re-fetch.
        let s = store();
        let (id, _, _) = s.create(None);
        s.finish(&id, json!({"ok": true}), false);

        assert_eq!(s.result(&id).unwrap().unwrap()["ok"], true);
        assert_eq!(
            s.result(&id).unwrap().unwrap()["ok"],
            true,
            "second fetch must still work"
        );
    }

    #[test]
    fn result_distinguishes_unknown_id_from_pending() {
        let s = store();
        let (id, _, _) = s.create(None);
        // A KNOWN but still-working task has no result payload -> Ok(None),
        // NOT the same error an unknown id gets. The caller must be able to tell
        // "no such task" from "not done yet".
        assert!(s.result(&id).unwrap().is_none());
        assert_eq!(s.get(&id).unwrap().state, TaskState::Working);
        assert!(matches!(s.result("nope"), Err(CoreError::TaskNotFound(_))));
    }

    #[test]
    fn a_failed_call_tool_result_marks_the_task_failed() {
        let s = store();
        let (id, _, _) = s.create(None);
        s.finish(&id, json!({"isError": true}), true);
        assert_eq!(s.get(&id).unwrap().state, TaskState::Failed);
    }

    #[test]
    fn cancel_signals_the_token_and_is_idempotent_only_once() {
        let s = store();
        let (id, ct, _) = s.create(None);
        assert!(!ct.is_cancelled());

        let snap = s.cancel(&id).unwrap();
        assert_eq!(snap.state, TaskState::Cancelled);
        assert!(ct.is_cancelled(), "cancel must reach the running future");

        let err = s.cancel(&id).unwrap_err();
        assert!(matches!(err, CoreError::TaskTerminal(_)));
    }

    #[test]
    fn cancelling_a_completed_task_is_an_error() {
        let s = store();
        let (id, _, _) = s.create(None);
        s.finish(&id, json!({}), false);
        assert!(matches!(s.cancel(&id), Err(CoreError::TaskTerminal(_))));
    }

    #[test]
    fn unknown_ids_are_reported_not_panicked() {
        let s = store();
        assert!(matches!(s.get("nope"), Err(CoreError::TaskNotFound(_))));
        assert!(matches!(s.cancel("nope"), Err(CoreError::TaskNotFound(_))));
    }

    #[test]
    fn gc_evicts_only_tasks_past_their_ttl() {
        let s = store();
        let (short, _, _) = s.create(Some(Duration::from_secs(1)));
        let (long, _, _) = s.create(Some(Duration::from_secs(3000)));
        s.finish(&short, json!({}), false);
        s.finish(&long, json!({}), false);

        let later = SystemTime::now() + Duration::from_secs(10);
        assert_eq!(s.gc(later), 1);
        assert!(matches!(s.get(&short), Err(CoreError::TaskNotFound(_))));
        assert!(s.get(&long).is_ok());
    }

    #[tokio::test]
    async fn subscribe_wakes_on_the_terminal_transition() {
        let s = store();
        let (id, _, _) = s.create(None);
        let mut rx = s.subscribe(&id).unwrap();
        assert!(!rx.borrow().is_terminal());

        s.finish(&id, json!({"done": 1}), false);
        rx.changed().await.unwrap();
        assert!(rx.borrow().is_terminal());
    }

    #[test]
    fn subscribe_after_terminal_sees_the_terminal_value() {
        // A subscriber that arrives AFTER the task finished still reads the
        // terminal state via `borrow()`, without needing a `changed()` wake.
        let s = store();
        let (id, _, _) = s.create(None);
        s.finish(&id, json!({}), false);
        let rx = s.subscribe(&id).unwrap();
        assert!(rx.borrow().is_terminal());
        assert_eq!(*rx.borrow(), TaskState::Completed);
    }

    #[test]
    fn gc_never_evicts_a_working_task_even_past_its_ttl() {
        // A still-Working task must survive gc regardless of age: evicting it
        // would orphan its running future and drop its cancel token. Retention
        // applies only to FINISHED results.
        let s = store();
        let (id, _, _) = s.create(Some(Duration::from_secs(1)));
        let later = SystemTime::now() + Duration::from_secs(10);
        assert_eq!(s.gc(later), 0, "a Working task is never evicted by age");
        assert_eq!(s.get(&id).unwrap().state, TaskState::Working);
    }

    #[test]
    fn retention_runs_from_finish_not_creation() {
        // A task that spent 90s queued/running then finished must keep its full
        // TTL window from the FINISH point. Backdate created_at to simulate the
        // long run; if gc keyed off created_at (age 110s > 100s ttl) it would
        // wrongly evict, but keyed off last_updated_at (age ~20s) it is kept.
        let s = store();
        let (id, _, _) = s.create(Some(Duration::from_secs(100)));
        s.inner.lock().unwrap().get_mut(&id).unwrap().created_at =
            SystemTime::now() - Duration::from_secs(90);
        s.finish(&id, json!({}), false); // last_updated_at = now
        let later = SystemTime::now() + Duration::from_secs(20);
        assert_eq!(
            s.gc(later),
            0,
            "retention must run from finish, not creation"
        );
        assert!(s.get(&id).is_ok());
    }

    #[test]
    fn task_ids_are_unguessable() {
        // No auth context exists on HTTP, so the id is the only secret.
        let s = store();
        let (a, _, _) = s.create(None);
        let (b, _, _) = s.create(None);
        assert_ne!(a, b);
        assert!(a.len() >= 32, "expected a uuid-length id, got {a:?}");
    }
}
