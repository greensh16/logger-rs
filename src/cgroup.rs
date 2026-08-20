//! cgroup discovery and metric collection.
//!
//! Supports both cgroup **v1** (what Gadi runs today) and cgroup **v2** /
//! unified (what a RHEL 9 refresh would bring). Previously this module was
//! v1-only and would exit at startup on a v2 host, silently killing telemetry
//! for every job on the machine.
//!
//! All of the actual parsing is done by free functions that take `&str`, so they
//! are unit-testable on a macOS development machine without a Linux host or any
//! fixture files. Only the thin `read_*` wrappers touch the filesystem.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Which cgroup hierarchy the kernel is exposing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupVersion {
    V1,
    V2,
}

impl CgroupVersion {
    pub fn as_str(&self) -> &'static str {
        match self {
            CgroupVersion::V1 => "cgroup-v1",
            CgroupVersion::V2 => "cgroup-v2",
        }
    }
}

/// CPU time consumed by the cgroup.
#[derive(Debug, Clone, Default)]
pub struct CpuUsage {
    /// Total CPU time across all cores, nanoseconds.
    pub total_ns: u64,
    /// Per-CPU breakdown, nanoseconds. Empty on cgroup v2, which does not
    /// expose a per-CPU breakdown at all.
    pub percpu_ns: Vec<u64>,
}

/// Memory accounting straight from the kernel. Unlike summing RSS across the
/// process tree, this does not double-count shared pages.
#[derive(Debug, Clone, Default)]
pub struct CgroupMemory {
    pub current_bytes: u64,
    /// Kernel-maintained high-water mark. Free of sampling error entirely —
    /// it catches spikes between our ticks. `None` if the kernel does not
    /// expose it (cgroup v2 before 5.19).
    pub peak_bytes: Option<u64>,
    pub swap_bytes: Option<u64>,
    /// The cgroup's memory limit — what the scheduler booked for this job **on
    /// this node**.
    ///
    /// This is the reliable source for booked memory. Gadi exports neither
    /// `PBS_RESOURCE_LIST_mem` nor `PBS_RESOURCE_mem`, so the environment gives
    /// the logger nothing and memory could only be reported in absolute GB
    /// rather than as a fraction of what was asked for. The kernel has known
    /// all along.
    ///
    /// `None` when the cgroup is unlimited.
    pub limit_bytes: Option<u64>,
}

/// Block I/O accounting from the cgroup. Job-scoped and, crucially, survives
/// the exit of individual processes — unlike `/proc/{pid}/io`.
#[derive(Debug, Clone, Default)]
pub struct CgroupIo {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ops: u64,
    pub write_ops: u64,
}

/// A discovered cgroup, with resolved paths for each controller we care about.
#[derive(Debug, Clone)]
pub struct Cgroup {
    pub version: CgroupVersion,
    /// The PID whose cgroup this is — the job's root process, not the logger's.
    /// Carried so the `allowed_cpus` fallbacks read the job's `/proc` entries
    /// rather than falling back to `/proc/self` and reintroducing exactly the
    /// mismatch that reading `/proc/{tree_pid}/cgroup` exists to avoid.
    pub pid: i32,
    /// Directory holding cpu accounting (`cpuacct.*` on v1, `cpu.stat` on v2).
    pub cpu_path: PathBuf,
    pub memory_path: Option<PathBuf>,
    pub io_path: Option<PathBuf>,
    pub cpuset_path: Option<PathBuf>,
    /// The cgroup's path relative to its mount, e.g. `/pbs_jobs.service/jobid/12345`.
    /// Empty or `/` means the root cgroup — i.e. the whole node.
    pub rel_path: String,
    /// Where the path came from, recorded in the summary for debugging.
    pub source: String,
}

