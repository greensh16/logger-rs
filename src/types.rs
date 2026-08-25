//! Wire types for the NDJSON telemetry stream and the summary file.
//!
//! **Schema stability.** These structs are the contract between this binary and
//! the dashboard watcher. New fields are added alongside existing ones rather
//! than replacing them, so an older dashboard keeps working against a newer
//! logger. Fields marked "legacy" below are known to be inaccurate and are
//! retained only until the dashboard has migrated to their replacement:
//!
//! | legacy field            | replacement              | why |
//! |-------------------------|--------------------------|-----|
//! | `rss_bytes_sum`         | `cgroup_mem_bytes`       | summed RSS double-counts shared pages |
//! | `io_*`                  | `cgroup_io_*`            | per-process I/O vanishes when children exit |
//! | `swap_bytes`            | `cgroup_swap_bytes`      | same |
//! | `system_cpu_efficiency` | `cpu_efficiency_pct`     | the old field is a duplicate of `cpu_pct_sum`, not an efficiency |
//!
//! Any change to these structs must be reflected in `tests/schema_contract.rs`,
//! which will fail if a field is renamed or removed.

use serde::{Deserialize, Serialize};

/// A single telemetry sample (one tick)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySample {
    /// Event type: always "job_log" for this struct.
    pub event: String,

    pub user_id: String,
    pub job_id: String,
    pub queue: String,
    pub job_name: String,
    pub project: String,

    /// Short hostname of the node this sample came from.
    ///
    /// A multi-node job runs one logger per node, each writing its own file.
    /// Carrying the hostname in every record means a merged or interleaved view
    /// can still attribute each reading to a specific node, without relying on
    /// the filename.
    #[serde(default)]
    pub hostname: String,

    /// Unix timestamp (epoch seconds with fractional part)
    pub t: f64,

    /// Length of the window this sample covers, in seconds. This is the
    /// *measured* elapsed time, which is always somewhat longer than the
    /// configured interval — consumers integrating rates over time should use
    /// this rather than assuming the nominal interval.
    #[serde(default)]
    pub dt_sec: f64,

    /// Allowed CPU **ids** (from cpuset). Note these are node CPU ids, not
    /// indices into the per-CPU vectors' first N slots.
    pub allowed_cpus: Vec<u32>,

    /// Sum of CPU percentage across the job cgroup. 400% == 4 fully busy cores.
    pub cpu_pct_sum: f64,

    /// Legacy: summed RSS across the process tree, in bytes. Double-counts
    /// shared pages. Prefer `cgroup_mem_bytes`.
    pub rss_bytes_sum: u64,

    pub n_procs: usize,

    /// Per-CPU busy percentage, indexed by **node CPU id**. Empty on cgroup v2,
    /// which exposes no per-CPU breakdown.
    pub system_percpu_pct: Vec<f64>,

    /// Legacy: mirrors `system_percpu_pct`. cgroup cpuacct does not attribute
    /// per-CPU time to individual processes, so a genuine process-tree per-CPU
    /// breakdown is not available from this data source.
    pub tree_percpu_pct: Vec<f64>,

    /// Legacy: an exact duplicate of `cpu_pct_sum`, despite the name.
    /// Prefer `cpu_efficiency_pct`.
    pub system_cpu_efficiency: f64,

    /// CPU used as a percentage of the cores actually booked. 100% means the
    /// job is using its whole allocation; 25% means three quarters is idle.
    #[serde(default)]
    pub cpu_efficiency_pct: f64,

    /// Booked walltime, always normalised to HH:MM:SS.
    pub booked_walltime: String,
    /// Booked walltime in seconds, so consumers need not parse the string.
    #[serde(default)]
    pub booked_walltime_sec: u64,

    /// Booked memory as written by the operator, e.g. "4GB".
    pub booked_mem: String,
    /// Booked memory in bytes, so consumers need not parse the string.
    #[serde(default)]
    pub booked_mem_bytes: u64,

    pub n_threads: usize,
    pub n_open_fds: usize,

    pub major_faults: u64,
    pub minor_faults: u64,

    /// Legacy per-process I/O counters. These are summed over *live* processes,
    /// so they fall when children exit. Prefer the `cgroup_io_*` fields.
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
    pub io_read_ops: u64,
    pub io_write_ops: u64,

    /// Legacy: summed per-process swap. Prefer `cgroup_swap_bytes`.
    pub swap_bytes: u64,

    // --- cgroup-sourced metrics (accurate; None if the controller is absent) ---
    /// Current memory charged to the job's cgroup. No double-counting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_mem_bytes: Option<u64>,

    /// Kernel-maintained memory high-water mark. Catches spikes that occur
    /// between our samples, so it is strictly better than any peak we compute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_mem_peak_bytes: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_swap_bytes: Option<u64>,

    /// Cumulative block I/O for the whole cgroup. Monotonic, and unaffected by
    /// processes coming and going.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_io_read_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_io_write_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_io_read_ops: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_io_write_ops: Option<u64>,

    // Scheduler metadata (for multi-node jobs)
    pub num_nodes: Option<u32>,
    pub tasks_per_node: Option<u32>,
    pub cpus_per_task: Option<u32>,

    /// Network counters, as a **delta over this tick**.
    ///
    /// These are read from /proc/net/dev and are therefore **node-wide**, not
    /// job-scoped: on a shared queue they include other jobs' traffic. See
    /// `net_is_node_scoped`.
    pub net_recv_bytes: u64,
    pub net_sent_bytes: u64,
    pub net_recv_packets: u64,
    pub net_sent_packets: u64,

    /// Always true today: network figures describe the whole node. Present so
    /// the dashboard can label them honestly rather than implying job scope.
    #[serde(default = "default_true")]
    pub net_is_node_scoped: bool,

    /// GPU metrics, one entry per GPU in scope.
    ///
    /// Scope depends on `gpu_is_node_scoped`: when `CUDA_VISIBLE_DEVICES` is set
    /// these are the job's own GPUs, otherwise they cover the whole node.
    pub gpu_utilization: Vec<f64>,
    pub gpu_memory_used: Vec<u64>,
    pub gpu_memory_total: Vec<u64>,
    pub gpu_temperature: Vec<f64>,
    pub gpu_power: Vec<f64>,

    /// Node GPU index for each entry above, so a reading stays tied to a
    /// physical device even after filtering to the job's GPUs.
    #[serde(default)]
    pub gpu_indices: Vec<u32>,

    /// False when the GPU figures were narrowed to this job via
    /// `CUDA_VISIBLE_DEVICES`; true when they describe every GPU on the host.
    #[serde(default = "default_true")]
    pub gpu_is_node_scoped: bool,
}

