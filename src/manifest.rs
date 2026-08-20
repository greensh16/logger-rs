//! The job manifest: one predictably-named file listing everything a job writes.
//!
//! Multi-node jobs produce one NDJSON per node, and the dashboard tails them
//! *live*. That rules out concatenating them into a single file — a combined
//! file cannot exist until the job has finished, which is exactly when live
//! tracking no longer matters.
//!
//! The manifest is the alternative: the poller looks up one file whose name it
//! can derive from the job id alone, and reads the list of per-node files from
//! it. No filename globbing, no parsing hostnames out of paths, and the node
//! count is stated up front so a node that has not started yet is
//! distinguishable from one that died.
//!
//! It is written for single-node jobs too, so the ingest path does not need a
//! special case.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::cli::expand_placeholders;
use crate::host::sanitise_for_filename;
use crate::types::{JobManifest, ManifestEntry};

/// Filename for a job's manifest, derivable from the job id alone.
pub fn manifest_filename(job_id: &str) -> String {
    format!("psutil_{}.manifest.json", sanitise_for_filename(job_id))
}

/// Build the manifest for a job.
///
/// `output_template` is the `--output` value, still containing its `{host}`
/// placeholder; it is expanded once per node here so the manifest lists the
/// exact filenames each logger will write.
#[allow(clippy::too_many_arguments)]
pub fn build_manifest(
    job_id: &str,
    user_id: &str,
    queue: &str,
    job_name: &str,
    project: &str,
    output_template: &str,
    hosts: &[String],
    created: String,
) -> JobManifest {
    let files = hosts
        .iter()
        .enumerate()
        .map(|(rank, host)| {
            let log = expand_placeholders(output_template, host, job_id, user_id);
            // Filenames only: the dashboard resolves them against the directory
            // it found the manifest in, so the job does not care where gdata is
            // mounted on the reader's side.
            let log_name = file_name_of(&log);
            ManifestEntry {
                hostname: host.clone(),
                node_rank: rank as u32,
                summary: format!("{}.summary.json", log_name),
                log: log_name,
            }
        })
        .collect();

    JobManifest {
        event: "job_manifest".to_string(),
        schema: "manifest-v1".to_string(),
        job_id: job_id.to_string(),
        user_id: user_id.to_string(),
        queue: queue.to_string(),
        job_name: job_name.to_string(),
        project: project.to_string(),
        created,
        logger_version: env!("CARGO_PKG_VERSION").to_string(),
        nodes_expected: hosts.len(),
        files,
        merged_summary: crate::merge::merged_summary_filename(job_id),
    }
}

fn file_name_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

/// Write the manifest into `dir`, atomically.
pub fn write_manifest(dir: &Path, manifest: &JobManifest) -> Result<PathBuf> {
    let path = dir.join(manifest_filename(&manifest.job_id));
    crate::output::write_json_atomic(&path, manifest)
        .with_context(|| format!("Failed to write manifest {:?}", path))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("gadi-cpu-clx-{:04}", i)).collect()
    }

    fn manifest_for(n: usize) -> JobManifest {
        build_manifest(
            "12345.gadi-pbs",
            "sam",
            "normal",
            "my_model",
            "ab12",
            "/g/data/ab12/dashboard/{user}/psutil_{jobid}_{host}.log",
            &hosts(n),
            "2026-07-28T11:28:00+1000".to_string(),
        )
    }

    #[test]
    fn test_manifest_filename_is_derivable_from_job_id() {
        // The whole point: the poller can construct this name without listing
        // the directory.
        assert_eq!(
            manifest_filename("12345.gadi-pbs"),
            "psutil_12345.gadi-pbs.manifest.json"
        );
    }

    #[test]
    fn test_manifest_lists_one_entry_per_node() {
        let m = manifest_for(3);

        assert_eq!(m.nodes_expected, 3);
        assert_eq!(m.files.len(), 3);
        assert_eq!(m.schema, "manifest-v1");
        assert_eq!(m.event, "job_manifest");

        let ranks: Vec<u32> = m.files.iter().map(|f| f.node_rank).collect();
        assert_eq!(ranks, vec![0, 1, 2]);
    }

    #[test]
    fn test_manifest_filenames_match_what_the_loggers_write() {
        let m = manifest_for(2);

        assert_eq!(
            m.files[0].log,
            "psutil_12345.gadi-pbs_gadi-cpu-clx-0001.log"
        );
        assert_eq!(
            m.files[0].summary,
            "psutil_12345.gadi-pbs_gadi-cpu-clx-0001.log.summary.json"
        );
        assert_eq!(
            m.files[1].log,
            "psutil_12345.gadi-pbs_gadi-cpu-clx-0002.log"
        );
    }

    #[test]
    fn test_manifest_entries_are_bare_filenames() {
        // Paths would tie the manifest to wherever gdata was mounted when the
        // job ran, which is not necessarily where the dashboard sees it.
        let m = manifest_for(2);
        for f in &m.files {
            assert!(!f.log.contains('/'), "{} should be a bare filename", f.log);
            assert!(!f.summary.contains('/'));
        }
    }

    #[test]
    fn test_manifest_points_at_the_merged_summary() {
        let m = manifest_for(2);
        assert_eq!(m.merged_summary, "psutil_12345.gadi-pbs.summary.json");
    }

    #[test]
    fn test_single_node_manifest_is_not_a_special_case() {
        let m = manifest_for(1);
        assert_eq!(m.nodes_expected, 1);
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.schema, "manifest-v1");
    }

    #[test]
    fn test_template_without_host_placeholder_still_works() {
        // Single-node jobs may use a plain path. Every entry then names the same
        // file, which is correct for one node and is why the wrapper inserts
        // {host} before launching a multi-node job.
        let m = build_manifest(
            "12345",
            "sam",
            "normal",
            "j",
            "ab12",
            "/g/data/plain.log",
            &["node1".to_string()],
            "2026-07-28T11:28:00+1000".to_string(),
        );
        assert_eq!(m.files[0].log, "plain.log");
    }

    #[test]
    fn test_manifest_round_trips() {
        let original = manifest_for(3);
        let text = serde_json::to_string(&original).unwrap();
        let restored: JobManifest = serde_json::from_str(&text).unwrap();

        assert_eq!(restored.job_id, original.job_id);
        assert_eq!(restored.nodes_expected, 3);
        assert_eq!(restored.files, original.files);
    }

    #[test]
    fn test_write_manifest_lands_at_the_derivable_name() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_for(2);

        let path = write_manifest(dir.path(), &m).unwrap();

        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "psutil_12345.gadi-pbs.manifest.json"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let restored: JobManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(restored.files.len(), 2);
    }
}