impl Cgroup {
    /// Find the cgroup belonging to this job.
    ///
    /// Two candidates are considered: the logger's own cgroup and `tree_pid`'s.
    /// Neither is reliable on its own.
    ///
    /// - On a **remote node** the logger is launched by `pbsdsh`/`srun`, which
    ///   place it inside the job's cgroup, so `/proc/self` is right. There is no
    ///   job process tree to follow there, and the `--tree-pid 1` the wrapper
    ///   used to pass resolves to the **root** cgroup — which would have
    ///   silently reported the entire node's CPU and memory as the job's.
    /// - On the **head node** both agree, since the wrapper is the job script.
    /// - If the logger is ever started from outside the job (say over ssh),
    ///   only `tree_pid` points at the job.
    ///
    /// So: reject the root cgroup, prefer whichever path names the job, then
    /// prefer the more specific one.
    pub fn discover(tree_pid: i32, job_id: &str) -> Result<Self> {
        let mut candidates: Vec<CgroupCandidate> = Vec::new();

        for (label, pid_path) in [
            ("self", "/proc/self/cgroup".to_string()),
            ("tree_pid", format!("/proc/{}/cgroup", tree_pid)),
        ] {
            if let Ok(content) = fs::read_to_string(&pid_path) {
                candidates.push(CgroupCandidate {
                    source: format!("{} ({})", label, pid_path),
                    content,
                });
            }
        }

        if candidates.is_empty() {
            anyhow::bail!(
                "could not read any cgroup file (tried /proc/self/cgroup and \
                 /proc/{}/cgroup). Is this a Linux host?",
                tree_pid
            );
        }

        let ranked = rank_candidates(&candidates, job_id);

        let mut last_error = None;
        for candidate in &ranked {
            match Self::from_cgroup_file(&candidate.content, tree_pid, &candidate.source) {
                Ok(cg) => return Ok(cg),
                Err(e) => last_error = Some(e),
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no usable cgroup"))).context(
            "Could not locate a usable cgroup. Neither cgroup v1 (cpuacct) nor \
             cgroup v2 (cpu.stat) was found. If this host uses the cgroup v2 \
             unified hierarchy, check that /sys/fs/cgroup is mounted as cgroup2 \
             and that the cpu controller is enabled.",
        )
    }

    /// Use an operator-supplied cgroup directory, bypassing discovery entirely.
    pub fn from_explicit_path(dir: &Path, tree_pid: i32) -> Result<Self> {
        if !dir.exists() {
            anyhow::bail!("--cgroup path {:?} does not exist", dir);
        }

        let version = if dir.join("cpu.stat").exists() && !dir.join("cpuacct.usage_percpu").exists()
        {
            CgroupVersion::V2
        } else {
            CgroupVersion::V1
        };

        Ok(Self {
            version,
            pid: tree_pid,
            cpu_path: dir.to_path_buf(),
            memory_path: dir
                .join(match version {
                    CgroupVersion::V1 => "memory.usage_in_bytes",
                    CgroupVersion::V2 => "memory.current",
                })
                .exists()
                .then(|| dir.to_path_buf()),
            io_path: dir
                .join(match version {
                    CgroupVersion::V1 => "blkio.throttle.io_service_bytes",
                    CgroupVersion::V2 => "io.stat",
                })
                .exists()
                .then(|| dir.to_path_buf()),
            cpuset_path: dir
                .join(match version {
                    CgroupVersion::V1 => "cpuset.cpus",
                    CgroupVersion::V2 => "cpuset.cpus.effective",
                })
                .exists()
                .then(|| dir.to_path_buf()),
            rel_path: dir.display().to_string(),
            source: "--cgroup".to_string(),
        })
    }

    fn from_cgroup_file(content: &str, pid: i32, source: &str) -> Result<Self> {
        // Prefer v2 when the unified hierarchy is actually mounted. On a hybrid
        // host both exist; v1 controllers carry the accounting we want, so only
        // take the v2 path when v1 has no cpuacct.
        let v2_rel = parse_cgroup_file_v2(content);
        let v1_rel = parse_cgroup_file_v1(content, "cpuacct");

        if v1_rel.is_some() {
            if let Ok(cg) = Self::discover_v1(content, pid, source) {
                return Ok(cg);
            }
        }

        if let Some(rel) = v2_rel {
            if let Ok(cg) = Self::discover_v2(&rel, pid, source) {
                return Ok(cg);
            }
        }

        Self::discover_v1(content, pid, source)
    }

    /// True when this cgroup is the root, i.e. describes the whole node.
    ///
    /// Enumerating processes from a root cgroup would count every process on the
    /// machine as belonging to the job, so the caller must not do that.
    pub fn is_root(&self) -> bool {
        let trimmed = self.rel_path.trim_matches('/');
        trimmed.is_empty()
    }

    fn discover_v1(cgroup_file: &str, pid: i32, source: &str) -> Result<Self> {
        let mounts = cgroup_v1_mounts();

        let cpu_rel = parse_cgroup_file_v1(cgroup_file, "cpuacct")
            .ok_or_else(|| anyhow::anyhow!("no cpuacct entry in cgroup file"))?;

        let cpu_path = resolve_v1_controller(&mounts, "cpuacct", &cpu_rel, "cpuacct.usage_percpu")
            .ok_or_else(|| anyhow::anyhow!("cpuacct controller not mounted or not populated"))?;

        // Only accept a controller that is actually delegated to *this job*.
        //
        // Gadi does not delegate blkio: the job's own cgroup file puts it at
        // `/system.slice/pbs.service`, the PBS daemon's cgroup. Reading it gave
        // the daemon's lifetime I/O across every job on the node — two nodes
        // running an identical workload reported 200 GB and 340 GB written for
        // 800 MB of actual work. Reporting no data is far better than reporting
        // someone else's, and it lets the per-process fallback take over.
        let controller_path = |name: &str, probe: &str| -> Option<PathBuf> {
            let rel = parse_cgroup_file_v1(cgroup_file, name)?;
            if !controller_is_job_scoped(&rel, &cpu_rel) {
                eprintln!(
                    "NOTE: cgroup controller '{}' is at {} but this job is at {}; \
                     it is not delegated to the job, so it will not be used.",
                    name, rel, cpu_rel
                );
                return None;
            }
            resolve_v1_controller(&mounts, name, &rel, probe)
        };

        let memory_path = controller_path("memory", "memory.usage_in_bytes");
        let io_path = controller_path("blkio", "blkio.throttle.io_service_bytes");
        let cpuset_path = controller_path("cpuset", "cpuset.cpus");

        Ok(Self {
            version: CgroupVersion::V1,
            pid,
            cpu_path,
            memory_path,
            io_path,
            cpuset_path,
            rel_path: cpu_rel,
            source: source.to_string(),
        })
    }

    fn discover_v2(rel: &str, pid: i32, source: &str) -> Result<Self> {
        let root =
            cgroup_v2_root().ok_or_else(|| anyhow::anyhow!("no cgroup2 filesystem mounted"))?;

        let dir = walk_up_to_probe(&root, rel, "cpu.stat")
            .ok_or_else(|| anyhow::anyhow!("cpu.stat not found under the unified hierarchy"))?;

        Ok(Self {
            version: CgroupVersion::V2,
            pid,
            cpu_path: dir.clone(),
            memory_path: dir.join("memory.current").exists().then(|| dir.clone()),
            io_path: dir.join("io.stat").exists().then(|| dir.clone()),
            cpuset_path: dir
                .join("cpuset.cpus.effective")
                .exists()
                .then(|| dir.clone()),
            rel_path: rel.to_string(),
            source: source.to_string(),
        })
    }

    /// The PIDs the kernel says belong to this cgroup.
    ///
    /// This is the authoritative membership list, and it is strictly better than
    /// walking the process tree from a root PID:
    ///
    /// - It works on a node where the job's processes are not descendants of
    ///   anything the logger started, which is every node but the head one.
    /// - It catches processes that reparent to init, which a tree walk loses.
    /// - It cannot pick up a neighbour's processes on a shared node.
    /// - It is cheaper: one read instead of a per-process `children` walk.
    ///
    /// Child cgroups are included, since some schedulers nest per-task cgroups
    /// beneath the job's.
    pub fn procs(&self) -> Result<Vec<i32>> {
        if self.is_root() {
            anyhow::bail!(
                "refusing to enumerate processes from the root cgroup: every process on the \
                 node would be counted as part of this job"
            );
        }

        let mut pids = Vec::new();
        collect_procs(&self.cpu_path, 0, &mut pids);

        if pids.is_empty() {
            anyhow::bail!("cgroup {:?} lists no processes", self.cpu_path);
        }

        pids.sort_unstable();
        pids.dedup();
        Ok(pids)
    }

    /// Read current CPU usage for the cgroup.
    pub fn read_cpu_usage(&self) -> Result<CpuUsage> {
        match self.version {
            CgroupVersion::V1 => {
                let percpu_file = self.cpu_path.join("cpuacct.usage_percpu");
                let content = fs::read_to_string(&percpu_file)
                    .with_context(|| format!("Failed to read {:?}", percpu_file))?;
                let percpu_ns = parse_percpu_ns(&content);

                if percpu_ns.is_empty() {
                    anyhow::bail!("No CPU usage data in {:?}", percpu_file);
                }

                // cpuacct.usage is authoritative for the total; fall back to the
                // sum of the per-CPU values if it is unreadable.
                let total_ns = fs::read_to_string(self.cpu_path.join("cpuacct.usage"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .unwrap_or_else(|| percpu_ns.iter().sum());

                Ok(CpuUsage {
                    total_ns,
                    percpu_ns,
                })
            }
            CgroupVersion::V2 => {
                let stat_file = self.cpu_path.join("cpu.stat");
                let content = fs::read_to_string(&stat_file)
                    .with_context(|| format!("Failed to read {:?}", stat_file))?;
                let total_ns = parse_cpu_stat_usage_ns(&content)
                    .ok_or_else(|| anyhow::anyhow!("no usage_usec in {:?}", stat_file))?;

                Ok(CpuUsage {
                    total_ns,
                    percpu_ns: Vec::new(),
                })
            }
        }
    }

    /// Read memory accounting. Returns `None` if the memory controller is not
    /// available, in which case the caller should fall back to summed RSS.
    pub fn read_memory(&self) -> Option<CgroupMemory> {
        let dir = self.memory_path.as_ref()?;

        match self.version {
            CgroupVersion::V1 => {
                let current_bytes = read_u64_file(&dir.join("memory.usage_in_bytes"))?;
                let peak_bytes = read_u64_file(&dir.join("memory.max_usage_in_bytes"));
                // memsw counts memory + swap, so swap alone is the difference.
                let swap_bytes = read_u64_file(&dir.join("memory.memsw.usage_in_bytes"))
                    .map(|memsw| memsw.saturating_sub(current_bytes));
                let limit_bytes = fs::read_to_string(dir.join("memory.limit_in_bytes"))
                    .ok()
                    .and_then(|s| parse_mem_limit(&s));

                Some(CgroupMemory {
                    current_bytes,
                    peak_bytes,
                    swap_bytes,
                    limit_bytes,
                })
            }
            CgroupVersion::V2 => {
                let current_bytes = read_u64_file(&dir.join("memory.current"))?;
                // memory.peak only exists on kernel 5.19+.
                let peak_bytes = read_u64_file(&dir.join("memory.peak"));
                let swap_bytes = read_u64_file(&dir.join("memory.swap.current"));
                let limit_bytes = fs::read_to_string(dir.join("memory.max"))
                    .ok()
                    .and_then(|s| parse_mem_limit(&s));

                Some(CgroupMemory {
                    current_bytes,
                    peak_bytes,
                    swap_bytes,
                    limit_bytes,
                })
            }
        }
    }

    /// Read block I/O accounting. Returns `None` when the controller is absent
    /// or reporting nothing.
    ///
    /// The empty-file case matters: on cgroup v2 `io.stat` exists but is empty
    /// whenever the `io` controller has not been enabled in the parent's
    /// `cgroup.subtree_control`. Parsing that as a valid all-zero reading would
    /// tell the dashboard "this job did no I/O", which is a much stronger claim
    /// than "the kernel is not accounting I/O for this job". `None` lets the
    /// dashboard show the difference.
    pub fn read_io(&self) -> Option<CgroupIo> {
        let dir = self.io_path.as_ref()?;

        match self.version {
            CgroupVersion::V1 => {
                let bytes = fs::read_to_string(dir.join("blkio.throttle.io_service_bytes")).ok()?;
                if bytes.trim().is_empty() {
                    return None;
                }

                let (read_bytes, write_bytes) = parse_blkio_two_col(&bytes);
                let (read_ops, write_ops) =
                    fs::read_to_string(dir.join("blkio.throttle.io_serviced"))
                        .ok()
                        .map(|s| parse_blkio_two_col(&s))
                        .unwrap_or((0, 0));

                Some(CgroupIo {
                    read_bytes,
                    write_bytes,
                    read_ops,
                    write_ops,
                })
            }
            CgroupVersion::V2 => {
                let content = fs::read_to_string(dir.join("io.stat")).ok()?;
                if content.trim().is_empty() {
                    return None;
                }
                Some(parse_io_stat_v2(&content))
            }
        }
    }

    /// Determine the CPUs this job is allowed to run on, with several
    /// fallbacks. Returns the CPU ids and a human-readable description of which
    /// source won, which is recorded in the summary for debugging.
    pub fn allowed_cpus(&self) -> (Vec<u32>, String) {
        // Strategy 1: the cpuset alongside our own cgroup.
        if let Some(dir) = &self.cpuset_path {
            let file = match self.version {
                CgroupVersion::V1 => dir.join("cpuset.cpus"),
                CgroupVersion::V2 => dir.join("cpuset.cpus.effective"),
            };
            if let Ok(content) = fs::read_to_string(&file) {
                if let Ok(cpus) = parse_cpuset_list(&content) {
                    return (cpus, file.display().to_string());
                }
            }
        }

        // Strategy 2: the job's /proc/{pid}/cpuset (v1 only, but harmless to try).
        if let Ok((cpus, src)) = allowed_cpus_via_proc_cpuset(self.pid) {
            return (cpus, src);
        }

        // Strategy 3: Cpus_allowed_list from the job's /proc/{pid}/status.
        if let Ok((cpus, src)) = allowed_cpus_via_proc_status(self.pid) {
            return (cpus, src);
        }

        // Strategy 4: ask the scheduler directly.
        if let Ok(cpus) = allowed_cpus_via_sched_getaffinity(self.pid) {
            return (cpus, "sched_getaffinity".to_string());
        }

        // Fallback: assume the whole node.
        let n = num_cpus::get();
        (
            (0..n as u32).collect(),
            "logical_cpu_count_fallback".to_string(),
        )
    }
}

// ---------------------------------------------------------------------------
// Choosing which cgroup is the job's
// ---------------------------------------------------------------------------

/// One `/proc/<pid>/cgroup` body under consideration.
#[derive(Debug, Clone)]
pub struct CgroupCandidate {
    pub source: String,
    pub content: String,
}

/// Order candidates most-likely-to-be-the-job first.
///
/// 1. Candidates resolving to the root cgroup go last: root means "the whole
///    node", which is never the answer for a job and is what `--tree-pid 1` on a
///    remote node produced.
/// 2. A path naming the job wins. PBS writes `/pbs_jobs.service/jobid/12345...`
///    and Slurm `/slurm/uid_1000/job_12345`, so the job's numeric id appearing
///    in the path is strong evidence.
/// 3. Otherwise the deeper path wins, being the more specific of the two.
pub fn rank_candidates(candidates: &[CgroupCandidate], job_id: &str) -> Vec<CgroupCandidate> {
    let key = job_id_key(job_id);

    let mut scored: Vec<(i32, usize, usize, &CgroupCandidate)> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let rel = representative_rel_path(&c.content);
            let is_root = rel.trim_matches('/').is_empty();
            let names_job = !key.is_empty() && rel.contains(&key);

            // Lower sorts first.
            let bucket = if is_root {
                2
            } else if names_job {
                0
            } else {
                1
            };
            let depth = rel
                .trim_matches('/')
                .split('/')
                .filter(|s| !s.is_empty())
                .count();
            (bucket, usize::MAX - depth, i, c)
        })
        .collect();

    scored.sort_by_key(|(bucket, inv_depth, i, _)| (*bucket, *inv_depth, *i));
    scored.into_iter().map(|(_, _, _, c)| c.clone()).collect()
}

/// The path we judge a candidate by: the v1 cpuacct entry, else the v2 entry.
fn representative_rel_path(content: &str) -> String {
    parse_cgroup_file_v1(content, "cpuacct")
        .or_else(|| parse_cgroup_file_v1(content, "cpu"))
        .or_else(|| parse_cgroup_file_v2(content))
        .unwrap_or_default()
}

/// Whether a controller's cgroup path belongs to the same job as `job_rel`.
///
/// Equal paths are the normal case. One being an ancestor of the other is
/// accepted too, since a site may nest a controller a level deeper or shallower.
/// Anything else — most importantly a controller sitting in a service's cgroup
/// rather than the job's — is a different cgroup and must not be read.
///
/// The root cgroup is never job-scoped: it is the whole node.
pub fn controller_is_job_scoped(controller_rel: &str, job_rel: &str) -> bool {
    let c = controller_rel.trim_matches('/');
    let j = job_rel.trim_matches('/');

    if c.is_empty() || j.is_empty() {
        return false;
    }

    c == j || c.starts_with(&format!("{}/", j)) || j.starts_with(&format!("{}/", c))
}

/// The distinctive part of a job id, for matching against cgroup paths.
///
/// `12345.gadi-pbs` -> `12345`, which is what appears in both PBS and Slurm
/// cgroup paths. Falls back to the whole string when there are no digits.
pub fn job_id_key(job_id: &str) -> String {
    let digits: String = job_id.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        job_id.trim().to_string()
    } else {
        digits
    }
}

