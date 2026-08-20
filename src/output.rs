use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::types::{MergedSummary, TelemetrySample, TelemetrySummary};

/// Write a telemetry sample as NDJSON (newline-delimited JSON)
pub fn write_ndjson_line(writer: &mut BufWriter<File>, sample: &TelemetrySample) -> Result<()> {
    let json = serde_json::to_string(sample)?;
    writeln!(writer, "{}", json)?;
    Ok(())
}

/// Write the per-node summary atomically.
pub fn write_summary_file(path: &Path, summary: &TelemetrySummary) -> Result<()> {
    write_json_atomic(path, summary)
}

/// Write a merged, job-level summary atomically.
pub fn write_merged_summary(path: &Path, merged: &MergedSummary) -> Result<()> {
    write_json_atomic(path, merged)
}

/// Write any serialisable value as pretty JSON: temp file, fsync, rename.
///
/// Two reasons for the dance, both of which apply to every JSON file this
/// binary produces:
///
/// - The fsync matters on Lustre. Without it a rename can survive a node crash
///   as a zero-length file, which looks like a successfully written but empty
///   document to anything downstream.
/// - The rename means a dashboard polling this directory never observes a
///   half-written file under the final name.
pub fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    ensure_parent_dir(path)?;

    // Keep the temp file in the same directory so the rename stays atomic
    // (a rename across filesystems is not).
    let tmp_path = sibling_tmp_path(path);

    {
        let file =
            File::create(&tmp_path).with_context(|| format!("Failed to create {:?}", tmp_path))?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, value)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        // Get the data on disk before the rename makes it visible.
        writer.get_ref().sync_all()?;
    }

    // Before the rename, so the file is never briefly visible under its final
    // name with the wrong mode.
    relax_permissions(&tmp_path);

    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("Failed to rename {:?} -> {:?}", tmp_path, path))?;

    Ok(())
}

/// Make a telemetry file readable by anyone who can reach the directory.
///
/// These land in a shared project directory that a dashboard — often running as
/// a *different* user — has to read, so the mode cannot be left to chance.
///
/// It was being left to chance. Nothing here set a mode, so files inherited the
/// umask of whatever shell launched the logger: the head node picked up the
/// login default and produced `-rw-r--r--`, while remote nodes launched through
/// `pbs_tmrsh` got a stricter default and produced `-rw-------`. One job, two
/// permission sets, and the multi-node files were the unreadable ones.
///
/// Applied *after* creation rather than via `OpenOptions::mode()`, because the
/// mode passed at open time is masked by the umask — which is the very thing
/// being worked around. `set_permissions` is not masked.
///
/// A failure here is deliberately not fatal: the telemetry is written either
/// way, and refusing to run because a chmod failed would be a worse outcome
/// than a file somebody has to chmod by hand.
#[cfg(unix)]
fn relax_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)) {
        eprintln!("warning: could not set permissions on {path:?}: {err}");
    }
}

#[cfg(not(unix))]
fn relax_permissions(_path: &Path) {}

/// `/a/b/job.log.summary.json` -> `/a/b/.job.log.summary.json.tmp`
///
/// Previously this used `Path::with_extension("tmp")`, which replaces the last
/// extension rather than appending: `psutil_123.log` became `psutil_123.tmp`,
/// and two jobs writing adjacent files could collide.
fn sibling_tmp_path(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "summary".to_string());
    path.with_file_name(format!(".{}.tmp", name))
}

/// Create a buffered writer for NDJSON output, creating parent directories.
pub fn create_ndjson_writer(path: &Path) -> Result<BufWriter<File>> {
    ensure_parent_dir(path)?;

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Failed to open output file {:?}", path))?;

    // Unconditional rather than only-on-create: cheap, and it repairs a file
    // left at 0600 by a logger from before this change that is now being
    // appended to.
    relax_permissions(path);

    Ok(BufWriter::with_capacity(64 * 1024, file))
}