fn default_true() -> bool {
    true
}

/// Summary statistics written on exit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySummary {
    // Metadata
    pub user_id: String,
    pub job_id: String,
    pub queue: String,
    pub job_name: String,
    pub project: String,

    /// Short hostname of the node this summary describes. Every figure below is
    /// for this node alone; `logger-rs --merge` combines them across nodes.
    #[serde(default)]
    pub hostname: String,
    /// Index of this node within the job's allocation, when known.
    #[serde(default)]
    pub node_rank: Option<u32>,
    /// How many nodes the scheduler allocated, for spotting nodes that never
    /// reported.
    #[serde(default)]
    pub num_nodes_allocated: Option<u32>,
    /// How the job's processes were identified: `cgroup.procs` (the kernel's
    /// membership list, correct on every node) or `process-tree` (descendants of
    /// `tree_pid`, only valid where they really are our descendants).
    #[serde(default)]
    pub proc_source: String,

    // Timing
    pub t_start: String, // ISO 8601
    pub t_end: String,   // ISO 8601
    pub duration_sec: f64,
    pub samples: usize,
    pub interval_sec: f64,
    pub tree_pid: i32,

    // CPU info
    pub allowed_cpus: Vec<u32>,
    pub mode: String,
    /// Which cgroup hierarchy the metrics came from: "cgroup-v1" or "cgroup-v2".
    #[serde(default)]
    pub cgroup_version: String,
    pub cpuacct_path: String,
    pub cpuset_path_used: Option<String>,

    // Peak values
    pub cpu_pct_sum_peak: f64,
    pub rss_bytes_peak: u64,
    pub rss_gb_peak: f64,
    pub n_procs_peak: usize,
    pub n_threads_peak: usize,
    pub n_open_fds_peak: usize,
    pub swap_bytes_peak: u64,

    // cgroup-sourced peaks (accurate)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_mem_peak_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_mem_peak_gb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_swap_peak_bytes: Option<u64>,

    // Aggregate CPU usage
    pub cpu_core_seconds: f64,
    /// Core-hours consumed. Service Units are this multiplied by the queue's
    /// charge rate, which lives in the dashboard's rate table rather than here —
    /// the logger has no way to know the rate for the queue it is running in.
    #[serde(default)]
    pub cpu_core_hours: f64,
    pub avg_cpu_percent_over_run: f64,
    /// Average CPU used as a percentage of the cores booked, over the whole run.
    /// This is the headline "did this job waste its allocation" number.
    #[serde(default)]
    pub cpu_efficiency_pct_avg: f64,
    #[serde(default)]
    pub cpu_efficiency_pct_peak: f64,
    pub cpu_core_seconds_by_cpu: Vec<f64>,
    pub tree_avg_cpu_percent_by_cpu: Vec<f64>,
    pub tree_percpu_pct_peak: Vec<f64>,
    pub system_percpu_pct_peak: Vec<f64>,

    // Page faults
    pub major_faults_total: u64,
    pub minor_faults_total: u64,

    // I/O totals (legacy, per-process derived)
    pub io_read_bytes_total: u64,
    pub io_write_bytes_total: u64,
    pub io_read_ops_total: u64,
    pub io_write_ops_total: u64,

    // I/O totals from the cgroup (accurate)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_io_read_bytes_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_io_write_bytes_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_io_read_ops_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_io_write_ops_total: Option<u64>,

    // Scheduler metadata
    pub num_nodes: Option<u32>,
    pub tasks_per_node: Option<u32>,
    pub cpus_per_task: Option<u32>,

    // Network totals (node-wide, see TelemetrySample)
    pub net_recv_bytes_total: u64,
    pub net_sent_bytes_total: u64,
    pub net_recv_packets_total: u64,
    pub net_sent_packets_total: u64,

    // GPU peaks (per-GPU vectors, node-wide)
    pub gpu_utilization_peak: Vec<f64>,
    pub gpu_memory_used_peak: Vec<u64>,
    /// Total memory per GPU, so utilisation can be computed from the summary
    /// alone rather than needing a sample line.
    #[serde(default)]
    pub gpu_memory_total: Vec<u64>,
    pub gpu_temperature_peak: Vec<f64>,
    pub gpu_power_peak: Vec<f64>,
    /// Node indices of the GPUs these peaks describe.
    #[serde(default)]
    pub gpu_indices: Vec<u32>,
    /// Whether the GPU peaks cover the node or just this job's GPUs.
    #[serde(default)]
    pub gpu_is_node_scoped: bool,

    // Booked resources
    pub booked_walltime: String,
    pub booked_walltime_sec: u64,
    pub booked_mem: String,
    pub booked_mem_bytes: u64,
    pub booked_mem_gb: f64,
    /// Where `booked_mem_bytes` came from: `cgroup-limit`, `pbs-env`, or
    /// `unknown`.
    #[serde(default)]
    pub booked_mem_source: String,
    /// Whether `booked_mem_bytes` describes this node alone or the whole job.
    ///
    /// The cgroup limit is per node and so sums correctly across a multi-node
    /// job; a `-l mem=` booking is the job's total and must not be summed.
    #[serde(default)]
    pub booked_mem_is_per_node: bool,

    /// Fraction of the booked walltime actually used, 0-100.
    #[serde(default)]
    pub walltime_efficiency_pct: f64,
    /// Peak memory as a percentage of booked memory, 0-100. Uses the cgroup
    /// peak when available, else the (inflated) summed-RSS peak.
    #[serde(default)]
    pub mem_efficiency_pct: f64,

    // Outcome
    /// Exit status of the workload, or `None` when it could not be determined.
    /// This used to be hardcoded to 0, which reported every job as successful
    /// and made failure analysis impossible.
    #[serde(default)]
    pub exit_status: Option<i32>,
    #[serde(default)]
    pub exit_reason: String,

    // Notes
    pub notes: SummaryNotes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryNotes {
    pub method: String,
    pub cpu_pct_sum: String,
    pub system_percpu_pct: String,
    pub cpuset_alignment: String,
    #[serde(default)]
    pub memory: String,
    #[serde(default)]
    pub node_scoped_metrics: String,
    #[serde(default)]
    pub legacy_fields: String,
}

