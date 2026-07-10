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
#[cfg(unix)]
pub fn kill_group(pid: u32) {
    let pgid = pid as libc::pid_t;
    // SAFETY: killpg on a pgid we created. A dead group returns ESRCH, which we ignore.
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_millis(250));
    // SAFETY: killpg on a pgid we created. A dead group returns ESRCH, which we ignore.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

/// On non-Unix we can only reach the direct child. Grandchildren leak; documented.
#[cfg(not(unix))]
pub fn kill_group(_pid: u32) {
    tracing::warn!("process-group kill is unsupported on this platform; compactc may be orphaned");
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
        cmd.arg("-c")
            .arg("sleep 30 & echo $!; wait")
            .stdout(std::process::Stdio::piped());

        let mut child = spawn_group(&mut cmd).unwrap();
        let child_pid = child.id().expect("child has a pid");

        let mut buf = [0u8; 32];
        let n = child.stdout.as_mut().unwrap().read(&mut buf).await.unwrap();
        let grandchild_pid: u32 = String::from_utf8_lossy(&buf[..n]).trim().parse().unwrap();

        assert!(is_alive(grandchild_pid), "grandchild should be running");

        kill_group(child_pid);
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let _ = child.wait().await;

        assert!(
            !is_alive(grandchild_pid),
            "grandchild was orphaned — process group not killed"
        );
    }
}
