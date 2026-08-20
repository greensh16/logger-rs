//! Scheduler (PBS / Slurm) metadata from the environment.

/// Scheduler metadata for multi-node jobs
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchedulerMetadata {
    pub num_nodes: Option<u32>,
    pub tasks_per_node: Option<u32>,
    pub cpus_per_task: Option<u32>,
}

/// Read scheduler metadata from the process environment.
pub fn get_scheduler_metadata() -> SchedulerMetadata {
    scheduler_metadata_from(|key| std::env::var(key).ok())
}

/// Pure form of [`get_scheduler_metadata`], parameterised over the variable
/// lookup.
///
/// Splitting this out is what makes the tests deterministic: they used to call
/// `env::set_var`/`remove_var`, which mutate *process-global* state while cargo
/// runs tests in parallel threads, so one test could clear the variables
/// another had just set. (`env::set_var` is also `unsafe` as of edition 2024.)
pub fn scheduler_metadata_from<F>(get: F) -> SchedulerMetadata
where
    F: Fn(&str) -> Option<String>,
{
    let num = |key: &str| -> Option<u32> { get(key)?.trim().parse::<u32>().ok() };

    // --- PBS ---
    let pbs_nodes = num("PBS_NUM_NODES").or_else(|| num("PBS_NNODES"));

    // NCPUS is *per chunk*, not the whole allocation.
    //
    // A 96-CPU job across two Gadi nodes has NCPUS=48, so dividing by the node
    // count reported 24 CPUs per node for a job that had 48. PBS has no notion
    // of "tasks" anyway, so nothing here derives cpus_per_task — the caller
    // falls back to the cpuset size, which is measured rather than inferred and
    // was correct all along.
    let mut meta = SchedulerMetadata {
        num_nodes: pbs_nodes,
        tasks_per_node: None,
        cpus_per_task: None,
    };

    // --- Slurm (overrides PBS when both are present) ---
    if let Some(n) = num("SLURM_JOB_NUM_NODES").or_else(|| num("SLURM_NNODES")) {
        meta.num_nodes = Some(n);
    }
    if let Some(n) = num("SLURM_NTASKS_PER_NODE") {
        meta.tasks_per_node = Some(n);
    }
    if let Some(n) = num("SLURM_CPUS_PER_TASK") {
        meta.cpus_per_task = Some(n);
    }

    meta
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn test_empty_environment() {
        let meta = scheduler_metadata_from(env_from(&[]));
        assert_eq!(meta, SchedulerMetadata::default());
        assert!(meta.num_nodes.is_none());
        assert!(meta.tasks_per_node.is_none());
        assert!(meta.cpus_per_task.is_none());
    }

    #[test]
    fn test_pbs_reports_node_count_only() {
        let meta =
            scheduler_metadata_from(env_from(&[("PBS_NUM_NODES", "4"), ("PBS_NCPUS", "16")]));
        assert_eq!(meta.num_nodes, Some(4));
        // PBS has no concept of tasks, and NCPUS is per chunk rather than for
        // the whole allocation, so nothing is inferred here.
        assert_eq!(meta.cpus_per_task, None);
        assert_eq!(meta.tasks_per_node, None);
    }

    #[test]
    fn test_pbs_does_not_divide_per_node_ncpus() {
        // Observed on Gadi: a 96-CPU job across 2 nodes has NCPUS=48, which is
        // the per-node figure. Dividing it reported 24 CPUs for a 48-CPU node.
        let meta = scheduler_metadata_from(env_from(&[("PBS_NNODES", "2"), ("NCPUS", "48")]));
        assert_eq!(meta.num_nodes, Some(2));
        assert_ne!(
            meta.cpus_per_task,
            Some(24),
            "must not halve a per-node NCPUS"
        );
        assert_eq!(meta.cpus_per_task, None);
    }

    #[test]
    fn test_pbs_single_node() {
        let meta = scheduler_metadata_from(env_from(&[("NCPUS", "48")]));
        assert_eq!(meta.cpus_per_task, None);
    }

    #[test]
    fn test_slurm_overrides_pbs() {
        let meta = scheduler_metadata_from(env_from(&[
            ("PBS_NUM_NODES", "4"),
            ("PBS_NCPUS", "16"),
            ("SLURM_JOB_NUM_NODES", "8"),
            ("SLURM_NTASKS_PER_NODE", "2"),
            ("SLURM_CPUS_PER_TASK", "4"),
        ]));
        assert_eq!(meta.num_nodes, Some(8));
        assert_eq!(meta.tasks_per_node, Some(2));
        assert_eq!(meta.cpus_per_task, Some(4));
    }

    #[test]
    fn test_ignores_unparseable_values() {
        let meta = scheduler_metadata_from(env_from(&[
            ("PBS_NUM_NODES", "not-a-number"),
            ("PBS_NCPUS", ""),
        ]));
        assert_eq!(meta.num_nodes, None);
        assert_eq!(meta.cpus_per_task, None);
    }

    #[test]
    fn test_zero_node_count_is_harmless() {
        let meta =
            scheduler_metadata_from(env_from(&[("PBS_NUM_NODES", "0"), ("PBS_NCPUS", "16")]));
        assert_eq!(meta.cpus_per_task, None);
    }

    #[test]
    fn test_reading_real_environment_does_not_panic() {
        let _ = get_scheduler_metadata();
    }
}