impl Default for SummaryNotes {
    fn default() -> Self {
        Self {
            method: "cgroup cpu accounting deltas (ns) → per-CPU seconds → %".to_string(),
            cpu_pct_sum: "Sum over job cgroup; 400% ~ 4 full CPUs on this node.".to_string(),
            system_percpu_pct:
                "Per-CPU deltas over the tick window, indexed by node CPU id. Empty on cgroup v2, \
                 which exposes no per-CPU breakdown."
                    .to_string(),
            cpuset_alignment:
                "allowed_cpus derived from the cpuset controller at the same relative cgroup path."
                    .to_string(),
            memory:
                "cgroup_mem_* comes from the kernel's memory controller and does not double-count \
                 shared pages. rss_bytes_* sums per-process RSS and overstates memory for MPI and \
                 shared-memory codes; it is retained for backwards compatibility only."
                    .to_string(),
            node_scoped_metrics:
                "Network and GPU figures describe the whole node, not just this job. On a shared \
                 queue they include other jobs' activity."
                    .to_string(),
            legacy_fields: "system_cpu_efficiency duplicates cpu_pct_sum; use cpu_efficiency_pct. \
                 tree_percpu_pct mirrors system_percpu_pct; cpuacct cannot attribute per-CPU time \
                 to individual processes."
                .to_string(),
        }
    }
}

