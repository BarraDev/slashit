// SPDX-License-Identifier: MPL-2.0
//! Runs the built desktop binary with arguments that must not start the GUI.
//!
//! Every XDG directory, and HOME, points at a fresh temporary directory, so
//! the test never reads or writes the developer's real SlashIt state. After
//! each run the directories must still be empty: no config, data, state,
//! cache, runtime files, control socket, or even an empty directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// A binary that opened a window would never exit, so this bound turns a
/// regression into a failure instead of a hung test run.
const EXIT_DEADLINE: Duration = Duration::from_secs(20);

struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("create sandbox"),
        }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.root.path().join(name);
        std::fs::create_dir_all(&path).expect("create sandbox directory");
        path
    }

    /// The environment variable for each XDG root and HOME, paired with its
    /// directory inside the sandbox. Every directory starts empty.
    fn state_dirs(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("XDG_CONFIG_HOME", self.dir("config")),
            ("XDG_DATA_HOME", self.dir("data")),
            ("XDG_STATE_HOME", self.dir("state")),
            ("XDG_CACHE_HOME", self.dir("cache")),
            ("XDG_RUNTIME_DIR", self.dir("runtime")),
            ("HOME", self.dir("home")),
        ]
    }

    fn run(&self, args: &[&str]) -> (std::process::ExitStatus, String, String) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_slashit-ui"))
            .args(args)
            .envs(self.state_dirs())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn slashit-ui");

        let deadline = Instant::now() + EXIT_DEADLINE;
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll slashit-ui") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("slashit-ui {args:?} did not exit; it may have started the GUI");
            }
            thread::sleep(Duration::from_millis(20));
        };

        let output = child.wait_with_output().expect("collect output");
        (
            status,
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn assert_untouched(&self, args: &[&str]) {
        for (var, dir) in self.state_dirs() {
            let entries = tree_entries(&dir);
            assert!(
                entries.is_empty(),
                "{args:?} wrote under {var}={}: {entries:?}",
                dir.display()
            );
        }
    }
}

/// Every entry under `path` at any depth: files, symlinks, sockets and
/// directories, including empty ones. Symlinks are listed, not followed.
fn tree_entries(path: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(path).expect("read sandbox directory") {
        let entry = entry.expect("read sandbox entry");
        let file_type = entry.file_type().expect("stat sandbox entry");
        let entry_path = entry.path();
        if file_type.is_dir() {
            found.extend(tree_entries(&entry_path));
        }
        found.push(entry_path);
    }
    found
}

#[test]
fn tree_entries_reports_empty_directories_at_any_depth() {
    let root = tempfile::tempdir().expect("create directory");
    assert!(tree_entries(root.path()).is_empty());

    let nested = root.path().join("a").join("b");
    std::fs::create_dir_all(&nested).expect("create nested directories");
    let entries = tree_entries(root.path());
    assert!(entries.contains(&root.path().join("a")), "{entries:?}");
    assert!(entries.contains(&nested), "{entries:?}");
}

#[test]
fn version_prints_the_version_and_exits_zero_without_state() {
    for flag in ["--version", "-V"] {
        let sandbox = Sandbox::new();
        let (status, stdout, _) = sandbox.run(&[flag]);
        assert_eq!(status.code(), Some(0), "{flag}");
        assert!(
            stdout.contains(env!("CARGO_PKG_VERSION")),
            "{flag} stdout was {stdout:?}"
        );
        sandbox.assert_untouched(&[flag]);
    }
}

#[test]
fn help_prints_usage_and_exits_zero_without_state() {
    for flag in ["--help", "-h"] {
        let sandbox = Sandbox::new();
        let (status, stdout, _) = sandbox.run(&[flag]);
        assert_eq!(status.code(), Some(0), "{flag}");
        assert!(stdout.contains("Usage"), "{flag} stdout was {stdout:?}");
        sandbox.assert_untouched(&[flag]);
    }
}

#[test]
fn unknown_argument_prints_usage_to_stderr_and_exits_nonzero_without_state() {
    let sandbox = Sandbox::new();
    let (status, stdout, stderr) = sandbox.run(&["--bogus"]);
    assert_ne!(status.code(), Some(0));
    assert_eq!(status.code(), Some(2));
    assert!(stdout.is_empty(), "stdout was {stdout:?}");
    assert!(stderr.contains("Usage"), "stderr was {stderr:?}");
    sandbox.assert_untouched(&["--bogus"]);
}
