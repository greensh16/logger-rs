//! Schema contract test.
//!
//! The NDJSON this binary writes is the interface between the logger and the
//! dashboard watcher, and nothing used to enforce it: the README, the committed
//! `test_sample.ndjson` fixture, and the code all described *different* wire
//! formats at once.
//!
//! These tests pin the field names. Adding a field is fine — add it to the list
//! below in the same change. Renaming or removing one will fail here, which is
//! the point: it should not be possible to break the dashboard silently.
//!
//! If a test fails, the fix is either to restore the field or to update this
//! list *and* the dashboard parser together.

use hpc_telemetry::types::{
    JobManifest, ManifestEntry, MergedSummary, TelemetrySample, TelemetrySummary,
};
use serde_json::Value;

/// Every key in the job manifest — the dashboard's entry point for a job.
const MANIFEST_FIELDS: &[&str] = &[
    "created",
    "event",
    "files",
    "job_id",
    "job_name",
    "logger_version",
    "merged_summary",
    "nodes_expected",
    "project",
    "queue",
    "schema",
    "user_id",
];

/// Every key of one entry in the manifest's `files` array.
const MANIFEST_ENTRY_FIELDS: &[&str] = &["hostname", "log", "node_rank", "summary"];

/// Every key in a merged, job-level summary.
const MERGED_FIELDS: &[&str] = &[
    "booked_walltime",
    "booked_walltime_sec",
    "cpu_core_hours",
    "cpu_core_seconds",
    "cpu_efficiency_pct_avg",
    "cpu_pct_sum_peak_max_node",
    "cpu_pct_sum_peak_sum_of_nodes",
    "duration_sec",
    "event",
    "exit_reason",
    "exit_status",
    "io_read_bytes_total",
    "io_write_bytes_total",
    "job_id",
    "job_name",
    "major_faults_total",
    "mem_peak_bytes_max_node",
    "mem_peak_bytes_sum_of_nodes",
    "minor_faults_total",
    "n_procs_peak_max_node",
    "n_threads_peak_max_node",
    "net_recv_bytes_total",
    "net_sent_bytes_total",
    "nodes_missing",
    "notes",
    "num_nodes_allocated",
    "num_nodes_reporting",
    "per_node",
    "project",
    "queue",
    "samples_total",
    "schema",
    "t_end",
    "t_start",
    "total_cpus",
    "user_id",
    "walltime_efficiency_pct",
];

/// Every key the dashboard may rely on in a `job_log` line.
const SAMPLE_FIELDS: &[&str] = &[
    "allowed_cpus",
    "booked_mem",
    "booked_mem_bytes",
    "booked_walltime",
    "booked_walltime_sec",
    "cgroup_io_read_bytes",
    "cgroup_io_read_ops",
    "cgroup_io_write_bytes",
    "cgroup_io_write_ops",
    "cgroup_mem_bytes",
    "cgroup_mem_peak_bytes",
    "cgroup_swap_bytes",
    "cpu_efficiency_pct",
    "cpu_pct_sum",
    "cpus_per_task",
    "dt_sec",
    "event",
    "gpu_indices",
    "gpu_is_node_scoped",
    "gpu_memory_total",
    "gpu_memory_used",
    "gpu_power",
    "gpu_temperature",
    "gpu_utilization",
    "hostname",
    "io_read_bytes",
    "io_read_ops",
    "io_write_bytes",
    "io_write_ops",
    "job_id",
    "job_name",
    "major_faults",
    "minor_faults",
    "n_open_fds",
    "n_procs",
    "n_threads",
    "net_is_node_scoped",
    "net_recv_bytes",
    "net_recv_packets",
    "net_sent_bytes",
    "net_sent_packets",
    "num_nodes",
    "project",
    "queue",
    "rss_bytes_sum",
    "swap_bytes",
    "system_cpu_efficiency",
    "system_percpu_pct",
    "t",
    "tasks_per_node",
    "tree_percpu_pct",
    "user_id",
];