impl TelemetrySummary {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        user_id: String,
        job_id: String,
        queue: String,
        job_name: String,
        project: String,
        tree_pid: i32,
        interval_sec: f64,
        allowed_cpus: Vec<u32>,
        cgroup_version: String,
        cpuacct_path: String,
        cpuset_path_used: Option<String>,
        booked_walltime: String,
        booked_walltime_sec: u64,
        booked_mem: String,
        booked_mem_bytes: u64,
    ) -> Self {
        Self {
            user_id,
            job_id,
            queue,
            job_name,
            project,
            // Assigned by the logger once the node is known; kept out of the
            // constructor's already-long argument list.
            hostname: String::new(),
            node_rank: None,
            num_nodes_allocated: None,
            proc_source: String::new(),
            t_start: String::new(),
            t_end: String::new(),
            duration_sec: 0.0,
            samples: 0,
            interval_sec,
            tree_pid,
            allowed_cpus,
            mode: "cgroup".to_string(),
            cgroup_version,
            cpuacct_path,
            cpuset_path_used,
            cpu_pct_sum_peak: 0.0,
            rss_bytes_peak: 0,
            rss_gb_peak: 0.0,
            n_procs_peak: 0,
            n_threads_peak: 0,
            n_open_fds_peak: 0,
            swap_bytes_peak: 0,
            cgroup_mem_peak_bytes: None,
            cgroup_mem_peak_gb: None,
            cgroup_swap_peak_bytes: None,
            cpu_core_seconds: 0.0,
            cpu_core_hours: 0.0,
            avg_cpu_percent_over_run: 0.0,
            cpu_efficiency_pct_avg: 0.0,
            cpu_efficiency_pct_peak: 0.0,
            // These per-CPU vectors are grown on demand to the node's CPU count.
            // They used to be sized to allowed_cpus.len(), which silently
            // discarded every CPU beyond the first N and misattributed the rest.
            cpu_core_seconds_by_cpu: Vec::new(),
            tree_avg_cpu_percent_by_cpu: Vec::new(),
            tree_percpu_pct_peak: Vec::new(),
            system_percpu_pct_peak: Vec::new(),
            major_faults_total: 0,
            minor_faults_total: 0,
            io_read_bytes_total: 0,
            io_write_bytes_total: 0,
            io_read_ops_total: 0,
            io_write_ops_total: 0,
            cgroup_io_read_bytes_total: None,
            cgroup_io_write_bytes_total: None,
            cgroup_io_read_ops_total: None,
            cgroup_io_write_ops_total: None,
            num_nodes: None,
            tasks_per_node: None,
            cpus_per_task: None,
            net_recv_bytes_total: 0,
            net_sent_bytes_total: 0,
            net_recv_packets_total: 0,
            net_sent_packets_total: 0,
            gpu_utilization_peak: Vec::new(),
            gpu_memory_used_peak: Vec::new(),
            gpu_memory_total: Vec::new(),
            gpu_temperature_peak: Vec::new(),
            gpu_power_peak: Vec::new(),
            gpu_indices: Vec::new(),
            gpu_is_node_scoped: true,
            booked_walltime,
            booked_walltime_sec,
            booked_mem,
            booked_mem_bytes,
            booked_mem_gb: booked_mem_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
            booked_mem_source: String::new(),
            booked_mem_is_per_node: false,
            walltime_efficiency_pct: 0.0,
            mem_efficiency_pct: 0.0,
            exit_status: None,
            exit_reason: "unknown".to_string(),
            notes: SummaryNotes::default(),
        }
    }

    /// Peak memory in bytes, preferring the accurate cgroup figure.
    pub fn effective_peak_mem_bytes(&self) -> u64 {
        self.cgroup_mem_peak_bytes.unwrap_or(self.rss_bytes_peak)
    }
}

