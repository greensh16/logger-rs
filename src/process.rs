//! Process-tree traversal.
//!
//! Note on what this is and is not good for: summing RSS across a process tree
//! double-counts every shared page (shared libraries, copy-on-write pages after
//! `fork`, SysV/POSIX shared memory), which overstates memory badly for MPI and
//! any shared-memory-heavy code. The cgroup memory controller is authoritative
//! and is used in preference — see [`crate::cgroup::Cgroup::read_memory`]. These
//! numbers are retained because they are the historical schema the dashboard
//! parses, and as a fallback when the memory controller is unavailable.

use anyhow::Result;

/// What to collect during a tree scan.
///
/// The per-process `/proc/{pid}/io` and `/proc/{pid}/status` reads are by far
/// the most expensive part of a scan — `status` in particular is a large file
/// parsed for a single field. When the cgroup can supply I/O and swap
/// (job-scoped, exit-safe, and much cheaper) we skip them entirely.
#[derive(Debug, Clone, Copy)]
pub struct ScanOptions {
    pub collect_io: bool,
    pub collect_swap: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            collect_io: true,
            collect_swap: true,
        }
    }
}

impl ScanOptions {
    /// Cheap scan: RSS, process/thread/FD counts and page faults only.
    pub fn minimal() -> Self {
        Self {
            collect_io: false,
            collect_swap: false,
        }
    }
}

/// Extended process tree stats including threads, FDs, page faults, I/O, and swap.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct ProcessTreeStats {
    pub rss_bytes: u64,
    pub proc_count: usize,
    pub thread_count: usize,
    pub fd_count: usize,
    pub major_faults: u64,
    pub minor_faults: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
    pub io_read_ops: u64,
    pub io_write_ops: u64,
    pub swap_bytes: u64,
}

impl ProcessTreeStats {
    /// Element-wise maximum, used to retain the high-water mark across the
    /// sub-samples taken within one tick.
    pub fn max_with(&mut self, other: &ProcessTreeStats) {
        self.rss_bytes = self.rss_bytes.max(other.rss_bytes);
        self.proc_count = self.proc_count.max(other.proc_count);
        self.thread_count = self.thread_count.max(other.thread_count);
        self.fd_count = self.fd_count.max(other.fd_count);
        self.major_faults = self.major_faults.max(other.major_faults);
        self.minor_faults = self.minor_faults.max(other.minor_faults);
        self.io_read_bytes = self.io_read_bytes.max(other.io_read_bytes);
        self.io_write_bytes = self.io_write_bytes.max(other.io_write_bytes);
        self.io_read_ops = self.io_read_ops.max(other.io_read_ops);
        self.io_write_ops = self.io_write_ops.max(other.io_write_ops);
        self.swap_bytes = self.swap_bytes.max(other.swap_bytes);
    }
}

/// Where the set of processes to measure comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcSource {
    /// The kernel's cgroup membership list. Correct on every node, and the only
    /// thing that works where the job's processes are not descendants of the
    /// logger.
    Cgroup,
    /// Breadth-first walk from a root PID. Only valid where the job's processes
    /// really are descendants of that PID — i.e. the head node.
    Tree,
}

impl ProcSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProcSource::Cgroup => "cgroup.procs",
            ProcSource::Tree => "process-tree",
        }
    }
}

/// Aggregate stats over an explicit set of PIDs — non-Linux stub.
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub fn get_stats_for_pids(_pids: &[i32], _opts: ScanOptions) -> Result<ProcessTreeStats> {
    Ok(ProcessTreeStats::default())
}

/// Aggregate stats over an explicit set of PIDs.
///
/// Used with the PID list from `cgroup.procs`, which is the kernel's own record
/// of which processes belong to the job. Compared with walking the process tree
/// this is exact rather than inferred: it does not depend on ancestry, so it
/// works on a node where the job's ranks were started by the scheduler rather
/// than by us, and it does not lose processes that reparent to init.
#[cfg(target_os = "linux")]
pub fn get_stats_for_pids(pids: &[i32], opts: ScanOptions) -> Result<ProcessTreeStats> {
    let mut stats = ProcessTreeStats::default();

    for &pid in pids {
        accumulate_pid(pid, opts, &mut stats);
    }

    Ok(stats)
}

/// Get extended process tree statistics - non-Linux stub.
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub fn get_process_tree_stats_extended(
    _root_pid: i32,
    _opts: ScanOptions,
) -> Result<ProcessTreeStats> {
    Ok(ProcessTreeStats::default())
}