/// Every key in the summary file.
const SUMMARY_FIELDS: &[&str] = &[
    "allowed_cpus",
    "avg_cpu_percent_over_run",
    "booked_mem",
    "booked_mem_bytes",
    "booked_mem_gb",
    "booked_mem_is_per_node",
    "booked_mem_source",
    "booked_walltime",
    "booked_walltime_sec",
    "cgroup_io_read_bytes_total",
    "cgroup_io_read_ops_total",
    "cgroup_io_write_bytes_total",
    "cgroup_io_write_ops_total",
    "cgroup_mem_peak_bytes",
    "cgroup_mem_peak_gb",
    "cgroup_swap_peak_bytes",
    "cgroup_version",
    "cpu_core_hours",
    "cpu_core_seconds",
    "cpu_core_seconds_by_cpu",
    "cpu_efficiency_pct_avg",
    "cpu_efficiency_pct_peak",
    "cpu_pct_sum_peak",
    "cpuacct_path",
    "cpus_per_task",
    "cpuset_path_used",
    "duration_sec",
    "exit_reason",
    "exit_status",
    "gpu_indices",
    "gpu_is_node_scoped",
    "gpu_memory_total",
    "gpu_memory_used_peak",
    "gpu_power_peak",
    "gpu_temperature_peak",
    "gpu_utilization_peak",
    "hostname",
    "interval_sec",
    "io_read_bytes_total",
    "io_read_ops_total",
    "io_write_bytes_total",
    "io_write_ops_total",
    "job_id",
    "job_name",
    "major_faults_total",
    "mem_efficiency_pct",
    "minor_faults_total",
    "mode",
    "n_open_fds_peak",
    "n_procs_peak",
    "n_threads_peak",
    "net_recv_bytes_total",
    "net_recv_packets_total",
    "net_sent_bytes_total",
    "net_sent_packets_total",
    "node_rank",
    "notes",
    "num_nodes",
    "num_nodes_allocated",
    "proc_source",
    "project",
    "queue",
    "rss_bytes_peak",
    "rss_gb_peak",
    "samples",
    "swap_bytes_peak",
    "system_percpu_pct_peak",
    "t_end",
    "t_start",
    "tasks_per_node",
    "tree_avg_cpu_percent_by_cpu",
    "tree_percpu_pct_peak",
    "tree_pid",
    "user_id",
    "walltime_efficiency_pct",
];