/// Read `cgroup.procs` here and in every child cgroup.
fn collect_procs(dir: &Path, depth: usize, out: &mut Vec<i32>) {
    // Deep enough for any real scheduler layout, shallow enough that a symlink
    // loop cannot spin forever.
    const MAX_DEPTH: usize = 8;

    if let Ok(content) = fs::read_to_string(dir.join("cgroup.procs")) {
        out.extend(parse_pid_list(&content));
    }

    if depth >= MAX_DEPTH {
        return;
    }

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            // Do not follow symlinks; is_dir() on the entry's own file type.
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                collect_procs(&entry.path(), depth + 1, out);
            }
        }
    }
}

/// Parse a cgroup memory limit, returning `None` when it means "unlimited".
///
/// The two hierarchies say unlimited differently, and both have to be caught —
/// reporting a job as having booked 8 exabytes would be worse than reporting
/// nothing:
///
/// - **v2** writes the literal string `max`.
/// - **v1** writes a sentinel close to `u64::MAX` rounded down to a page
///   boundary (`0x7FFFFFFFFFFFF000` on 64-bit). Rather than match that exact
///   constant, which varies with page size and kernel, anything implausibly
///   large for real hardware is treated as unlimited.
pub fn parse_mem_limit(content: &str) -> Option<u64> {
    let trimmed = content.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("max") {
        return None;
    }

    let value: u64 = trimmed.parse().ok()?;
    if value == 0 {
        return None;
    }

    // 1 PiB. No node has this much RAM, so anything at or above it is a
    // sentinel rather than a real booking.
    const IMPLAUSIBLE: u64 = 1 << 50;
    if value >= IMPLAUSIBLE {
        return None;
    }

    Some(value)
}

