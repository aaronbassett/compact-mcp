use tokio::io::AsyncReadExt;

use crate::CoreError;

/// A finished subprocess's exit status plus its size-capped stdout/stderr.
/// `*_truncated` is set when that stream produced more than the cap and the
/// overflow was drained-and-discarded rather than retained.
pub(crate) struct CappedOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
}

/// Wait for `child` to exit while draining stdout+stderr CONCURRENTLY, retaining
/// at most `limit` bytes from EACH stream. Bytes past the cap are read and
/// discarded — so a chatty child never deadlocks on a full pipe — but never
/// buffered, bounding capture memory no matter how much the subprocess writes.
/// This replaces `Child::wait_with_output`, whose capture is unbounded (and then
/// roughly doubled by a downstream `from_utf8_lossy().into_owned()`).
///
/// The child must have been spawned with piped stdout/stderr (as both
/// [`Toolchain::run`] and [`Toolchain::compile`] do).
pub(crate) async fn wait_with_capped_output(
    mut child: tokio::process::Child,
    limit: usize,
) -> std::io::Result<CappedOutput> {
    // Take the pipe handles out so `child.wait()` can be polled concurrently
    // with the two readers; without concurrent draining a full pipe would block
    // the child and `wait()` would never resolve (classic deadlock).
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (out_res, err_res, status) = tokio::join!(
        read_capped(stdout, limit),
        read_capped(stderr, limit),
        child.wait(),
    );
    let (stdout, stdout_truncated) = out_res?;
    let (stderr, stderr_truncated) = err_res?;
    Ok(CappedOutput {
        status: status?,
        stdout,
        stdout_truncated,
        stderr,
        stderr_truncated,
    })
}

/// Read `reader` to EOF, retaining at most `limit` bytes and reporting whether
/// anything past the cap was seen (and discarded). Reading continues past the
/// cap so the writing end always reaches EOF and the child can exit.
async fn read_capped<R>(reader: Option<R>, limit: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return Ok((Vec::new(), false));
    };
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if retained.len() < limit {
            let take = (limit - retained.len()).min(n);
            retained.extend_from_slice(&chunk[..take]);
            if take < n {
                truncated = true;
            }
        } else {
            // Over the cap: keep draining to EOF, but retain nothing.
            truncated = true;
        }
    }
    Ok((retained, truncated))
}

/// `from_utf8_lossy` a captured stream and, when it was truncated at the cap,
/// append a single marker line so a consumer sees the output was cut rather than
/// silently short.
pub(crate) fn lossy_with_marker(bytes: &[u8], truncated: bool, limit: usize) -> String {
    let mut s = String::from_utf8_lossy(bytes).into_owned();
    if truncated {
        s.push_str(&format!("\n[output truncated at {limit} bytes]"));
    }
    s
}

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

        // TIMING is the load-bearing assertion. `kill_group` must reap the whole
        // group promptly: SIGTERM lands on the grandchild `sleep` at once, with a
        // SIGKILL backstop 250ms later. A NEUTERED (no-op) `kill_group` signals
        // nothing, so the grandchild survives until its own `sleep 5` drains ~5s
        // later. We therefore bound the wait FAR below that natural expiry: a
        // grandchild still alive at the deadline proves the group was never killed.
        //
        // Ordering is what makes this non-vacuous. The prior version did
        // `child.wait().await` FIRST — but a neutered `kill_group` leaves the `sh`
        // wrapper `wait`ing on its backgrounded `sleep 5`, so `child.wait()` itself
        // blocks the full ~5s; by the time it returned the grandchild had died
        // naturally, so `is_alive` read false and a no-op reap still PASSED (just
        // slower). Here we start the clock, kill, then poll the grandchild directly
        // and never `wait()` before the deadline.
        //
        // Poll (don't single-sleep): after SIGKILL the grandchild is briefly a
        // zombie until its subreaper (launchd/init) reaps it, and `is_alive` reads
        // true for a zombie; a healthy reap clears in well under the deadline.
        const REAP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
        let start = std::time::Instant::now();
        kill_group(child_pid);

        let mut reaped = false;
        while start.elapsed() < REAP_DEADLINE {
            if !is_alive(grandchild_pid) {
                reaped = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            reaped,
            "grandchild still alive after {:?} (deadline {REAP_DEADLINE:?}; natural \
             `sleep 5` expiry ~5s) — kill_group did not reap the process group",
            start.elapsed(),
        );

        // Reap the `sh` wrapper so it doesn't linger as a zombie. In the healthy
        // path the group is already dead, so this returns at once; on the failure
        // path we've already panicked above (kill_on_drop then cleans up the direct
        // child).
        let _ = child.wait().await;
    }
}
