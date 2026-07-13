// Proves the `BuildGate` concurrency limit is enforced for `witness_scaffold`
// THROUGH the MCP tool boundary — not just in the core unit test. Before #6 only
// `compile` acquired the gate, so `witness_scaffold` (which runs a full --skip-zk
// compile) spawned `compact` ungated and the configured `max_concurrent_builds`
// was not a real global bound.
//
// Unix-only (drives a shell stub) and behind `testing` (uses the test-only client
// helpers). Run with `--features testing`.
#![cfg(all(unix, feature = "testing"))]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use compact_mcp::testing::connect_with_bin;
use rmcp::model::CallToolRequestParams;
use rmcp::object;

/// How many `compact` subprocesses have been SPAWNED so far. The stub creates one
/// unique marker per invocation and never removes it, so this count is monotonic
/// and equals the number of processes that reached the run point — exactly the
/// quantity the gate is meant to bound.
fn spawned_count(started: &Path) -> usize {
    std::fs::read_dir(started).map(|rd| rd.count()).unwrap_or(0)
}

/// Poll until at least `n` subprocesses have spawned, or give up after ~5s.
async fn wait_for_spawned(started: &Path, n: usize) -> bool {
    for _ in 0..200 {
        if spawned_count(started) >= n {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    spawned_count(started) >= n
}

#[tokio::test]
async fn witness_scaffold_serialises_through_the_build_gate() {
    let dir = tempfile::tempdir().unwrap();
    let started = dir.path().join("started");
    std::fs::create_dir(&started).unwrap();
    let go = dir.path().join("go");

    // A stub `compact`: on entry it drops a unique marker (so the marker set size
    // == subprocesses spawned), then blocks until the test creates the `go` file,
    // then exits 0. The block lets us hold one build "in flight" and observe
    // whether a second one is allowed to spawn concurrently.
    let stub = dir.path().join("fake-compact.sh");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\nmktemp \"{started}/run.XXXXXXXX\" >/dev/null 2>&1\n\
             while [ ! -f \"{go}\" ]; do sleep 0.02; done\nexit 0\n",
            started = started.display(),
            go = go.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

    // `connect_with_bin` builds the server with the DEFAULT `BuildGate` —
    // max_concurrent = 1 — so two builds must serialise.
    let client = connect_with_bin(dir.path(), &stub).await;

    // Fire call #1 and wait until its subprocess is actually running (holding the
    // one permit, parked in the stub's go-loop). rmcp spawns a task per request,
    // so these `call_tool`s are genuinely concurrent in-flight requests.
    let peer1 = client.peer().clone();
    let c1 = tokio::spawn(async move {
        peer1
            .call_tool(
                CallToolRequestParams::new("witness_scaffold")
                    .with_arguments(object!({ "source": "ledger x: Counter;" })),
            )
            .await
    });
    assert!(
        wait_for_spawned(&started, 1).await,
        "call #1's compact subprocess never started"
    );

    // Fire call #2. Its handler runs concurrently and reaches the gate, but the
    // single permit is held by #1, so its subprocess MUST NOT spawn. Give it ample
    // time to misbehave: the pre-#6 ungated path would spawn immediately and push
    // the marker count to 2.
    let peer2 = client.peer().clone();
    let c2 = tokio::spawn(async move {
        peer2
            .call_tool(
                CallToolRequestParams::new("witness_scaffold")
                    .with_arguments(object!({ "source": "ledger y: Counter;" })),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        spawned_count(&started),
        1,
        "a second witness_scaffold spawned a concurrent `compact` while the first \
         still held the only build permit — the gate did not serialise it"
    );

    // Release #1. Its permit frees; #2 now acquires it and spawns its subprocess.
    // Reaching 2 here proves #2 genuinely ran (the earlier assertion is not
    // vacuous) — just strictly after #1, exactly as a global bound requires.
    std::fs::write(&go, b"").unwrap();
    assert!(
        wait_for_spawned(&started, 2).await,
        "call #2 never ran after #1 released the permit — the gate stalled rather \
         than queued"
    );

    // Both RPCs round-trip. The scaffold itself reports an error (the stub emits
    // no contract-info.json), but that is an isError CallToolResult, not an RPC
    // failure — we only assert the boundary served both concurrent calls.
    c1.await.unwrap().expect("call #1 RPC round-trip");
    c2.await.unwrap().expect("call #2 RPC round-trip");

    client.cancel().await.unwrap();
}
