use compact_mcp::testing::{
    await_terminal, cancel_task, connect, start_compile_task, task_payload,
};
use rmcp::model::TaskStatus;

#[tokio::test]
#[cfg_attr(not(feature = "toolchain-tests"), ignore)]
async fn compile_runs_as_a_task_and_the_result_is_re_fetchable() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        "tests/fixtures/counter.compact",
        dir.path().join("c.compact"),
    )
    .unwrap();
    let client = connect(dir.path()).await;

    let id = start_compile_task(&client, "c.compact", true, 60_000).await;
    assert_eq!(await_terminal(&client, &id).await, TaskStatus::Completed);

    // Non-consuming: rmcp's default handler would 404 on the second fetch.
    let a = task_payload(&client, &id).await;
    let b = task_payload(&client, &id).await;
    assert_eq!(a, b, "tasks/result must not consume the result");

    // The spec MUSTs this on tasks/result — the payload alone names no task.
    assert_eq!(
        a["_meta"]["io.modelcontextprotocol/related-task"]["taskId"],
        id.as_str()
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(feature = "toolchain-tests"), ignore)]
async fn cancelling_a_terminal_task_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        "tests/fixtures/counter.compact",
        dir.path().join("c.compact"),
    )
    .unwrap();
    let client = connect(dir.path()).await;

    let id = start_compile_task(&client, "c.compact", true, 60_000).await;
    await_terminal(&client, &id).await;

    let err = cancel_task(&client, &id).await.unwrap_err();
    assert!(
        err.contains("-32602") || err.to_lowercase().contains("invalid params"),
        "expected invalid-params on cancelling a terminal task, got {err}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(feature = "toolchain-tests"), ignore)]
async fn a_failing_compile_marks_the_task_failed() {
    // Per spec: a CallToolResult with isError:true moves the task to `failed`.
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        "tests/fixtures/broken.compact",
        dir.path().join("b.compact"),
    )
    .unwrap();
    let client = connect(dir.path()).await;

    let id = start_compile_task(&client, "b.compact", true, 60_000).await;
    assert_eq!(await_terminal(&client, &id).await, TaskStatus::Failed);

    let payload = task_payload(&client, &id).await;
    assert!(format!("{payload}").contains("unbound identifier"));

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn an_unknown_task_id_is_invalid_params() {
    let dir = tempfile::tempdir().unwrap();
    let client = connect(dir.path()).await;
    let err = cancel_task(&client, "00000000-0000-0000-0000-000000000000")
        .await
        .unwrap_err();
    assert!(
        err.contains("-32602") || err.to_lowercase().contains("invalid params"),
        "{err}"
    );
    client.cancel().await.unwrap();
}
