#![cfg(unix)]
//! A plain future-drop of a `compact` invocation must reap the whole process
//! GROUP, not just the tracked wrapper pid. `proc::spawn_group` sets
//! `kill_on_drop`, which reaps only the direct child; the worker it forks
//! (`compactc.bin` for `compile`, `format-compact` for `format`) shares the group
//! and would be orphaned on a bare drop — an HTTP client disconnecting mid-build,
//! the very scenario the `Toolchain::run`/`Toolchain::compile` doc comments name.
//! The `proc::KillGroupOnDrop` guard closes that gap by `killpg`-ing the group.
//!
//! These reproduce that drop path WITHOUT any explicit cancellation: a
//! never-cancelled [`CancellationToken`] plus a 600s timeout that cannot fire, so
//! the only thing that reaps the tree is the drop guard. Without the guard the
//! `sleep 30` grandchild survives and the reap assertion fails.
//!
//! Hermetic: an `sh` stub stands in for `compact`, so this runs in the fast CI
//! job with no `compact` binary and no `toolchain-tests` gate. Modelled on
//! `compact-mcp/tests/cancellation.rs`, which exercises the cancel path.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use compact_mcp_core::Toolchain;
use compact_mcp_core::Workspace;
use compact_mcp_core::toolchain::compile::CompileRequest;
use compact_mcp_core::toolchain::fmt::FmtInput;
use tokio_util::sync::CancellationToken;

/// A valid counter contract, so `format`'s internal parse gate passes and it
/// actually spawns the (stubbed) subprocess — the `run` path where mod.rs installs
/// its drop guard. `compile` has no parse gate, so a bare pragma suffices there.
const COUNTER: &str = "pragma language_version >= 0.23;\nimport CompactStandardLibrary;\n\nexport ledger round: Counter;\n\nexport circuit increment(): [] {\n  round.increment(1);\n}\n";

/// `kill -0` succeeds iff the process exists. Shelling out (rather than pulling in
/// `libc`) keeps this test dependency-free, mirroring `cancellation.rs`.
fn is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Stands in for `compact`, which forks a worker (`compactc.bin`/`format-compact`).
/// It backgrounds a long-lived grandchild, records its pid, and `wait`s — so
/// reaping only the direct child would leave the grandchild orphaned, exactly as a
/// real build would leave its worker running.
fn write_stub(dir: &Path, pidfile: &Path) -> PathBuf {
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

/// Poll until the stub has written its grandchild pid and that process is live,
/// rather than guessing at a fixed sleep. Returns the (live) grandchild pid.
async fn wait_for_live_grandchild(pidfile: &Path) -> u32 {
    for _ in 0..200 {
        if let Ok(s) = std::fs::read_to_string(pidfile)
            && let Ok(pid) = s.trim().parse::<u32>()
            && is_alive(pid)
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("stub never spawned a live grandchild");
}

/// Assert the grandchild is reaped within a few seconds. After SIGTERM->SIGKILL it
/// is a zombie until its subreaper (init/launchd) collects it, and `kill -0` reads
/// a zombie as alive — so poll until it is fully gone rather than bet on a fixed
/// duration a loaded CI box could blow past (mirrors cancellation.rs). A genuine
/// orphan (still sleeping) keeps failing the assertion.
async fn assert_reaped(pid: u32) {
    for _ in 0..120 {
        if !is_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("grandchild {pid} was orphaned — the process group was not killed on drop");
}

#[tokio::test]
async fn dropping_a_compile_future_reaps_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("c.compact"),
        "pragma language_version >= 0.23;\n",
    )
    .unwrap();
    let ws = Workspace::new(dir.path()).unwrap();

    let pidfile = dir.path().join("grandchild.pid");
    let stub = write_stub(dir.path(), &pidfile);
    let tc = Toolchain::new(stub.to_string_lossy().into_owned(), None);

    let req = CompileRequest {
        source: ws.resolve("c.compact").unwrap(),
        target_dir: ws.resolve("out").unwrap(),
        skip_zk: true,
        no_communications_commitment: false,
        source_root: None,
    };

    // Race the (never-completing) compile against the grandchild coming alive.
    // When the detector wins, `select!` DROPS the still-pending compile future it
    // owns — a plain future-drop with NO CancellationToken cancel. The 600s
    // timeout guarantees the timeout arm can't fire and mask the drop path.
    let grandchild = tokio::select! {
        biased;
        _ = tc.compile(&req, CancellationToken::new(), Duration::from_secs(600)) => {
            panic!("the sleep-30 stub cannot have compiled; the compile future should still be pending");
        }
        pid = wait_for_live_grandchild(&pidfile) => pid,
    };

    // Without KillGroupOnDrop, `kill_on_drop` reaps only the `sh` wrapper and this
    // `sleep 30` grandchild survives, so this assertion fails on unpatched code.
    assert_reaped(grandchild).await;
}

#[tokio::test]
async fn dropping_a_format_future_reaps_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();

    let pidfile = dir.path().join("grandchild.pid");
    let stub = write_stub(dir.path(), &pidfile);
    let tc = Toolchain::new(stub.to_string_lossy().into_owned(), None);

    // `format` runs through `Toolchain::run` (mod.rs) — the second drop-guarded
    // site. Same race-then-drop shape as the compile test above.
    let ct = CancellationToken::new();
    let grandchild = tokio::select! {
        biased;
        _ = tc.format(&ws, FmtInput::Source(COUNTER.into()), false, &ct) => {
            panic!("the sleep-30 stub cannot have formatted; the format future should still be pending");
        }
        pid = wait_for_live_grandchild(&pidfile) => pid,
    };

    assert_reaped(grandchild).await;
}
