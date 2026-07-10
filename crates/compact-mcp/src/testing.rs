//! Test-only client helpers. Never compiled into the shipped binary.
#![cfg(any(test, feature = "testing"))]

use std::path::Path;

use rmcp::{
    RoleClient, ServiceExt,
    model::{
        CallToolRequestParams, CancelTaskParams, ClientRequest, GetTaskParams,
        GetTaskPayloadParams, Request, ServerResult, TaskMetadata, TaskStatus,
    },
    object,
    service::RunningService,
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