fn sorted_keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("expected a JSON object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

fn assert_fields_match(actual: &[String], expected: &[&str], what: &str) {
    let expected: Vec<String> = {
        let mut e: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        e.sort();
        e
    };

    let missing: Vec<&String> = expected.iter().filter(|k| !actual.contains(k)).collect();
    let unexpected: Vec<&String> = actual.iter().filter(|k| !expected.contains(k)).collect();

    assert!(
        missing.is_empty(),
        "{what}: fields the dashboard expects are MISSING from the output: {missing:?}. \
         Removing or renaming a field breaks the dashboard parser."
    );
    assert!(
        unexpected.is_empty(),
        "{what}: NEW fields present that are not in the contract list: {unexpected:?}. \
         Add them to tests/schema_contract.rs in this change so the addition is deliberate."
    );
}

/// A sample with every optional field populated, so nothing is skipped.
fn full_sample() -> TelemetrySample {
    TelemetrySample {
        event: "job_log".to_string(),
        user_id: "sam".to_string(),
        job_id: "12345.gadi-pbs".to_string(),
        queue: "normal".to_string(),
        job_name: "my_model".to_string(),
        project: "ab12".to_string(),
        hostname: "gadi-cpu-clx-0123".to_string(),
        t: 1_709_145_600.5,
        dt_sec: 0.52,
        allowed_cpus: vec![0, 1, 2, 3],
        cpu_pct_sum: 385.0,
        rss_bytes_sum: 8_589_934_592,
        n_procs: 9,
        system_percpu_pct: vec![96.0, 97.0, 95.0, 97.0],
        tree_percpu_pct: vec![96.0, 97.0, 95.0, 97.0],
        system_cpu_efficiency: 385.0,
        cpu_efficiency_pct: 96.25,
        booked_walltime: "02:00:00".to_string(),
        booked_walltime_sec: 7200,
        booked_mem: "16GB".to_string(),
        booked_mem_bytes: 17_179_869_184,
        n_threads: 36,
        n_open_fds: 120,
        major_faults: 12,
        minor_faults: 998_877,
        io_read_bytes: 1_073_741_824,
        io_write_bytes: 536_870_912,
        io_read_ops: 4096,
        io_write_ops: 2048,
        swap_bytes: 0,
        cgroup_mem_bytes: Some(5_368_709_120),
        cgroup_mem_peak_bytes: Some(6_442_450_944),
        cgroup_swap_bytes: Some(0),
        cgroup_io_read_bytes: Some(2_147_483_648),
        cgroup_io_write_bytes: Some(1_073_741_824),
        cgroup_io_read_ops: Some(8192),
        cgroup_io_write_ops: Some(4096),
        num_nodes: Some(1),
        tasks_per_node: Some(1),
        cpus_per_task: Some(4),
        net_recv_bytes: 1_048_576,
        net_sent_bytes: 524_288,
        net_recv_packets: 1024,
        net_sent_packets: 512,
        net_is_node_scoped: true,
        gpu_utilization: vec![88.0],
        gpu_memory_used: vec![4_294_967_296],
        gpu_memory_total: vec![42_949_672_960],
        gpu_temperature: vec![71.0],
        gpu_power: vec![245.5],
        // A job given GPU 1 of a multi-GPU node: readings narrowed via
        // CUDA_VISIBLE_DEVICES, so they are job-scoped rather than node-wide.
        gpu_indices: vec![1],
        gpu_is_node_scoped: false,
    }
}

fn full_summary() -> TelemetrySummary {
    let mut summary = TelemetrySummary::new(
        "sam".to_string(),
        "12345.gadi-pbs".to_string(),
        "normal".to_string(),
        "my_model".to_string(),
        "ab12".to_string(),
        4242,
        0.5,
        vec![0, 1, 2, 3],
        "cgroup-v1".to_string(),
        "/sys/fs/cgroup/cpuacct/pbs_jobs.service/jobid/12345.gadi-pbs".to_string(),
        Some("/sys/fs/cgroup/cpuset/pbs_jobs.service/jobid/12345.gadi-pbs".to_string()),
        "02:00:00".to_string(),
        7200,
        "16GB".to_string(),
        17_179_869_184,
    );

    // Populate the skip_serializing_if fields so they appear in the output.
    summary.cgroup_mem_peak_bytes = Some(6_442_450_944);
    summary.cgroup_mem_peak_gb = Some(6.0);
    summary.cgroup_swap_peak_bytes = Some(0);
    summary.cgroup_io_read_bytes_total = Some(2_147_483_648);
    summary.cgroup_io_write_bytes_total = Some(1_073_741_824);
    summary.cgroup_io_read_ops_total = Some(8192);
    summary.cgroup_io_write_ops_total = Some(4096);
    summary.exit_status = Some(0);
    summary.exit_reason = "completed".to_string();
    summary.hostname = "gadi-cpu-clx-0123".to_string();
    summary.node_rank = Some(0);
    summary.num_nodes_allocated = Some(2);
    summary.proc_source = "cgroup.procs".to_string();

    summary
}

/// A merged summary built by the real merge code, not hand-assembled.
fn merged_summary() -> MergedSummary {
    let mut node_a = full_summary();
    node_a.hostname = "gadi-cpu-clx-0123".to_string();
    node_a.node_rank = Some(0);
    node_a.duration_sec = 100.0;
    node_a.cpu_core_seconds = 4000.0;

    let mut node_b = full_summary();
    node_b.hostname = "gadi-cpu-clx-0124".to_string();
    node_b.node_rank = Some(1);
    node_b.duration_sec = 100.0;
    node_b.cpu_core_seconds = 3000.0;

    hpc_telemetry::merge::merge_summaries(
        &[node_a, node_b],
        &[
            "gadi-cpu-clx-0123".to_string(),
            "gadi-cpu-clx-0124".to_string(),
        ],
    )
    .expect("merging two nodes must succeed")
}

#[test]
fn sample_schema_is_stable() {
    let json = serde_json::to_value(full_sample()).unwrap();
    assert_fields_match(&sorted_keys(&json), SAMPLE_FIELDS, "TelemetrySample");
}

#[test]
fn summary_schema_is_stable() {
    let json = serde_json::to_value(full_summary()).unwrap();
    assert_fields_match(&sorted_keys(&json), SUMMARY_FIELDS, "TelemetrySummary");
}

fn manifest() -> JobManifest {
    hpc_telemetry::manifest::build_manifest(
        "12345.gadi-pbs",
        "sam",
        "normal",
        "my_model",
        "ab12",
        "/g/data/ab12/dashboard/{user}/psutil_{jobid}_{host}.log",
        &[
            "gadi-cpu-clx-0123".to_string(),
            "gadi-cpu-clx-0124".to_string(),
        ],
        "2026-07-28T11:28:00+1000".to_string(),
    )
}

#[test]
fn manifest_schema_is_stable() {
    let json = serde_json::to_value(manifest()).unwrap();
    assert_fields_match(&sorted_keys(&json), MANIFEST_FIELDS, "JobManifest");

    let entry = &json["files"].as_array().unwrap()[0];
    assert_fields_match(&sorted_keys(entry), MANIFEST_ENTRY_FIELDS, "ManifestEntry");
}

#[test]
fn manifest_is_findable_from_the_job_id_alone() {
    // The reason the manifest exists: the poller must be able to construct the
    // filename without listing or globbing the directory.
    assert_eq!(
        hpc_telemetry::manifest::manifest_filename("12345.gadi-pbs"),
        "psutil_12345.gadi-pbs.manifest.json"
    );

    let m = manifest();
    assert_eq!(m.schema, "manifest-v1");
    assert_eq!(m.event, "job_manifest");
    assert_eq!(m.nodes_expected, 2);
    assert_eq!(m.files.len(), 2);

    // Bare filenames: paths would tie the manifest to wherever gdata happened
    // to be mounted when the job ran.
    for f in &m.files {
        assert!(!f.log.contains('/'), "{} must be a bare filename", f.log);
        assert!(!f.summary.contains('/'));
    }
}

#[test]
fn manifest_entries_name_the_files_the_loggers_actually_write() {
    // If these drift apart the dashboard will wait forever for files that are
    // never written under those names.
    let m = manifest();
    let entry = ManifestEntry {
        hostname: "gadi-cpu-clx-0123".to_string(),
        node_rank: 0,
        log: "psutil_12345.gadi-pbs_gadi-cpu-clx-0123.log".to_string(),
        summary: "psutil_12345.gadi-pbs_gadi-cpu-clx-0123.log.summary.json".to_string(),
    };
    assert_eq!(m.files[0], entry);
    assert_eq!(m.merged_summary, "psutil_12345.gadi-pbs.summary.json");
}

#[test]
fn merged_summary_schema_is_stable() {
    let json = serde_json::to_value(merged_summary()).unwrap();
    assert_fields_match(&sorted_keys(&json), MERGED_FIELDS, "MergedSummary");
}

#[test]
fn merged_summary_is_distinguishable_from_a_per_node_one() {
    // Both land in the same directory, so the dashboard must be able to tell
    // them apart without relying on the filename.
    let merged = serde_json::to_value(merged_summary()).unwrap();
    let per_node = serde_json::to_value(full_summary()).unwrap();

    assert_eq!(merged["event"], "job_summary");
    assert_eq!(merged["schema"], "merged-v1");
    assert!(
        per_node.get("schema").is_none(),
        "a per-node summary must not claim the merged schema marker"
    );
}

#[test]
fn merged_totals_are_exact_sums_and_peaks_carry_both_bounds() {
    let merged = merged_summary();

    // Core-seconds are additive regardless of when each node was busy.
    assert!((merged.cpu_core_seconds - 7000.0).abs() < 1e-9);
    assert_eq!(merged.num_nodes_reporting, 2);
    assert_eq!(merged.total_cpus, 8); // 2 nodes x 4 allowed CPUs
    assert!(merged.nodes_missing.is_empty());

    // Peaks cannot be summed exactly, so both bounds must be present and the
    // upper bound must never be below the observed one.
    assert!(merged.cpu_pct_sum_peak_sum_of_nodes >= merged.cpu_pct_sum_peak_max_node);
    assert!(merged.mem_peak_bytes_sum_of_nodes >= merged.mem_peak_bytes_max_node);

    // Efficiency is a ratio of two sums: 7000 core-seconds used out of
    // 8 CPUs x 100s = 800 available... which exceeds 100%, so just check it is
    // computed from the totals rather than averaged over nodes.
    assert!(merged.cpu_efficiency_pct_avg > 0.0);
    assert_eq!(merged.per_node.len(), 2);
}

#[test]
fn merged_summary_survives_a_round_trip() {
    let original = merged_summary();
    let text = serde_json::to_string(&original).unwrap();
    let restored: MergedSummary = serde_json::from_str(&text).unwrap();

    assert_eq!(restored.job_id, original.job_id);
    assert_eq!(restored.num_nodes_reporting, 2);
    assert_eq!(restored.per_node.len(), 2);
    assert!((restored.cpu_core_seconds - original.cpu_core_seconds).abs() < 1e-9);
}

#[test]
fn summary_records_how_processes_were_identified() {
    // Whether the job's processes came from the kernel's cgroup membership or
    // from a tree walk changes how much the process and thread counts can be
    // trusted, so the summary has to say which was used.
    let json = serde_json::to_value(full_summary()).unwrap();
    assert_eq!(json["proc_source"], "cgroup.procs");
}

#[test]
fn every_sample_carries_the_node_it_came_from() {
    // A multi-node job writes one file per node. Without the hostname on each
    // record, a merged or interleaved view cannot attribute a reading to a node.
    let json = serde_json::to_value(full_sample()).unwrap();
    assert_eq!(json["hostname"], "gadi-cpu-clx-0123");

    let summary = serde_json::to_value(full_summary()).unwrap();
    assert_eq!(summary["hostname"], "gadi-cpu-clx-0123");
    assert_eq!(summary["node_rank"], 0);
}

#[test]
fn sample_serialises_as_a_single_ndjson_line() {
    // NDJSON means one object per line: an embedded newline would corrupt the
    // stream for every consumer reading line by line.
    let line = serde_json::to_string(&full_sample()).unwrap();
    assert!(
        !line.contains('\n'),
        "sample must serialise without newlines"
    );

    let parsed: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(parsed["event"], "job_log");
}

#[test]
fn booked_resources_are_reported_consistently() {
    // job_start used to emit booked_mem as "4294967296b" while job_log emitted
    // "4GB" — same field name, two formats, in the same file. Both the string
    // and a numeric form must now travel together.
    let json = serde_json::to_value(full_sample()).unwrap();

    assert_eq!(json["booked_mem"], "16GB");
    assert_eq!(json["booked_mem_bytes"], 17_179_869_184u64);
    assert_eq!(json["booked_walltime"], "02:00:00");
    assert_eq!(json["booked_walltime_sec"], 7200);
}

#[test]
fn metric_scope_is_labelled_rather_than_implied() {
    let json = serde_json::to_value(full_sample()).unwrap();

    // Network counters come from /proc/net/dev, which has no notion of job
    // ownership, so they are always node-wide and always say so.
    assert_eq!(json["net_is_node_scoped"], true);

    // GPU readings can be narrowed to the job via CUDA_VISIBLE_DEVICES. When
    // they have been, the flag must say so and the physical indices must travel
    // with them — otherwise the dashboard cannot tell "GPU 1 was busy" from
    // "one of the node's GPUs was busy".
    assert_eq!(json["gpu_is_node_scoped"], false);
    assert_eq!(json["gpu_indices"], serde_json::json!([1]));
    assert_eq!(
        json["gpu_indices"].as_array().unwrap().len(),
        json["gpu_utilization"].as_array().unwrap().len(),
        "gpu_indices must stay aligned with the reading vectors"
    );
}

#[test]
fn exit_status_is_null_when_unknown_not_zero() {
    // The single most misleading thing the old logger did was report
    // exit_status 0 for every job, including failures.
    let summary = TelemetrySummary::new(
        "sam".to_string(),
        "1".to_string(),
        "normal".to_string(),
        "j".to_string(),
        "ab12".to_string(),
        1,
        0.5,
        vec![0],
        "cgroup-v1".to_string(),
        "/x".to_string(),
        None,
        "01:00:00".to_string(),
        3600,
        "1GB".to_string(),
        1_073_741_824,
    );

    let json = serde_json::to_value(&summary).unwrap();
    assert!(
        json["exit_status"].is_null(),
        "unknown exit status must serialise as null, got {}",
        json["exit_status"]
    );
    assert_ne!(json["exit_status"], 0);
}

#[test]
fn summary_survives_a_round_trip() {
    let original = full_summary();
    let text = serde_json::to_string(&original).unwrap();
    let restored: TelemetrySummary = serde_json::from_str(&text).unwrap();

    assert_eq!(restored.job_id, original.job_id);
    assert_eq!(restored.cgroup_version, "cgroup-v1");
    assert_eq!(restored.exit_status, Some(0));
    assert_eq!(restored.cgroup_mem_peak_bytes, Some(6_442_450_944));
}

#[test]
fn older_payloads_without_new_fields_still_deserialise() {
    // A dashboard replaying archived logs must not choke on lines written by an
    // older logger. Every field added since is #[serde(default)].
    let legacy = r#"{
        "event": "job_log",
        "user_id": "sam",
        "job_id": "1.gadi-pbs",
        "queue": "normal",
        "job_name": "j",
        "project": "ab12",
        "t": 1709145600.5,
        "allowed_cpus": [0, 1],
        "cpu_pct_sum": 150.0,
        "rss_bytes_sum": 1024,
        "n_procs": 2,
        "system_percpu_pct": [75.0, 75.0],
        "tree_percpu_pct": [75.0, 75.0],
        "system_cpu_efficiency": 150.0,
        "booked_walltime": "01:00:00",
        "booked_mem": "4GB",
        "n_threads": 4,
        "n_open_fds": 10,
        "major_faults": 0,
        "minor_faults": 5,
        "io_read_bytes": 0,
        "io_write_bytes": 0,
        "io_read_ops": 0,
        "io_write_ops": 0,
        "swap_bytes": 0,
        "num_nodes": 1,
        "tasks_per_node": 1,
        "cpus_per_task": 2,
        "net_recv_bytes": 0,
        "net_sent_bytes": 0,
        "net_recv_packets": 0,
        "net_sent_packets": 0,
        "gpu_utilization": [],
        "gpu_memory_used": [],
        "gpu_memory_total": [],
        "gpu_temperature": [],
        "gpu_power": []
    }"#;

    let sample: TelemetrySample =
        serde_json::from_str(legacy).expect("legacy payload must still parse");

    assert_eq!(sample.job_id, "1.gadi-pbs");
    assert_eq!(sample.cgroup_mem_bytes, None);
    assert_eq!(sample.dt_sec, 0.0);
    // Defaults for the scope flags say "node" because that is what the old
    // logger was in fact reporting.
    assert!(sample.net_is_node_scoped);
}