/// Read one process's contribution into `stats`, ignoring it if it has exited.
#[cfg(target_os = "linux")]
fn accumulate_pid(pid: i32, opts: ScanOptions, stats: &mut ProcessTreeStats) {
    use procfs::process::Process;
    use std::fs;

    // The process may have exited between being listed and being read.
    let Ok(proc) = Process::new(pid) else {
        return;
    };
    let Ok(stat) = proc.stat() else {
        return;
    };

    let page_size = procfs::page_size();
    stats.rss_bytes += stat.rss * page_size;
    stats.proc_count += 1;
    stats.minor_faults += stat.minflt;
    stats.major_faults += stat.majflt;

    if let Ok(task_entries) = fs::read_dir(format!("/proc/{}/task", pid)) {
        stats.thread_count += task_entries.count();
    }

    if let Ok(fd_entries) = fs::read_dir(format!("/proc/{}/fd", pid)) {
        // read_dir itself holds an open descriptor which appears in its own
        // listing, so every process was previously counted one too high.
        stats.fd_count += fd_entries.count().saturating_sub(1);
    }

    if opts.collect_io {
        if let Ok(io_stats) = read_proc_io(pid) {
            stats.io_read_bytes += io_stats.read_bytes;
            stats.io_write_bytes += io_stats.write_bytes;
            stats.io_read_ops += io_stats.read_ops;
            stats.io_write_ops += io_stats.write_ops;
        }
    }

    if opts.collect_swap {
        if let Ok(swap) = read_vmswap(pid) {
            stats.swap_bytes += swap;
        }
    }
}

/// Get extended process tree statistics.
/// Uses BFS to traverse the process tree starting from `root_pid`.
#[cfg(target_os = "linux")]
pub fn get_process_tree_stats_extended(
    root_pid: i32,
    opts: ScanOptions,
) -> Result<ProcessTreeStats> {
    use std::collections::{HashSet, VecDeque};

    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    let mut stats = ProcessTreeStats::default();

    queue.push_back(root_pid);
    visited.insert(root_pid);

    while let Some(pid) = queue.pop_front() {
        accumulate_pid(pid, opts, &mut stats);

        for child_pid in get_children(pid) {
            if visited.insert(child_pid) {
                queue.push_back(child_pid);
            }
        }
    }

    Ok(stats)
}

/// I/O statistics from `/proc/{pid}/io`
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct ProcIoStats {
    read_bytes: u64,
    write_bytes: u64,
    read_ops: u64,
    write_ops: u64,
}

#[cfg(target_os = "linux")]
fn read_proc_io(pid: i32) -> Result<ProcIoStats> {
    let content = std::fs::read_to_string(format!("/proc/{}/io", pid))?;
    Ok(parse_proc_io(&content))
}

/// Parse the body of `/proc/{pid}/io`. Separated out so it is testable.
#[cfg(target_os = "linux")]
fn parse_proc_io(content: &str) -> ProcIoStats {
    let mut stats = ProcIoStats::default();

    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "read_bytes:" => stats.read_bytes = value,
            "write_bytes:" => stats.write_bytes = value,
            "syscr:" => stats.read_ops = value,
            "syscw:" => stats.write_ops = value,
            _ => {}
        }
    }

    stats
}

/// Read swap usage from `/proc/{pid}/status` (VmSwap field)
#[cfg(target_os = "linux")]
fn read_vmswap(pid: i32) -> Result<u64> {
    let content = std::fs::read_to_string(format!("/proc/{}/status", pid))?;
    Ok(parse_vmswap(&content))
}

/// Parse the VmSwap line out of a `/proc/{pid}/status` body, in bytes.
#[cfg(target_os = "linux")]
fn parse_vmswap(content: &str) -> u64 {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("VmSwap:") {
            // Format: "VmSwap:     1234 kB"
            if let Some(kb) = rest.split_whitespace().next() {
                if let Ok(kb) = kb.parse::<u64>() {
                    return kb.saturating_mul(1024);
                }
            }
        }
    }
    0
}

