use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn stdout_carries_only_jsonrpc_not_logs() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_compact-mcp"))
        .args(["--transport", "stdio", "--workspace-root"])
        .arg(dir.path())
        .env("RUST_LOG", "debug") // force noisy logging; it must all go to stderr
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{req}\n").as_bytes())
        .unwrap();
    // Dropping the stdin handle above closes the write end, which EOFs the
    // server's stdin reader; the server then finishes `.waiting()` and the
    // process exits. `wait_with_output` blocks until that happens, and if
    // the server ever regressed into hanging on stdin, this test would hang
    // the whole suite — so run it on a helper thread and bound it with a
    // channel `recv_timeout` instead of waiting on the child directly.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let out = child.wait_with_output();
        let _ = tx.send(out);
    });
    let out = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("compact-mcp did not exit within 10s after stdin closed")
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let first = stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .expect("no stdout");
    let v: serde_json::Value = serde_json::from_str(first)
        .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {first:?}"));
    assert_eq!(
        v["jsonrpc"], "2.0",
        "first stdout line must be a JSON-RPC message, got: {first}"
    );
}