/// One node's files, as listed in the job manifest.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ManifestEntry {
    pub hostname: String,
    pub node_rank: u32,
    /// NDJSON filename, relative to the manifest's own directory.
    pub log: String,
    /// Per-node summary filename, relative to the manifest's own directory.
    pub summary: String,
}

/// Written once at job start, listing every file the job will produce.
///
/// A multi-node job writes one NDJSON per node, and the dashboard polls them
/// live — so they cannot be concatenated into one file while the job is still
/// running. The manifest gives the poller a single, predictably-named file to
/// find, from which it learns the rest.
///
/// It also states how many nodes are *expected*. A listed file that does not
/// exist yet is a node still starting; one that stops growing while others
/// continue is a node whose logger died. Without the manifest neither is
/// distinguishable from a node that was never part of the job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobManifest {
    pub event: String,
    pub schema: String,

    pub job_id: String,
    pub user_id: String,
    pub queue: String,
    pub job_name: String,
    pub project: String,

    /// When the manifest was written, ISO 8601.
    pub created: String,
    pub logger_version: String,

    pub nodes_expected: usize,
    pub files: Vec<ManifestEntry>,

    /// Filename the job-level merged summary will have once the job ends.
    pub merged_summary: String,
}

/// One node's contribution to a multi-node job.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeSummary {
    pub hostname: String,
    pub node_rank: Option<u32>,
    pub duration_sec: f64,
    pub samples: usize,
    /// Number of CPUs this node contributed.
    pub cpus: usize,
    pub cpu_core_seconds: f64,
    pub cpu_pct_sum_peak: f64,
    pub cpu_efficiency_pct_avg: f64,
    /// Peak memory, from the cgroup where available.
    pub mem_peak_bytes: u64,
    pub exit_status: Option<i32>,
}

