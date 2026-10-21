//! Exercise the sourced shell library under the strict shell options used by
//! production job scripts. A fake logger keeps these tests independent of PBS.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn fake_logger(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("fake-logger");
    executable(
        &path,
        "#!/bin/bash\n\
         case \"$1\" in --write-manifest|--merge) exit 0;; esac\n\
         trap 'exit 0' TERM\n\
         while :; do sleep 0.1; done\n",
    );
    path
}

fn run(script: &str, dir: &Path, fake: &Path) -> Output {
    Command::new("bash")
        .arg("-c")
        .arg(script)
        .env(
            "LOGGER_SH",
            concat!(env!("CARGO_MANIFEST_DIR"), "/logger-rs.sh"),
        )
        .env("TEST_DIR", dir)
        .env("FAKE_LOGGER", fake)
        .env_remove("PBS_JOBID")
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "exit={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn logger_failure_does_not_fail_a_successful_workload() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_logger(dir.path());
    let output = Command::new("bash")
        .arg("-c")
        .arg(
            "set -e; source \"$LOGGER_SH\"; \
             telemetry_start --single-node --telemetry-bin \"$FAKE_LOGGER\" --output \"$TEST_DIR/job.log\"; \
             kill -KILL \"$_HT_LOCAL_PID\"; wait \"$_HT_LOCAL_PID\" || true; \
             telemetry_stop; echo WORKLOAD_COMPLETED",
        )
        .env("LOGGER_SH", concat!(env!("CARGO_MANIFEST_DIR"), "/logger-rs.sh"))
        .env("TEST_DIR", dir.path())
        .env("FAKE_LOGGER", &fake)
        .env_remove("PBS_JOBID")
        .output()
        .unwrap();
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("WORKLOAD_COMPLETED"));
}

#[test]
fn existing_exit_trap_runs_once_after_telemetry_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_logger(dir.path());
    let output = run(
        "set -e; source \"$LOGGER_SH\"; \
         trap 'printf \"cleanup\\n\" >> \"$TEST_DIR/cleanup\"' EXIT; \
         telemetry_start --single-node --telemetry-bin \"$FAKE_LOGGER\" --output \"$TEST_DIR/job.log\"; \
         exit 0",
        dir.path(),
        &fake,
    );
    assert_success(&output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("cleanup")).unwrap(),
        "cleanup\n"
    );
}

#[test]
fn existing_signal_trap_runs_before_the_original_exit_trap() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_logger(dir.path());
    let output = run(
        "source \"$LOGGER_SH\"; \
         trap 'echo term >> \"$TEST_DIR/traps\"' TERM; \
         trap 'echo exit >> \"$TEST_DIR/traps\"' EXIT; \
         telemetry_start --single-node --telemetry-bin \"$FAKE_LOGGER\" --output \"$TEST_DIR/job.log\"; \
         kill -TERM $$",
        dir.path(),
        &fake,
    );
    assert_eq!(output.status.code(), Some(143));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("traps")).unwrap(),
        "term\nexit\n"
    );
}

#[test]
fn missing_qstat_walltime_does_not_abort_a_pipefail_job() {
    let dir = tempfile::tempdir().unwrap();
    let qstat = dir.path().join("qstat");
    let timeout = dir.path().join("timeout");
    executable(&qstat, "#!/bin/sh\nprintf 'Job Id: 123\\n'\n");
    executable(&timeout, "#!/bin/sh\nshift\nexec \"$@\"\n");
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap()
    );
    let output = Command::new("bash")
        .arg("-c")
        .arg(
            "set -euo pipefail; source \"$LOGGER_SH\"; \
             telemetry_start --telemetry-bin /bin/true; echo WORKLOAD_REACHED",
        )
        .env(
            "LOGGER_SH",
            concat!(env!("CARGO_MANIFEST_DIR"), "/logger-rs.sh"),
        )
        .env("PBS_JOBID", "123")
        .env("PATH", path)
        .output()
        .unwrap();
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("WORKLOAD_REACHED"));
}