/// Parse a newline-separated PID list, as written by `cgroup.procs`.
pub fn parse_pid_list(content: &str) -> Vec<i32> {
    content
        .split_whitespace()
        .filter_map(|s| s.parse::<i32>().ok())
        .filter(|pid| *pid > 0)
        .collect()
}

// ---------------------------------------------------------------------------
// Mount and path resolution
// ---------------------------------------------------------------------------

/// Controller name -> mount point, parsed from `/proc/mounts`.
///
/// Parsing the real mount table is more robust than the previous hardcoded list
/// of three candidate paths, which missed any site that mounts controllers
/// somewhere non-standard.
fn cgroup_v1_mounts() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();

    if let Ok(content) = fs::read_to_string("/proc/mounts") {
        for line in content.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 4 || fields[2] != "cgroup" {
                continue;
            }
            let mount = PathBuf::from(fields[1]);
            for opt in fields[3].split(',') {
                if matches!(opt, "cpuacct" | "cpu" | "cpuset" | "memory" | "blkio") {
                    out.push((opt.to_string(), mount.clone()));
                }
            }
        }
    }

    // Fall back to the conventional layout if /proc/mounts was unhelpful.
    if out.is_empty() {
        for (controller, path) in [
            ("cpuacct", "/sys/fs/cgroup/cpuacct"),
            ("cpuacct", "/sys/fs/cgroup/cpu,cpuacct"),
            ("cpuacct", "/sys/fs/cgroup/cpuacct,cpu"),
            ("cpuset", "/sys/fs/cgroup/cpuset"),
            ("memory", "/sys/fs/cgroup/memory"),
            ("blkio", "/sys/fs/cgroup/blkio"),
        ] {
            out.push((controller.to_string(), PathBuf::from(path)));
        }
    }

    out
}