/// A whole job, combined from the per-node summaries by `logger-rs --merge`.
///
/// **On combining peaks.** Sums of rates and totals (core-seconds, bytes) are
/// exact: they are additive regardless of when each node was busy. Peaks are
/// not. Two nodes each peaking at 90% may have done so seconds apart, so the
/// job never actually used 180% simultaneously. Rather than pick one and
/// pretend, both bounds are reported: `*_max_node` (definitely reached by some
/// node) and `*_sum_of_nodes` (an upper bound the job cannot have exceeded).
/// The true simultaneous peak lies between them, and recovering it exactly
/// would need the per-node sample streams aligned on a common clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedSummary {
    pub event: String,
    /// Schema marker so the dashboard can tell a merged file from a per-node one.
    pub schema: String,

    pub user_id: String,
    pub job_id: String,
    pub queue: String,
    pub job_name: String,
    pub project: String,

    /// Earliest start and latest end across all reporting nodes.
    pub t_start: String,
    pub t_end: String,
    /// Longest node duration — the job's wall time.
    pub duration_sec: f64,

    pub num_nodes_reporting: usize,
    pub num_nodes_allocated: Option<u32>,
    /// Allocated nodes that produced no summary: a logger that was killed, or a
    /// node that never started one. Their usage is missing from the totals
    /// below, so a non-empty list means the job figures understate reality.
    pub nodes_missing: Vec<String>,

    /// Total CPUs across all reporting nodes.
    pub total_cpus: usize,
    pub samples_total: usize,

    // --- exact sums ---
    pub cpu_core_seconds: f64,
    pub cpu_core_hours: f64,
    pub io_read_bytes_total: u64,
    pub io_write_bytes_total: u64,
    pub net_recv_bytes_total: u64,
    pub net_sent_bytes_total: u64,
    pub major_faults_total: u64,
    pub minor_faults_total: u64,

    // --- peaks, both bounds (see the note on this struct) ---
    pub cpu_pct_sum_peak_max_node: f64,
    pub cpu_pct_sum_peak_sum_of_nodes: f64,
    pub mem_peak_bytes_max_node: u64,
    pub mem_peak_bytes_sum_of_nodes: u64,
    pub n_procs_peak_max_node: usize,
    pub n_threads_peak_max_node: usize,

    /// Core-seconds actually used as a percentage of core-seconds available
    /// across the whole allocation. The headline "did this job earn its nodes"
    /// number, and exact — it is a ratio of two sums, not a peak.
    pub cpu_efficiency_pct_avg: f64,
    pub walltime_efficiency_pct: f64,

    pub booked_walltime: String,
    pub booked_walltime_sec: u64,

    /// Worst status across nodes: any non-zero wins, `None` if any node's
    /// outcome is unknown.
    pub exit_status: Option<i32>,
    pub exit_reason: String,

    pub per_node: Vec<NodeSummary>,
    pub notes: MergedNotes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedNotes {
    pub totals: String,
    pub peaks: String,
    pub missing_nodes: String,
}

impl Default for MergedNotes {
    fn default() -> Self {
        Self {
            totals: "cpu_core_seconds, IO and network totals are exact sums over reporting nodes."
                .to_string(),
            peaks:
                "Peaks cannot be summed exactly: nodes may peak at different moments. *_max_node is \
                 a value some node definitely reached; *_sum_of_nodes is an upper bound the job \
                 cannot have exceeded. The true simultaneous peak lies between them."
                    .to_string(),
            missing_nodes:
                "nodes_missing lists allocated nodes that produced no summary. If it is non-empty, \
                 every total here understates the job."
                    .to_string(),
        }
    }
}

