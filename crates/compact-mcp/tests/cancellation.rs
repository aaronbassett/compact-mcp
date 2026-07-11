#![cfg(all(unix, feature = "testing"))]

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use compact_mcp::testing::{cancel_task, connect_with_bin, start_compile_task, task_status};
use rmcp::model::TaskStatus;

/// `kill -0` succeeds iff the process exists.
fn is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Stands in for `compact`, which execs `compactc.bin`. It backgrounds a
/// grandchild, records its pid, and waits — so killing only the direct child
/// would leave `sleep` orphaned, exactly as a real build would leave `compactc`.
fn write_stub(dir: &std::path::Path, pidfile: &std::path::Path) -> std::path::PathBuf {
    let stub = dir.join("fake-compact.sh");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\nsleep 30 &\necho $! > {}\nwait\n",
            pidfile.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    stub
}

#[tokio::test]
async fn cancelling_a_build_task_reaps_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("c.compact"),
        "pragma language_version >= 0.23;\n",
    )
    .unwrap();

    let pidfile = dir.path().join("grandchild.pid");
    let stub = write_stub(dir.path(), &pidfile);
    let client = connect_with_bin(dir.path(), &stub).await;

    let id = start_compile_task(&client, "c.compact", true, 60_000).await;

    // Wait for the grandchild to exist, rather than guessing at a sleep duration.
    let mut grandchild = None;
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(&pidfile)
            && let Ok(pid) = s.trim().parse::<u32>()
        {
            grandchild = Some(pid);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let grandchild = grandchild.expect("stub never spawned its grandchild");
    assert!(
        is_alive(grandchild),
        "grandchild should be running before we cancel"
    );

    assert_eq!(
        cancel_task(&client, &id).await.unwrap(),
        TaskStatus::Cancelled
    );
    assert_eq!(task_status(&client, &id).await, TaskStatus::Cancelled);

    // After SIGTERM->SIGKILL the grandchild is a zombie until its subreaper
    // (init/launchd) reaps it, and `kill -0` reports a zombie as alive — so poll
    // until it's fully gone rather than betting on a fixed duration that a loaded
    // CI box could blow past (mirrors the proc.rs kill_group unit test).
    let mut reaped = false;
    for _ in 0..60 {
        if !is_alive(grandchild) {
            reaped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        reaped,
        "grandchild {grandchild} was orphaned — the process group was not killed"
    );

    client.cancel().await.unwrap();
}