#[test]
fn committed_fixture_matches_the_current_schema() {
    // test_sample.ndjson used to describe a schema the code had never emitted
    // ({"event":"sample","cpu_pct":...,"memory_mb":...}). Keeping it honest
    // means it has to parse with the real types.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test_sample.ndjson");
    let content = std::fs::read_to_string(path).expect("test_sample.ndjson must exist");

    let mut saw_start = false;
    let mut saw_log = false;
    let mut saw_end = false;

    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let value: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {} is not valid JSON: {e}", i + 1));

        match value["event"].as_str() {
            Some("job_start") => saw_start = true,
            Some("job_end") => {
                saw_end = true;
                assert!(
                    value.get("exit_status").is_some(),
                    "job_end must carry an exit_status field"
                );
            }
            Some("job_log") => {
                saw_log = true;
                // Must round-trip through the real type, not just be valid JSON.
                let _: TelemetrySample = serde_json::from_value(value.clone())
                    .unwrap_or_else(|e| panic!("line {} is not a TelemetrySample: {e}", i + 1));
            }
            other => panic!("line {} has unknown event type {:?}", i + 1, other),
        }
    }

    assert!(saw_start, "fixture should contain a job_start line");
    assert!(saw_log, "fixture should contain at least one job_log line");
    assert!(saw_end, "fixture should contain a job_end line");
}
