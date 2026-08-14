#![cfg(unix)]

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct ProcessCleanup {
    parent: Child,
    parent_pid: i32,
    guard_pid: Option<i32>,
}

impl Drop for ProcessCleanup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-self.parent_pid, libc::SIGKILL);
            if let Some(guard_pid) = self.guard_pid {
                libc::kill(-guard_pid, libc::SIGKILL);
            }
        }
        let _ = self.parent.wait();
    }
}

fn wait_for_pid(path: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(value) = std::fs::read_to_string(path) {
            if let Ok(pid) = value.trim().parse::<i32>() {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "PID file was not created: {path:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn process_is_running(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("inspect process state");
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .trim()
            .chars()
            .next()
            .is_some_and(|state| state != 'Z')
}

fn wait_until_stopped(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_is_running(pid) {
        assert!(
            Instant::now() < deadline,
            "process {pid} survived its parent"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn daemon_sigkill_cannot_orphan_guarded_compiler_processes() {
    let temp = tempfile::tempdir().unwrap();
    let guard_pid_file = temp.path().join("guard.pid");
    let payload_pid_file = temp.path().join("payload.pid");
    let perch = env!("CARGO_BIN_EXE_perch");

    // This shell stands in for the daemon. The hidden guard must observe its
    // exact parent disappearing and kill the private compiler process group.
    let script = r#"
        "$PERCH_TEST_BIN" compiler-guard --parent-pid "$$" -- \
            sh -c 'echo $$ > "$PERCH_TEST_PAYLOAD_PID"; exec sleep 60' &
        echo $! > "$PERCH_TEST_GUARD_PID"
        wait
    "#;
    let parent = Command::new("sh")
        .arg("-c")
        .arg(script)
        .env("PERCH_TEST_BIN", perch)
        .env("PERCH_TEST_GUARD_PID", &guard_pid_file)
        .env("PERCH_TEST_PAYLOAD_PID", &payload_pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("spawn surrogate daemon");
    let parent_pid = parent.id() as i32;
    let mut cleanup = ProcessCleanup {
        parent,
        parent_pid,
        guard_pid: None,
    };
    let guard_pid = wait_for_pid(&guard_pid_file);
    cleanup.guard_pid = Some(guard_pid);
    let payload_pid = wait_for_pid(&payload_pid_file);
    assert!(process_is_running(guard_pid));
    assert!(process_is_running(payload_pid));

    assert_eq!(unsafe { libc::kill(parent_pid, libc::SIGKILL) }, 0);
    let status = cleanup.parent.wait().expect("reap surrogate daemon");
    assert!(!status.success());

    wait_until_stopped(guard_pid);
    wait_until_stopped(payload_pid);
    cleanup.guard_pid = None;
}
