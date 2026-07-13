use crate::CoreError;

/// Spawn `cmd` as the leader of a brand-new process group, so that we can later
/// signal the entire tree. `compact` execs `compactc.bin`; without this, killing
/// the direct child leaves the compiler running.
pub fn spawn_group(cmd: &mut tokio::process::Command) -> Result<tokio::process::Child, CoreError> {
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    {
        // pgid 0 => "use my own pid as the group id".
        cmd.process_group(0);
    }
    Ok(cmd.spawn()?)
}

/// SIGTERM the group led by `pid`, then SIGKILL it. Because the child is its own
/// group leader, its pgid equals its pid.
///
/// # Blocking
///
/// Blocks the calling thread for ~250ms between the two signals. This is a
/// synchronous primitive intended for `Drop` and other sync contexts. Never call
/// it directly from an async task — it would stall a Tokio worker for the grace
/// period; wrap it in `spawn_blocking` (or an async equivalent) instead.
#[cfg(unix)]
pub fn kill_group(pid: u32) {
    let pgid = pid as libc::pid_t;
    // SAFETY: killpg is an integer-only syscall (no pointers). We target a pgid we
    // created. The return value is intentionally discarded: a dead group yields
    // ESRCH and there is nothing to recover from any other error here.
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_millis(250));
    // SAFETY: as above — SIGKILL the same group; any error (incl. ESRCH if SIGTERM
    // already reaped it) is intentionally ignored.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

/// On non-Unix we can only reach the direct child. Grandchildren leak; documented.
#[cfg(not(unix))]
pub fn kill_group(_pid: u32) {
    tracing::warn!("process-group kill is unsupported on this platform; compactc may be orphaned");
}

/// RAII guard that reaps a spawned process GROUP if it is still armed when it is
/// dropped. [`spawn_group`] sets `kill_on_drop`, but that reaps only the DIRECT
/// child (the `compact` wrapper); the worker it forks (`compactc.bin` /
/// `format-compact`) shares the group and would survive. On a plain future-drop —
/// e.g. an HTTP client disconnecting mid-build, the scenario `Toolchain::run` and
/// `Toolchain::compile` name in their own docs — there is no cancel or timeout arm
/// to run [`kill_group`], so without this guard that heavyweight worker is
/// orphaned with no reap, no timeout, and no `BuildGate` accounting.
///
/// The cancel/timeout arms reap explicitly (awaited, so the gate permit is held
/// until the tree is dead) and [`disarm`](Self::disarm) this guard, so it never
/// double-kills; it therefore fires ONLY on the drop-before-resolution path.
pub(crate) struct KillGroupOnDrop {
    pid: u32,
    armed: bool,
}

impl KillGroupOnDrop {
    /// Arm the guard for `pid`, the group leader (its pgid equals its pid — see
    /// [`spawn_group`]). A `0` pid is inert on drop (see the `killpg(0)` note in
    /// [`Drop`]).
    pub(crate) fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }

    /// Take over responsibility for reaping (or record that the child already
    /// exited), so [`Drop`] does nothing. Every `select!` arm that resolves calls
    /// this before returning.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        // Reuse the same `pid != 0` self-kill guard the async arms use: `killpg`
        // with a pgid of 0 targets the CALLER's process group — a self-inflicted
        // kill — so a `0` pid (a lost `child.id()`) must stay inert here too.
        if !self.armed || self.pid == 0 {
            return;
        }
        let pid = self.pid;
        // `kill_group` BLOCKS for the ~250ms SIGTERM->SIGKILL grace. We cannot
        // `.await` in `Drop`, and running it inline would stall the Tokio worker
        // that is dropping this future for the whole grace period. Mirror the
        // cancel/timeout path and offload it to the blocking pool: fire-and-forget
        // (Drop has nothing to await it with), but the entire GROUP is still reaped
        // because the grandchild keeps the group alive until SIGKILL lands. Outside
        // a runtime, fall back to a synchronous reap so the group is never leaked.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                // Fire-and-forget: `Drop` has nothing to await the handle with, so
                // drop the `JoinHandle` explicitly to detach the reaping task.
                // (`let _ =` would trip clippy's `let_underscore_future`, since a
                // `JoinHandle` is itself a future.)
                drop(handle.spawn_blocking(move || kill_group(pid)));
            }
            Err(_) => kill_group(pid),
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) fn is_alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs error checking without sending a signal.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// `sh` spawns a background `sleep` (the grandchild) and prints its pid.
    /// Killing only the direct child would leave `sleep` running.
    #[tokio::test]
    async fn kill_group_reaps_the_grandchild_not_just_the_child() {
        let mut cmd = tokio::process::Command::new("sh");
        // `sleep 5` (not 30) bounds the leaked-process lifetime if this test ever
        // regresses — long enough to still be alive at the checkpoint below.
        cmd.arg("-c")
            .arg("sleep 5 & echo $!; wait")
            .stdout(std::process::Stdio::piped());

        let mut child = spawn_group(&mut cmd).unwrap();
        let child_pid = child.id().expect("child has a pid");

        let mut buf = [0u8; 32];
        let n = child.stdout.as_mut().unwrap().read(&mut buf).await.unwrap();
        let grandchild_pid: u32 = String::from_utf8_lossy(&buf[..n]).trim().parse().unwrap();

        assert!(is_alive(grandchild_pid), "grandchild should be running");

        kill_group(child_pid);
        let _ = child.wait().await;

        // Poll instead of a single fixed sleep: after SIGKILL the grandchild becomes
        // a zombie until its subreaper (launchd/init) reaps it, and `is_alive` reads
        // true for a zombie. Poll up to ~2s so a loaded CI box can't flake, while a
        // genuine orphan (still sleeping) keeps failing the assertion.
        let mut reaped = false;
        for _ in 0..40 {
            if !is_alive(grandchild_pid) {
                reaped = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(reaped, "grandchild was orphaned — process group not killed");
    }
}
