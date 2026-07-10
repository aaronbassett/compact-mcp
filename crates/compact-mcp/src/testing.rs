//! Test-only client helpers. Never compiled into the shipped binary.
#![cfg(any(test, feature = "testing"))]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::{
        CallToolRequestParams, CancelTaskParams, ClientRequest, GetTaskParams,
        GetTaskPayloadParams, NumberOrString, ProgressNotificationParam, ProgressToken, Request,
        RequestParamsMeta, ServerResult, TaskMetadata, TaskStatus,
    },
    object,
    service::{NotificationContext, RunningService},
};

use crate::server::CompactMcp;

pub async fn connect(dir: &Path) -> RunningService<RoleClient, ()> {
    let ws = compact_mcp_core::Workspace::new(dir).unwrap();
    let (client_t, server_t) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        let _ = CompactMcp::new(ws)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    ().serve(client_t).await.unwrap()
}

/// Task-augmented `tools/call`. Returns the task id.
pub async fn start_compile_task(
    client: &RunningService<RoleClient, ()>,
    path: &str,
    skip_zk: bool,
    ttl_ms: u64,
) -> String {
    let res = client
        .send_request(ClientRequest::CallToolRequest(Request::new(
            CallToolRequestParams::new("compile")
                .with_arguments(object!({ "path": path, "skip_zk": skip_zk }))
                .with_task(TaskMetadata::new().with_ttl(ttl_ms)),
        )))
        .await
        .expect("tools/call with params.task");

    match res {
        ServerResult::CreateTaskResult(c) => c.task.task_id,
        other => panic!("expected CreateTaskResult, got {other:?}"),
    }
}

pub async fn task_status(client: &RunningService<RoleClient, ()>, id: &str) -> TaskStatus {
    let res = client
        .send_request(ClientRequest::GetTaskRequest(Request::new(
            GetTaskParams::new(id),
        )))
        .await
        .expect("tasks/get");
    match res {
        ServerResult::GetTaskResult(r) => r.task.status,
        other => panic!("expected GetTaskResult, got {other:?}"),
    }
}

pub async fn await_terminal(client: &RunningService<RoleClient, ()>, id: &str) -> TaskStatus {
    loop {
        let s = task_status(client, id).await;
        if !matches!(s, TaskStatus::Working | TaskStatus::InputRequired) {
            return s;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// `tasks/result`. `ServerResult` is `#[serde(untagged)]`, so the payload decodes
/// as whichever variant matches the JSON shape first.
pub async fn task_payload(client: &RunningService<RoleClient, ()>, id: &str) -> serde_json::Value {
    let res = client
        .send_request(ClientRequest::GetTaskPayloadRequest(Request::new(
            GetTaskPayloadParams::new(id),
        )))
        .await
        .expect("tasks/result");
    match res {
        ServerResult::CallToolResult(r) => serde_json::to_value(r).unwrap(),
        ServerResult::CustomResult(c) => c.0,
        other => panic!("unexpected task payload: {other:?}"),
    }
}

/// `Ok(status)` on success, `Err(debug string)` when the server refuses.
pub async fn cancel_task(
    client: &RunningService<RoleClient, ()>,
    id: &str,
) -> Result<TaskStatus, String> {
    match client
        .send_request(ClientRequest::CancelTaskRequest(Request::new(
            CancelTaskParams::new(id),
        )))
        .await
    {
        Ok(ServerResult::CancelTaskResult(r)) => Ok(r.task.status),
        Ok(other) => Err(format!("unexpected: {other:?}")),
        Err(e) => Err(format!("{e:?}")),
    }
}

/// A client that does nothing but count `notifications/progress`.
#[derive(Clone, Default)]
pub struct ProgressCounter(pub Arc<AtomicUsize>);

impl ClientHandler for ProgressCounter {
    // KEEP THIS YIELD-FREE (no `.await`). `compile_and_count_progress` relies on
    // it: on the single-threaded test runtime, the server writes its progress
    // notification to the stream BEFORE the tool response, so the client's
    // notification task runs this increment to completion before the response
    // event is dispatched — making `seen >= 1` reliable without waiting. Adding
    // an await here (or switching the test to a multi-thread runtime) could
    // reintroduce a delivery race.
    async fn on_progress(
        &self,
        _params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Run a full build with a `progressToken` attached; return how many progress
/// notifications the server sent.
pub async fn compile_and_count_progress(dir: &Path, path: &str) -> usize {
    let ws = compact_mcp_core::Workspace::new(dir).unwrap();
    let (client_t, server_t) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        let _ = CompactMcp::new(ws)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });

    let counter = ProgressCounter::default();
    let seen = counter.0.clone();
    let client = counter.serve(client_t).await.unwrap();

    // CallToolRequestParams has no `with_meta`; attach the progress token via the
    // mutating RequestParamsMeta accessor.
    let mut params = CallToolRequestParams::new("compile")
        .with_arguments(object!({ "path": path, "skip_zk": false }));
    params.set_progress_token(ProgressToken(NumberOrString::Number(1)));

    let res = client
        .call_tool(params)
        .await
        .expect("the tools/call RPC should round-trip");
    // The RPC succeeding only means the round-trip worked; assert the compile
    // itself did not fail, so a silently-broken build can't pass this test.
    assert_ne!(
        res.is_error,
        Some(true),
        "compile reported an error: {:?}",
        res.content
    );

    client.cancel().await.unwrap();
    seen.load(Ordering::SeqCst)
}
