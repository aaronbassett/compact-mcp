use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::CoreError;

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
        let _ = r.state_tx.send(r.state);
    }

    pub fn get(&self, id: &str) -> Result<TaskSnapshot, CoreError> {
        self.inner
            .lock()
            .unwrap()
            .get(id)
            .map(|r| r.snapshot(id))
            .ok_or_else(|| CoreError::TaskNotFound(id.to_string()))
    }

    /// Terminal tasks only. Non-consuming: call it as often as you like.
    pub fn result(&self, id: &str) -> Result<serde_json::Value, CoreError> {
        let g = self.inner.lock().unwrap();
        let r = g
            .get(id)
            .ok_or_else(|| CoreError::TaskNotFound(id.to_string()))?;
        r.result
            .clone()
            .ok_or_else(|| CoreError::TaskNotFound(format!("{id} has no result yet")))
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
        let _ = r.state_tx.send(TaskState::Cancelled);
        Ok(r.snapshot(id))
    }

    pub fn list(&self) -> Vec<TaskSnapshot> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(id, r)| r.snapshot(id))
            .collect()
    }

    /// Evict tasks whose retention window has closed. `now` is injected so this
    /// is deterministic under test.
    pub fn gc(&self, now: SystemTime) -> usize {
        let mut g = self.inner.lock().unwrap();
        let before = g.len();
        g.retain(|_, r| {
            now.duration_since(r.created_at)
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

        assert_eq!(s.result(&id).unwrap()["ok"], true);
        assert_eq!(
            s.result(&id).unwrap()["ok"],
            true,
            "second fetch must still work"
        );
    }

    #[test]
    fn result_before_terminal_is_an_error() {
        let s = store();
        let (id, _, _) = s.create(None);
        // Name the exact variant: a trailing `| Err(_)` would assert nothing.
        assert!(matches!(s.result(&id), Err(CoreError::TaskNotFound(_))));
        assert_eq!(s.get(&id).unwrap().state, TaskState::Working);
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
    fn task_ids_are_unguessable() {
        // No auth context exists on HTTP, so the id is the only secret.
        let s = store();
        let (a, _, _) = s.create(None);
        let (b, _, _) = s.create(None);
        assert_ne!(a, b);
        assert!(a.len() >= 32, "expected a uuid-length id, got {a:?}");
    }
}