/// Mount point of the cgroup2 unified hierarchy, if any.
fn cgroup_v2_root() -> Option<PathBuf> {
    if let Ok(content) = fs::read_to_string("/proc/mounts") {
        for line in content.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 3 && fields[2] == "cgroup2" {
                return Some(PathBuf::from(fields[1]));
            }
        }
    }

    let conventional = PathBuf::from("/sys/fs/cgroup");
    conventional
        .join("cgroup.controllers")
        .exists()
        .then_some(conventional)
}

/// Find the deepest directory at or above `root/rel` that contains `probe`.
///
/// Walking upwards matters because a job's cgroup can be nested deeper than the
/// level at which a given controller is actually populated.
fn walk_up_to_probe(root: &Path, rel: &str, probe: &str) -> Option<PathBuf> {
    let mut candidate = root.join(rel.trim_start_matches('/'));

    loop {
        if candidate.join(probe).exists() {
            return Some(candidate);
        }
        if candidate.as_path() == root {
            return None;
        }
        match candidate.parent() {
            Some(parent) => candidate = parent.to_path_buf(),
            None => return None,
        }
    }
}

fn resolve_v1_controller(
    mounts: &[(String, PathBuf)],
    controller: &str,
    rel: &str,
    probe: &str,
) -> Option<PathBuf> {
    for (name, mount) in mounts {
        if name != controller {
            continue;
        }
        if let Some(path) = walk_up_to_probe(mount, rel, probe) {
            return Some(path);
        }
    }
    None
}

fn read_u64_file(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse::<u64>().ok()
}

// ---------------------------------------------------------------------------
// Parsers — pure functions over &str, unit-testable anywhere
// ---------------------------------------------------------------------------

/// Extract the relative path for a v1 controller from a `/proc/{pid}/cgroup`
/// body. Lines look like `4:cpu,cpuacct:/job/12345`.
pub fn parse_cgroup_file_v1(content: &str, controller: &str) -> Option<String> {
    for line in content.lines() {
        let mut parts = line.splitn(3, ':');
        let _hierarchy = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;

        if controllers.split(',').any(|c| c == controller) {
            return Some(path.to_string());
        }
    }
    None
}

/// Extract the unified-hierarchy path. The v2 line always looks like `0::/path`.
pub fn parse_cgroup_file_v2(content: &str) -> Option<String> {
    for line in content.lines() {
        let mut parts = line.splitn(3, ':');
        let hierarchy = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;

        if hierarchy == "0" && controllers.is_empty() {
            return Some(path.to_string());
        }
    }
    None
}

/// Parse whitespace-separated nanosecond counters from `cpuacct.usage_percpu`.
pub fn parse_percpu_ns(content: &str) -> Vec<u64> {
    content
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Pull `usage_usec` out of a cgroup v2 `cpu.stat` and convert to nanoseconds.
pub fn parse_cpu_stat_usage_ns(content: &str) -> Option<u64> {
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some("usage_usec") {
            let usec: u64 = parts.next()?.parse().ok()?;
            return Some(usec.saturating_mul(1_000));
        }
    }
    None
}

/// Sum the Read and Write rows of a v1 blkio two-column file.
///
/// Format is `MAJ:MIN Read 1234` per device, followed by a `Total 5678` line
/// which must be skipped to avoid double-counting.
pub fn parse_blkio_two_col(content: &str) -> (u64, u64) {
    let mut read = 0u64;
    let mut write = 0u64;

    for line in content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Device rows have three fields; the trailing "Total N" row has two.
        if fields.len() != 3 {
            continue;
        }
        let Ok(value) = fields[2].parse::<u64>() else {
            continue;
        };
        match fields[1] {
            "Read" => read = read.saturating_add(value),
            "Write" => write = write.saturating_add(value),
            _ => {}
        }
    }

    (read, write)
}

/// Parse a cgroup v2 `io.stat`, summing across devices.
///
/// Format: `8:0 rbytes=1024 wbytes=2048 rios=10 wios=20 dbytes=0 dios=0`
pub fn parse_io_stat_v2(content: &str) -> CgroupIo {
    let mut io = CgroupIo::default();

    for line in content.lines() {
        for token in line.split_whitespace().skip(1) {
            let Some((key, value)) = token.split_once('=') else {
                continue;
            };
            let Ok(value) = value.parse::<u64>() else {
                continue;
            };
            match key {
                "rbytes" => io.read_bytes = io.read_bytes.saturating_add(value),
                "wbytes" => io.write_bytes = io.write_bytes.saturating_add(value),
                "rios" => io.read_ops = io.read_ops.saturating_add(value),
                "wios" => io.write_ops = io.write_ops.saturating_add(value),
                _ => {}
            }
        }
    }

    io
}

/// Parse cpuset format: `"0-3,8,10-11"` -> `[0,1,2,3,8,10,11]`
pub fn parse_cpuset_list(s: &str) -> Result<Vec<u32>> {
    let mut cpus = Vec::new();

    for part in s.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        if let Some((start, end)) = part.split_once('-') {
            let start: u32 = start.trim().parse()?;
            let end: u32 = end.trim().parse()?;
            if end < start {
                anyhow::bail!("Inverted CPU range: {}", part);
            }
            cpus.extend(start..=end);
        } else {
            cpus.push(part.parse()?);
        }
    }

    cpus.sort_unstable();
    cpus.dedup();

    if cpus.is_empty() {
        anyhow::bail!("Empty CPU list");
    }

    Ok(cpus)
}

// ---------------------------------------------------------------------------
// Allowed-CPU fallbacks
// ---------------------------------------------------------------------------

fn allowed_cpus_via_proc_cpuset(pid: i32) -> Result<(Vec<u32>, String)> {
    let source = format!("/proc/{}/cpuset", pid);
    let cpuset_path = fs::read_to_string(&source)?;
    let rel = cpuset_path.trim().trim_start_matches('/');

    if rel.is_empty() {
        anyhow::bail!("No cpuset in {}", source);
    }

    let cpuset_file = Path::new("/sys/fs/cgroup/cpuset")
        .join(rel)
        .join("cpuset.cpus");
    let content = fs::read_to_string(&cpuset_file)?;
    let cpus = parse_cpuset_list(&content)?;

    Ok((cpus, source))
}