/// Element-wise max into `dst`, growing it as needed.
///
/// Growing rather than truncating is the fix for per-CPU vectors: they are
/// indexed by node CPU id, and the node has more CPUs than the job booked.
pub fn max_into_f64(dst: &mut Vec<f64>, src: &[f64]) {
    if dst.len() < src.len() {
        dst.resize(src.len(), 0.0);
    }
    for (d, &s) in dst.iter_mut().zip(src.iter()) {
        *d = d.max(s);
    }
}

/// Element-wise max into `dst`, growing it as needed.
pub fn max_into_u64(dst: &mut Vec<u64>, src: &[u64]) {
    if dst.len() < src.len() {
        dst.resize(src.len(), 0);
    }
    for (d, &s) in dst.iter_mut().zip(src.iter()) {
        *d = (*d).max(s);
    }
}

/// Element-wise `dst[i] += src[i] * scale`, growing `dst` as needed.
pub fn add_scaled_into(dst: &mut Vec<f64>, src: &[f64], scale: f64) {
    if dst.len() < src.len() {
        dst.resize(src.len(), 0.0);
    }
    for (d, &s) in dst.iter_mut().zip(src.iter()) {
        *d += s * scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_max_into_f64_grows() {
        let mut dst = vec![1.0];
        max_into_f64(&mut dst, &[0.5, 9.0, 3.0]);
        assert_eq!(dst, vec![1.0, 9.0, 3.0]);
    }

    #[test]
    fn test_max_into_f64_shorter_src_leaves_tail() {
        let mut dst = vec![1.0, 2.0, 3.0];
        max_into_f64(&mut dst, &[5.0]);
        assert_eq!(dst, vec![5.0, 2.0, 3.0]);
    }

    #[test]
    fn test_max_into_u64_grows() {
        let mut dst = vec![10u64];
        max_into_u64(&mut dst, &[5, 20]);
        assert_eq!(dst, vec![10, 20]);
    }

    #[test]
    fn test_add_scaled_into_grows() {
        let mut dst = vec![1.0];
        // 50% of a core for 2 seconds = 1.0 core-seconds added.
        add_scaled_into(&mut dst, &[50.0, 100.0], 2.0 / 100.0);
        assert!((dst[0] - 2.0).abs() < 1e-9);
        assert!((dst[1] - 2.0).abs() < 1e-9);
    }

    #[test]
    fn test_effective_peak_mem_prefers_cgroup() {
        let mut summary = TelemetrySummary::new(
            "u".into(),
            "j".into(),
            "q".into(),
            "n".into(),
            "p".into(),
            1,
            0.5,
            vec![0, 1],
            "cgroup-v1".into(),
            "/sys/fs/cgroup/cpuacct".into(),
            None,
            "01:00:00".into(),
            3600,
            "4GB".into(),
            4 * 1024 * 1024 * 1024,
        );

        // Falls back to the inflated summed-RSS figure when the cgroup has none.
        summary.rss_bytes_peak = 999;
        assert_eq!(summary.effective_peak_mem_bytes(), 999);

        summary.cgroup_mem_peak_bytes = Some(500);
        assert_eq!(summary.effective_peak_mem_bytes(), 500);
    }

    #[test]
    fn test_summary_new_starts_percpu_vectors_empty() {
        let summary = TelemetrySummary::new(
            "u".into(),
            "j".into(),
            "q".into(),
            "n".into(),
            "p".into(),
            1,
            0.5,
            vec![0, 1],
            "cgroup-v1".into(),
            "/x".into(),
            None,
            String::new(),
            0,
            String::new(),
            0,
        );

        // Grown on demand from the real per-CPU vector length, not pre-sized to
        // allowed_cpus.len().
        assert!(summary.cpu_core_seconds_by_cpu.is_empty());
        assert!(summary.system_percpu_pct_peak.is_empty());
        assert_eq!(summary.exit_status, None);
        assert_eq!(summary.exit_reason, "unknown");
    }
}