/// Create the parent directory if it does not exist.
///
/// Without this, a user who forgets the `mkdir -p` step in the README loses the
/// entire run to an opaque "No such file or directory".
fn ensure_parent_dir(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() || parent.exists() {
        return Ok(());
    }

    std::fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create output directory {:?}", parent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_for_test() -> TelemetrySample {
        TelemetrySample {
            event: "job_log".to_string(),
            user_id: "testuser".to_string(),
            job_id: "job123".to_string(),
            queue: "normal".to_string(),
            job_name: "test".to_string(),
            project: "proj1".to_string(),
            hostname: "test-node".to_string(),
            t: 1234567890.5,
            dt_sec: 0.52,
            allowed_cpus: vec![0, 1],
            cpu_pct_sum: 150.0,
            rss_bytes_sum: 1024 * 1024 * 100,
            n_procs: 5,
            system_percpu_pct: vec![75.0, 75.0],
            tree_percpu_pct: vec![75.0, 75.0],
            system_cpu_efficiency: 150.0,
            cpu_efficiency_pct: 75.0,
            booked_walltime: "01:00:00".to_string(),
            booked_walltime_sec: 3600,
            booked_mem: "4GB".to_string(),
            booked_mem_bytes: 4 * 1024 * 1024 * 1024,
            n_threads: 10,
            n_open_fds: 15,
            major_faults: 42,
            minor_faults: 1234,
            io_read_bytes: 1024 * 1024 * 100,
            io_write_bytes: 1024 * 1024 * 50,
            io_read_ops: 1000,
            io_write_ops: 500,
            swap_bytes: 0,
            cgroup_mem_bytes: Some(1024 * 1024 * 90),
            cgroup_mem_peak_bytes: Some(1024 * 1024 * 110),
            cgroup_swap_bytes: Some(0),
            cgroup_io_read_bytes: Some(1024 * 1024 * 120),
            cgroup_io_write_bytes: Some(1024 * 1024 * 60),
            cgroup_io_read_ops: Some(1200),
            cgroup_io_write_ops: Some(600),
            num_nodes: Some(1),
            tasks_per_node: Some(1),
            cpus_per_task: Some(2),
            net_recv_bytes: 1024 * 1024 * 10,
            net_sent_bytes: 1024 * 1024 * 5,
            net_recv_packets: 10000,
            net_sent_packets: 5000,
            net_is_node_scoped: true,
            gpu_utilization: vec![],
            gpu_memory_used: vec![],
            gpu_memory_total: vec![],
            gpu_temperature: vec![],
            gpu_power: vec![],
            gpu_indices: vec![],
            gpu_is_node_scoped: true,
        }
    }

    fn summary_for_test() -> TelemetrySummary {
        TelemetrySummary::new(
            "testuser".to_string(),
            "job123".to_string(),
            "normal".to_string(),
            "testjob".to_string(),
            "proj1".to_string(),
            12345,
            0.5,
            vec![0, 1],
            "cgroup-v1".to_string(),
            "/sys/fs/cgroup/cpuacct/test".to_string(),
            Some("/sys/fs/cgroup/cpuset/test".to_string()),
            "01:00:00".to_string(),
            3600,
            "4GB".to_string(),
            4 * 1024 * 1024 * 1024,
        )
    }

    #[test]
    fn test_write_ndjson_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.log");

        let mut writer = create_ndjson_writer(&path).unwrap();
        write_ndjson_line(&mut writer, &sample_for_test()).unwrap();
        writer.flush().unwrap();
        drop(writer);

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["user_id"], "testuser");
        assert_eq!(parsed["cgroup_mem_bytes"], 1024 * 1024 * 90);
    }

    #[test]
    fn test_write_summary_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("job.log.summary.json");

        write_summary_file(&path, &summary_for_test()).unwrap();
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["user_id"], "testuser");
        assert_eq!(parsed["cgroup_version"], "cgroup-v1");
    }

    #[test]
    fn test_write_summary_leaves_no_temp_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("job.log.summary.json");

        write_summary_file(&path, &summary_for_test()).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {:?}",
            leftovers
        );
    }

    #[test]
    fn test_creates_missing_parent_directories() {
        let dir = tempdir().unwrap();
        // Two levels that do not exist yet — the old code failed here with a
        // bare ENOENT and lost the whole run.
        let path = dir.path().join("gdata").join("user").join("out.log");

        let writer = create_ndjson_writer(&path);
        assert!(
            writer.is_ok(),
            "should create parent dirs: {:?}",
            writer.err()
        );
        assert!(path.exists());
    }

    #[test]
    fn test_sibling_tmp_path_appends_rather_than_replacing() {
        assert_eq!(
            sibling_tmp_path(Path::new("/g/data/psutil_123.log")),
            std::path::PathBuf::from("/g/data/.psutil_123.log.tmp")
        );
        // Same directory, so the rename stays atomic.
        assert_eq!(
            sibling_tmp_path(Path::new("/g/data/x.summary.json")).parent(),
            Some(Path::new("/g/data"))
        );
    }

    #[test]
    fn test_summary_roundtrips() {
        // The summary must deserialise back into itself — this catches a serde
        // attribute that serialises a field but cannot read it back.
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.json");
        let original = summary_for_test();

        write_summary_file(&path, &original).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let restored: TelemetrySummary = serde_json::from_str(&content).unwrap();

        assert_eq!(restored.user_id, original.user_id);
        assert_eq!(restored.cgroup_version, original.cgroup_version);
        assert_eq!(restored.exit_status, None);
    }
}