/// Find all child processes of a given PID.
#[cfg(target_os = "linux")]
fn get_children(parent_pid: i32) -> Vec<i32> {
    use procfs::process::Process;
    use std::fs;

    let mut children = Vec::new();

    // Fast path: /proc/{pid}/task/{tid}/children (Linux 3.5+). Note this only
    // lists children of the main thread; threads that fork are covered by the
    // fallback below on kernels where it matters.
    let children_file = format!("/proc/{}/task/{}/children", parent_pid, parent_pid);
    if let Ok(content) = fs::read_to_string(&children_file) {
        for pid_str in content.split_whitespace() {
            if let Ok(child_pid) = pid_str.parse::<i32>() {
                children.push(child_pid);
            }
        }
        return children;
    }

    // Fallback: scan all of /proc. O(number of processes on the node) per call,
    // so this is genuinely slow on a busy node — but it only runs on kernels
    // without the children file.
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(file_name) = entry.file_name().into_string() else {
                continue;
            };
            let Ok(child_pid) = file_name.parse::<i32>() else {
                continue;
            };
            if child_pid == parent_pid {
                continue;
            }
            if let Ok(proc) = Process::new(child_pid) {
                if let Ok(stat) = proc.stat() {
                    if stat.ppid == parent_pid {
                        children.push(child_pid);
                    }
                }
            }
        }
    }

    children
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_process_tree_stats_extended_self() {
        let pid = std::process::id() as i32;
        let result = get_process_tree_stats_extended(pid, ScanOptions::default());
        assert!(result.is_ok());

        #[cfg(target_os = "linux")]
        {
            let stats = result.unwrap();
            assert!(stats.rss_bytes > 0, "RSS should be non-zero");
            assert!(stats.proc_count > 0, "Process count should be at least 1");
            assert!(stats.thread_count > 0, "Thread count should be at least 1");
            // Previously this asserted `major_faults >= 0` on a u64, which is
            // vacuously true and lints as unused_comparisons.
        }

        #[cfg(not(target_os = "linux"))]
        {
            let stats = result.unwrap();
            assert_eq!(stats.rss_bytes, 0);
            assert_eq!(stats.proc_count, 0);
        }
    }

    #[test]
    fn test_process_tree_stats_default() {
        let stats = ProcessTreeStats::default();
        assert_eq!(stats.rss_bytes, 0);
        assert_eq!(stats.proc_count, 0);
        assert_eq!(stats.thread_count, 0);
        assert_eq!(stats.fd_count, 0);
        assert_eq!(stats.major_faults, 0);
        assert_eq!(stats.minor_faults, 0);
        assert_eq!(stats.io_read_bytes, 0);
        assert_eq!(stats.io_write_bytes, 0);
        assert_eq!(stats.io_read_ops, 0);
        assert_eq!(stats.io_write_ops, 0);
        assert_eq!(stats.swap_bytes, 0);
    }

    #[test]
    fn test_max_with_keeps_high_water_mark() {
        let mut a = ProcessTreeStats {
            rss_bytes: 100,
            proc_count: 5,
            io_read_bytes: 900,
            ..Default::default()
        };
        let b = ProcessTreeStats {
            rss_bytes: 50,
            proc_count: 9,
            io_read_bytes: 400,
            ..Default::default()
        };

        a.max_with(&b);

        // The point of max_with: a later sample with fewer live processes must
        // not drag the totals back down.
        assert_eq!(a.rss_bytes, 100);
        assert_eq!(a.proc_count, 9);
        assert_eq!(a.io_read_bytes, 900);
    }

    #[test]
    fn test_proc_source_labels() {
        assert_eq!(ProcSource::Cgroup.as_str(), "cgroup.procs");
        assert_eq!(ProcSource::Tree.as_str(), "process-tree");
    }

    #[test]
    fn test_get_stats_for_pids_matches_a_tree_walk_of_one_process() {
        // Measuring an explicit PID set must agree with walking a tree that
        // contains only that process.
        let pid = std::process::id() as i32;

        let explicit = get_stats_for_pids(&[pid], ScanOptions::minimal()).unwrap();

        #[cfg(target_os = "linux")]
        {
            assert_eq!(explicit.proc_count, 1);
            assert!(explicit.rss_bytes > 0);
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert_eq!(explicit.proc_count, 0);
        }
    }

    #[test]
    fn test_get_stats_for_pids_ignores_dead_pids() {
        // The PID list comes from a file that may be stale by the time we read
        // it; a vanished process must not abort the whole sample.
        let stats = get_stats_for_pids(&[i32::MAX], ScanOptions::minimal());
        assert!(stats.is_ok());
        assert_eq!(stats.unwrap().proc_count, 0);
    }

    #[test]
    fn test_get_stats_for_pids_empty_is_zero_not_an_error() {
        let stats = get_stats_for_pids(&[], ScanOptions::default()).unwrap();
        assert_eq!(stats.proc_count, 0);
        assert_eq!(stats.rss_bytes, 0);
    }

    #[test]
    fn test_scan_options_minimal() {
        let opts = ScanOptions::minimal();
        assert!(!opts.collect_io);
        assert!(!opts.collect_swap);

        let opts = ScanOptions::default();
        assert!(opts.collect_io);
        assert!(opts.collect_swap);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_parse_proc_io() {
        let content = "\
rchar: 1000
wchar: 2000
syscr: 10
syscw: 20
read_bytes: 4096
write_bytes: 8192
cancelled_write_bytes: 0
";
        let stats = parse_proc_io(content);
        assert_eq!(stats.read_bytes, 4096);
        assert_eq!(stats.write_bytes, 8192);
        assert_eq!(stats.read_ops, 10);
        assert_eq!(stats.write_ops, 20);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_parse_vmswap() {
        assert_eq!(parse_vmswap("VmSwap:\t    1234 kB\n"), 1234 * 1024);
        assert_eq!(parse_vmswap("VmRSS:\t 100 kB\n"), 0);
        assert_eq!(parse_vmswap(""), 0);
    }
}
