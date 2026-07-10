use std::time::{Duration, SystemTime};

use compact_mcp_core::jobs::TaskState;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CancelTaskParams, CancelTaskResult, CreateTaskResult, GetTaskParams,
        GetTaskPayloadParams, GetTaskPayloadResult, GetTaskResult, ListTasksResult,
        PaginatedRequestParams, Task, TaskStatus,
    },
    service::RequestContext,
};

use crate::server::{CompactMcp, TASK_CANCEL};

const POLL_INTERVAL_MS: u64 = 2_000;
const RELATED_TASK_KEY: &str = "io.modelcontextprotocol/related-task";

fn rfc3339(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339()
}

fn to_status(s: TaskState) -> TaskStatus {
    match s {
        TaskState::Working => TaskStatus::Working,
        TaskState::Completed => TaskStatus::Completed,
        TaskState::Failed => TaskStatus::Failed,
        TaskState::Cancelled => TaskStatus::Cancelled,
    }
}

fn to_task(snap: &compact_mcp_core::jobs::TaskSnapshot) -> Task {
    let mut t = Task::new(
        snap.id.clone(),
        to_status(snap.state),
        rfc3339(snap.created_at),
        rfc3339(snap.last_updated_at),
    )
    // Report the ACTUAL ttl, in milliseconds, as the spec requires.
    .with_ttl(snap.ttl.as_millis() as u64)
    .with_poll_interval(POLL_INTERVAL_MS);
    if let Some(m) = &snap.status_message {
        t = t.with_status_message(m.clone());
    }
    t
}

impl CompactMcp {
    /// The five task methods are PROTOCOL methods: their `Err` return is the
    /// JSON-RPC error the client's task API itself consumes, never rendered
    /// opaquely to an agent the way a tool's `McpError` would be. A bad or
    /// already-terminal task id is a request-shape error, so the MCP task spec
    /// surfaces it as `invalid_params` (-32602); anything else is our fault.
    fn task_err(e: compact_mcp_core::CoreError) -> McpError {
        use compact_mcp_core::CoreError;
        match e {
            CoreError::TaskNotFound(_) | CoreError::TaskTerminal(_) => {
                McpError::invalid_params(e.to_string(), None)
            }
            other => McpError::internal_error(other.to_string(), None),
        }
    }
}

impl CompactMcp {
    pub(crate) async fn enqueue_task_impl(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CreateTaskResult, McpError> {
        let requested = request
            .task
            .as_ref()
            .and_then(|t| t.ttl)
            .map(Duration::from_millis);
        let (id, cancel, _actual) = self.tasks.create(requested);

        let this = self.clone();
        let id2 = id.clone();
        tokio::spawn(async move {
            let store = this.tasks.clone();
            store.set_status(&id2, "running");
            let out = TASK_CANCEL
                .scope(cancel, async { this.call_tool(request, context).await })
                .await;

            match out {
                Ok(result) => {
                    // Per spec, a CallToolResult with isError:true => task `failed`.
                    let failed = result.is_error.unwrap_or(false);
                    store.finish(
                        &id2,
                        serde_json::to_value(result).unwrap_or_default(),
                        failed,
                    );
                }
                Err(e) => {
                    let v = serde_json::json!({ "error": e.to_string() });
                    store.finish(&id2, v, true);
                }
            }
        });

        Ok(CreateTaskResult::new(to_task(
            &self.tasks.get(&id).map_err(Self::task_err)?,
        )))
    }
}

impl CompactMcp {
    pub(crate) async fn get_task_info_impl(
        &self,
        request: GetTaskParams,
    ) -> Result<GetTaskResult, McpError> {
        let snap = self.tasks.get(&request.task_id).map_err(Self::task_err)?;
        Ok(GetTaskResult::new(to_task(&snap)))
    }

    /// Blocks until the task reaches a terminal state, per spec. Non-consuming.
    pub(crate) async fn get_task_result_impl(
        &self,
        request: GetTaskPayloadParams,
    ) -> Result<GetTaskPayloadResult, McpError> {
        let mut rx = self
            .tasks
            .subscribe(&request.task_id)
            .map_err(Self::task_err)?;
        while !rx.borrow().is_terminal() {
            if rx.changed().await.is_err() {
                break;
            }
        }
        // Terminal now. A Completed/Failed task has a payload; a Cancelled task
        // has none, so synthesize a minimal object so the related-task _meta can
        // still be attached.
        let mut value = self
            .tasks
            .result(&request.task_id)
            .map_err(Self::task_err)?
            .unwrap_or_else(|| serde_json::json!({ "cancelled": true }));

        // `tasks/result` MUST carry the related-task metadata: the payload alone
        // does not identify which task it belongs to.
        if let Some(obj) = value.as_object_mut() {
            obj.entry("_meta")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .expect("_meta is an object")
                .insert(
                    RELATED_TASK_KEY.to_string(),
                    serde_json::json!({ "taskId": request.task_id }),
                );
        }
        Ok(GetTaskPayloadResult::new(value))
    }

    pub(crate) async fn cancel_task_impl(
        &self,
        request: CancelTaskParams,
    ) -> Result<CancelTaskResult, McpError> {
        let snap = self
            .tasks
            .cancel(&request.task_id)
            .map_err(Self::task_err)?;
        Ok(CancelTaskResult::new(to_task(&snap)))
    }

    pub(crate) async fn list_tasks_impl(
        &self,
        _request: Option<PaginatedRequestParams>,
    ) -> Result<ListTasksResult, McpError> {
        // Advertised on stdio only; HTTP cannot identify requestors (§7.2 of the spec).
        if !self.advertise_task_list {
            return Err(McpError::method_not_found::<rmcp::model::ListTasksMethod>());
        }
        let mut tasks: Vec<Task> = self.tasks.list().iter().map(to_task).collect();
        tasks.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        // `ListTasksResult` is #[non_exhaustive] — no struct literal.
        Ok(ListTasksResult::new(tasks))
    }
}
