//! A `quiesce` whose reply is lost must still name its operation on stderr,
//! because the instance may already have closed admission under that id.
#![cfg(unix)]

use std::io::Read;
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::time::Duration;

#[test]
fn a_lost_quiesce_reply_still_names_the_generated_operation() {
    let dir = std::env::temp_dir().join(format!("slashit-quiesce-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch directory");
    let socket = dir.join("ipc.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).expect("bind fake endpoint");

    // The fake instance reads the whole request, then closes without replying.
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client connects");
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut request = Vec::new();
        let _ = stream.read_to_end(&mut request);
        request
    });

    let output = Command::new(env!("CARGO_BIN_EXE_slashit"))
        .arg("--endpoint")
        .arg(format!("unix:{}", socket.display()))
        .arg("--timeout")
        .arg("3")
        .arg("quiesce")
        .output()
        .expect("run slashit quiesce");
    let request = server.join().expect("fake endpoint finished");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(!output.status.success(), "a lost reply must be an error exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let request = String::from_utf8_lossy(&request);
    let line = stderr
        .lines()
        .find(|l| l.starts_with("quiesce operation: "))
        .unwrap_or_else(|| panic!("no operation line on stderr:\n{stderr}"));
    let operation = line
        .trim_start_matches("quiesce operation: ")
        .split_whitespace()
        .next()
        .expect("operation id");
    assert!(operation.starts_with("cli-"), "generated id: {operation}");
    assert!(
        request.contains(operation),
        "the id printed for recovery must be the one the request carried\nstderr: {stderr}\nrequest: {request}"
    );
}