fn allowed_cpus_via_proc_status(pid: i32) -> Result<(Vec<u32>, String)> {
    let source = format!("/proc/{}/status", pid);
    let status = fs::read_to_string(&source)?;

    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Cpus_allowed_list:") {
            let cpus = parse_cpuset_list(rest.trim())?;
            return Ok((cpus, format!("{}:Cpus_allowed_list", source)));
        }
    }

    anyhow::bail!("Cpus_allowed_list not found in {}", source)
}

#[cfg(target_os = "linux")]
fn allowed_cpus_via_sched_getaffinity(pid: i32) -> Result<Vec<u32>> {
    use nix::sched::{sched_getaffinity, CpuSet};
    use nix::unistd::Pid;

    // The job's affinity, not the logger's.
    let cpu_set = sched_getaffinity(Pid::from_raw(pid))?;
    let cpus: Vec<u32> = (0..CpuSet::count())
        .filter(|&i| cpu_set.is_set(i).unwrap_or(false))
        .map(|i| i as u32)
        .collect();

    if cpus.is_empty() {
        anyhow::bail!("Empty CPU set from sched_getaffinity");
    }

    Ok(cpus)
}

#[cfg(not(target_os = "linux"))]
fn allowed_cpus_via_sched_getaffinity(_pid: i32) -> Result<Vec<u32>> {
    anyhow::bail!("sched_getaffinity not available on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cpuset_list() {
        assert_eq!(parse_cpuset_list("0-3").unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpuset_list("0,2,4").unwrap(), vec![0, 2, 4]);
        assert_eq!(
            parse_cpuset_list("0-2,5,8-10").unwrap(),
            vec![0, 1, 2, 5, 8, 9, 10]
        );
        assert_eq!(parse_cpuset_list("10").unwrap(), vec![10]);
        // Trailing newline, as read from sysfs.
        assert_eq!(parse_cpuset_list("0-1\n").unwrap(), vec![0, 1]);
    }

    #[test]
    fn test_parse_cpuset_list_rejects_garbage() {
        assert!(parse_cpuset_list("").is_err());
        assert!(parse_cpuset_list("abc").is_err());
        assert!(parse_cpuset_list("5-1").is_err());
    }

    #[test]
    fn test_parse_cgroup_file_v1() {
        let content = "\
12:pids:/job/123
11:memory:/job/123
4:cpu,cpuacct:/job/123
3:cpuset:/job/123
0::/user.slice/user-1000.slice
";
        assert_eq!(
            parse_cgroup_file_v1(content, "cpuacct").as_deref(),
            Some("/job/123")
        );
        assert_eq!(
            parse_cgroup_file_v1(content, "memory").as_deref(),
            Some("/job/123")
        );
        // Must match a whole comma-separated entry, not a substring: "cpu"
        // should match the "cpu,cpuacct" line, but "acct" should match nothing.
        assert_eq!(
            parse_cgroup_file_v1(content, "cpu").as_deref(),
            Some("/job/123")
        );
        assert_eq!(parse_cgroup_file_v1(content, "acct"), None);
        assert_eq!(parse_cgroup_file_v1(content, "blkio"), None);
    }

    #[test]
    fn test_parse_cgroup_file_v2() {
        let hybrid = "4:cpu,cpuacct:/job/123\n0::/user.slice/user-1000.slice\n";
        assert_eq!(
            parse_cgroup_file_v2(hybrid).as_deref(),
            Some("/user.slice/user-1000.slice")
        );

        let pure_v1 = "4:cpu,cpuacct:/job/123\n";
        assert_eq!(parse_cgroup_file_v2(pure_v1), None);

        let pure_v2 = "0::/\n";
        assert_eq!(parse_cgroup_file_v2(pure_v2).as_deref(), Some("/"));
    }

    #[test]
    fn test_parse_percpu_ns() {
        assert_eq!(parse_percpu_ns("1000 2000 3000\n"), vec![1000, 2000, 3000]);
        assert!(parse_percpu_ns("").is_empty());
    }

    #[test]
    fn test_parse_cpu_stat_usage_ns() {
        let content = "\
usage_usec 1234567
user_usec 1000000
system_usec 234567
nr_periods 0
";
        // microseconds -> nanoseconds
        assert_eq!(parse_cpu_stat_usage_ns(content), Some(1_234_567_000));
        assert_eq!(parse_cpu_stat_usage_ns("user_usec 5\n"), None);
    }

    #[test]
    fn test_parse_blkio_two_col() {
        // Two devices plus the Total row that must not be double-counted.
        let content = "\
8:0 Read 1024
8:0 Write 2048
8:0 Sync 0
8:0 Async 3072
8:16 Read 512
8:16 Write 256
Total 6912
";
        assert_eq!(parse_blkio_two_col(content), (1024 + 512, 2048 + 256));
    }

    #[test]
    fn test_parse_io_stat_v2() {
        let content = "\
8:0 rbytes=1024 wbytes=2048 rios=10 wios=20 dbytes=0 dios=0
8:16 rbytes=512 wbytes=256 rios=5 wios=6 dbytes=0 dios=0
";
        let io = parse_io_stat_v2(content);
        assert_eq!(io.read_bytes, 1536);
        assert_eq!(io.write_bytes, 2304);
        assert_eq!(io.read_ops, 15);
        assert_eq!(io.write_ops, 26);
    }

    #[test]
    fn test_parse_io_stat_v2_tolerates_junk() {
        let io = parse_io_stat_v2("8:0 rbytes=abc wbytes=100 nonsense\n");
        assert_eq!(io.read_bytes, 0);
        assert_eq!(io.write_bytes, 100);
    }

    #[test]
    fn test_walk_up_to_probe_missing() {
        // A probe that cannot exist anywhere under a real directory should walk
        // all the way to the root and give up rather than loop forever.
        let root = Path::new("/tmp");
        assert_eq!(
            walk_up_to_probe(root, "/a/b/c", "definitely-not-a-real-cgroup-file"),
            None
        );
    }

    #[test]
    fn test_cgroup_version_as_str() {
        assert_eq!(CgroupVersion::V1.as_str(), "cgroup-v1");
        assert_eq!(CgroupVersion::V2.as_str(), "cgroup-v2");
    }

    fn candidate(source: &str, content: &str) -> CgroupCandidate {
        CgroupCandidate {
            source: source.to_string(),
            content: content.to_string(),
        }
    }

    const JOB_CGROUP: &str = "12:memory:/pbs_jobs.service/jobid/12345.gadi-pbs\n\
                              4:cpu,cpuacct:/pbs_jobs.service/jobid/12345.gadi-pbs\n";
    const ROOT_CGROUP: &str = "12:memory:/\n4:cpu,cpuacct:/\n";
    const OTHER_CGROUP: &str = "4:cpu,cpuacct:/user.slice/user-1000.slice\n";

    #[test]
    fn test_root_cgroup_never_wins() {
        // This is the bug being fixed: a remote logger launched with
        // --tree-pid 1 resolves to the root cgroup, which would report the
        // entire node's CPU and memory as the job's.
        let ranked = rank_candidates(
            &[
                candidate("tree_pid (/proc/1/cgroup)", ROOT_CGROUP),
                candidate("self (/proc/self/cgroup)", JOB_CGROUP),
            ],
            "12345.gadi-pbs",
        );

        assert!(
            ranked[0].source.starts_with("self"),
            "the job's own cgroup must beat the root cgroup, got {:?}",
            ranked[0].source
        );
        assert!(ranked[1].source.starts_with("tree_pid"));
    }

    #[test]
    fn test_path_naming_the_job_wins() {
        // Logger started from outside the job (e.g. over ssh): its own cgroup is
        // some user slice, and only tree_pid points at the job.
        let ranked = rank_candidates(
            &[
                candidate("self", OTHER_CGROUP),
                candidate("tree_pid", JOB_CGROUP),
            ],
            "12345.gadi-pbs",
        );
        assert_eq!(ranked[0].source, "tree_pid");
    }

    #[test]
    fn test_deeper_path_wins_when_neither_names_the_job() {
        let shallow = "4:cpu,cpuacct:/system.slice\n";
        let deep = "4:cpu,cpuacct:/system.slice/nested/deeper\n";

        let ranked = rank_candidates(
            &[candidate("shallow", shallow), candidate("deep", deep)],
            "no-digits-here",
        );
        assert_eq!(ranked[0].source, "deep");
    }

    #[test]
    fn test_ranking_is_stable_when_both_agree() {
        // Head node: the logger and the job script share a cgroup. Either is
        // correct, but the order must be deterministic.
        let ranked = rank_candidates(
            &[
                candidate("self", JOB_CGROUP),
                candidate("tree_pid", JOB_CGROUP),
            ],
            "12345.gadi-pbs",
        );
        assert_eq!(ranked[0].source, "self");
        assert_eq!(ranked.len(), 2);
    }

    #[test]
    fn test_ranking_handles_slurm_paths() {
        let slurm = "4:cpu,cpuacct:/slurm/uid_1000/job_12345/step_0\n";
        let ranked = rank_candidates(
            &[candidate("root", ROOT_CGROUP), candidate("slurm", slurm)],
            "12345",
        );
        assert_eq!(ranked[0].source, "slurm");
    }

    /// The real /proc/self/cgroup from a Gadi normal-queue job, job 174849168.
    /// Note blkio and pids sit in the PBS daemon's cgroup, not the job's.
    const GADI_REAL: &str = "\
12:pids:/system.slice/pbs.service
11:rdma:/
10:hugetlb:/
9:cpu,cpuacct:/pbspro.service/jobid/174849168.gadi-pbs
8:devices:/pbspro.service/jobid/174849168.gadi-pbs
7:net_cls,net_prio:/pbspro.service/jobid/174849168.gadi-pbs
6:perf_event:/
5:blkio:/system.slice/pbs.service
4:cpuset:/pbspro.service/jobid/174849168.gadi-pbs
3:freezer:/
2:memory:/pbspro.service/jobid/174849168.gadi-pbs
1:name=systemd:/pbspro.service/jobid/174849168.gadi-pbs
";

    #[test]
    fn test_gadi_layout_accepts_memory_and_cpuset_but_rejects_blkio() {
        let job = parse_cgroup_file_v1(GADI_REAL, "cpuacct").unwrap();
        assert_eq!(job, "/pbspro.service/jobid/174849168.gadi-pbs");

        let memory = parse_cgroup_file_v1(GADI_REAL, "memory").unwrap();
        let cpuset = parse_cgroup_file_v1(GADI_REAL, "cpuset").unwrap();
        let blkio = parse_cgroup_file_v1(GADI_REAL, "blkio").unwrap();

        assert!(controller_is_job_scoped(&memory, &job));
        assert!(controller_is_job_scoped(&cpuset, &job));

        // The regression: blkio lives in the PBS daemon's cgroup on Gadi, so
        // reading it reported the daemon's lifetime I/O across every job on the
        // node — 200 GB and 340 GB on two nodes doing 800 MB of real work.
        assert!(
            !controller_is_job_scoped(&blkio, &job),
            "blkio at {blkio} must not be treated as this job's"
        );
    }

    #[test]
    fn test_controller_is_job_scoped() {
        let job = "/pbspro.service/jobid/12345.gadi-pbs";

        assert!(controller_is_job_scoped(job, job));
        // Trailing-slash differences are not real differences.
        assert!(controller_is_job_scoped(
            "pbspro.service/jobid/12345.gadi-pbs",
            job
        ));
        // A level deeper or shallower is still the same job.
        assert!(controller_is_job_scoped(
            "/pbspro.service/jobid/12345.gadi-pbs/task_0",
            job
        ));
        assert!(controller_is_job_scoped(
            "/pbspro.service",
            "/pbspro.service/jobid/12345"
        ));

        // Different subtree entirely.
        assert!(!controller_is_job_scoped("/system.slice/pbs.service", job));
        assert!(!controller_is_job_scoped("/user.slice", job));
        // Root is the whole node, never a job.
        assert!(!controller_is_job_scoped("/", job));
        assert!(!controller_is_job_scoped("", job));
        assert!(!controller_is_job_scoped(job, "/"));
        // A sibling job must not match, despite the shared prefix.
        assert!(!controller_is_job_scoped(
            "/pbspro.service/jobid/99999.gadi-pbs",
            job
        ));
    }

    #[test]
    fn test_gadi_layout_ranks_the_job_cgroup_first() {
        // Confirms the real file also survives candidate ranking.
        let ranked = rank_candidates(
            &[
                candidate("tree_pid", ROOT_CGROUP),
                candidate("self", GADI_REAL),
            ],
            "174849168.gadi-pbs",
        );
        assert_eq!(ranked[0].source, "self");
    }

    #[test]
    fn test_job_id_key() {
        assert_eq!(job_id_key("12345.gadi-pbs"), "12345");
        assert_eq!(job_id_key("12345"), "12345");
        assert_eq!(job_id_key("local_run"), "local_run");
        assert_eq!(job_id_key(""), "");
    }

    #[test]
    fn test_parse_mem_limit() {
        // A real booking.
        assert_eq!(parse_mem_limit("68719476736\n"), Some(68719476736));
        assert_eq!(parse_mem_limit("34359738368"), Some(34359738368));

        // cgroup v2's unlimited.
        assert_eq!(parse_mem_limit("max\n"), None);
        assert_eq!(parse_mem_limit("MAX"), None);

        // cgroup v1's unlimited sentinel — the actual value Gadi's kernel
        // writes for an unlimited cgroup. Reporting this as a booking would
        // claim the job asked for 8 exabytes.
        assert_eq!(parse_mem_limit("9223372036854771712"), None);
        assert_eq!(parse_mem_limit(&u64::MAX.to_string()), None);

        // Junk and edge cases.
        assert_eq!(parse_mem_limit(""), None);
        assert_eq!(parse_mem_limit("   \n"), None);
        assert_eq!(parse_mem_limit("0"), None);
        assert_eq!(parse_mem_limit("not-a-number"), None);
    }

    #[test]
    fn test_parse_mem_limit_boundary() {
        // Just under 1 PiB is implausible-but-accepted; at or above is a
        // sentinel. Worth pinning so the threshold cannot drift silently.
        assert!(parse_mem_limit(&((1u64 << 50) - 1).to_string()).is_some());
        assert_eq!(parse_mem_limit(&(1u64 << 50).to_string()), None);
    }

    #[test]
    fn test_parse_pid_list() {
        assert_eq!(parse_pid_list("101\n202\n303\n"), vec![101, 202, 303]);
        assert_eq!(parse_pid_list(""), Vec::<i32>::new());
        // Junk and non-positive values are dropped rather than poisoning the set.
        assert_eq!(
            parse_pid_list("101\nnonsense\n0\n-5\n202\n"),
            vec![101, 202]
        );
    }

    #[test]
    fn test_is_root() {
        let mut cg = Cgroup {
            version: CgroupVersion::V1,
            pid: 1,
            cpu_path: PathBuf::from("/x"),
            memory_path: None,
            io_path: None,
            cpuset_path: None,
            rel_path: "/".to_string(),
            source: "test".to_string(),
        };
        assert!(cg.is_root());

        cg.rel_path = String::new();
        assert!(cg.is_root());

        cg.rel_path = "/pbs_jobs.service/jobid/12345".to_string();
        assert!(!cg.is_root());
    }

    #[test]
    fn test_procs_refuses_the_root_cgroup() {
        let cg = Cgroup {
            version: CgroupVersion::V1,
            pid: 1,
            cpu_path: PathBuf::from("/sys/fs/cgroup/cpuacct"),
            memory_path: None,
            io_path: None,
            cpuset_path: None,
            rel_path: "/".to_string(),
            source: "test".to_string(),
        };

        let err = cg.procs().unwrap_err().to_string();
        assert!(
            err.contains("root cgroup"),
            "must refuse rather than count the whole node, got: {err}"
        );
    }

    #[test]
    fn test_procs_reads_the_cgroup_and_its_children() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        writeln!(
            std::fs::File::create(root.join("cgroup.procs")).unwrap(),
            "100\n101"
        )
        .unwrap();

        // A nested per-task cgroup, as some schedulers create.
        let child = root.join("task_0");
        std::fs::create_dir(&child).unwrap();
        writeln!(
            std::fs::File::create(child.join("cgroup.procs")).unwrap(),
            "200"
        )
        .unwrap();

        let cg = Cgroup {
            version: CgroupVersion::V1,
            pid: 1,
            cpu_path: root.to_path_buf(),
            memory_path: None,
            io_path: None,
            cpuset_path: None,
            rel_path: "/pbs_jobs.service/jobid/12345".to_string(),
            source: "test".to_string(),
        };

        let mut pids = cg.procs().unwrap();
        pids.sort_unstable();
        assert_eq!(pids, vec![100, 101, 200]);
    }

    #[test]
    fn test_read_io_returns_none_for_an_empty_controller_file() {
        use std::io::Write;

        // cgroup v2 leaves io.stat present but empty when the io controller has
        // not been enabled in the parent's cgroup.subtree_control. Reporting
        // that as an all-zero reading would tell the dashboard "this job did no
        // I/O" when the truth is "the kernel is not accounting I/O here".
        let dir = tempfile::tempdir().unwrap();
        std::fs::File::create(dir.path().join("io.stat")).unwrap();

        let cg = Cgroup {
            version: CgroupVersion::V2,
            pid: 1,
            cpu_path: dir.path().to_path_buf(),
            memory_path: None,
            io_path: Some(dir.path().to_path_buf()),
            cpuset_path: None,
            rel_path: "/job/1".to_string(),
            source: "test".to_string(),
        };
        assert!(cg.read_io().is_none(), "empty io.stat must read as None");

        // A populated file still parses.
        let mut f = std::fs::File::create(dir.path().join("io.stat")).unwrap();
        writeln!(f, "8:0 rbytes=1024 wbytes=2048 rios=10 wios=20").unwrap();
        drop(f);

        let io = cg.read_io().expect("populated io.stat must read as Some");
        assert_eq!(io.read_bytes, 1024);
        assert_eq!(io.write_bytes, 2048);
    }

    #[test]
    fn test_read_io_v1_empty_blkio_is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::File::create(dir.path().join("blkio.throttle.io_service_bytes")).unwrap();

        let cg = Cgroup {
            version: CgroupVersion::V1,
            pid: 1,
            cpu_path: dir.path().to_path_buf(),
            memory_path: None,
            io_path: Some(dir.path().to_path_buf()),
            cpuset_path: None,
            rel_path: "/job/1".to_string(),
            source: "test".to_string(),
        };
        assert!(cg.read_io().is_none());
    }

    #[test]
    fn test_read_io_none_when_controller_absent() {
        let cg = Cgroup {
            version: CgroupVersion::V2,
            pid: 1,
            cpu_path: PathBuf::from("/nonexistent"),
            memory_path: None,
            io_path: None,
            cpuset_path: None,
            rel_path: "/job/1".to_string(),
            source: "test".to_string(),
        };
        assert!(cg.read_io().is_none());
        assert!(cg.read_memory().is_none());
    }
}
