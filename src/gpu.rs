//! GPU metrics via `nvidia-smi`.
//!
//! Two things to know about these numbers:
//!
//! 1. **Scope.** `nvidia-smi` reports every GPU on the host. When
//!    `CUDA_VISIBLE_DEVICES` is set — which PBS and Slurm both do for GPU jobs —
//!    the readings are filtered down to the GPUs actually assigned to this job,
//!    and `GpuStats::job_scoped` is set. When it is unset we cannot tell which
//!    GPUs are ours, so the figures stay node-wide and are labelled as such.
//! 2. **Cost.** Each poll forks a process. `nvidia-smi` typically takes 50-200ms
//!    and can block on a busy GPU, so polling it at the 0.5s sample interval both
//!    cost real time and stretched the sampling tick. [`GpuSampler`] probes once
//!    for availability and then polls on its own slower cadence, reusing the last
//!    reading in between.

use std::process::Command;
use std::time::{Duration, Instant};

/// GPU statistics, one entry per GPU in scope.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuStats {
    /// Node GPU index for each entry, so a reading can be tied back to a
    /// physical device even after filtering.
    pub indices: Vec<u32>,
    /// GPU UUIDs. Used to resolve `CUDA_VISIBLE_DEVICES` when it is expressed as
    /// UUIDs rather than indices; not written to the telemetry stream.
    pub uuids: Vec<String>,
    pub utilization: Vec<f64>,
    pub memory_used: Vec<u64>,
    pub memory_total: Vec<u64>,
    pub temperature: Vec<f64>,
    pub power: Vec<f64>,
    /// True when these entries are the job's GPUs rather than the node's.
    pub job_scoped: bool,
}

impl GpuStats {
    pub fn gpu_count(&self) -> usize {
        self.utilization.len()
    }
}

/// Which GPUs the scheduler has assigned to this job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuVisibility {
    /// No restriction known — readings will be node-wide.
    All,
    /// Explicitly no GPUs.
    NoneVisible,
    /// Indices or UUIDs, exactly as written in the environment variable.
    Only(Vec<String>),
}

impl GpuVisibility {
    /// Whether a filtered result can honestly be called job-scoped.
    pub fn is_job_scoped(&self) -> bool {
        !matches!(self, GpuVisibility::All)
    }
}

/// Read the GPU assignment from the environment.
///
/// `CUDA_VISIBLE_DEVICES` is set by PBS and Slurm for GPU jobs;
/// `NVIDIA_VISIBLE_DEVICES` is the container-runtime equivalent and is checked
/// as a fallback.
pub fn visibility_from_env() -> GpuVisibility {
    for var in ["CUDA_VISIBLE_DEVICES", "NVIDIA_VISIBLE_DEVICES"] {
        if let Ok(raw) = std::env::var(var) {
            let vis = parse_visible_devices(Some(&raw));
            if vis != GpuVisibility::All {
                return vis;
            }
        }
    }
    GpuVisibility::All
}

/// Parse a `CUDA_VISIBLE_DEVICES`-style value.
///
/// CUDA's own rules: unset means all devices; empty means none; the literal
/// `all` and `none` are honoured; otherwise it is a comma-separated list of
/// indices or (possibly abbreviated) UUIDs.
pub fn parse_visible_devices(raw: Option<&str>) -> GpuVisibility {
    let Some(raw) = raw else {
        return GpuVisibility::All;
    };
    let trimmed = raw.trim();

    if trimmed.is_empty() {
        return GpuVisibility::NoneVisible;
    }
    if trimmed.eq_ignore_ascii_case("all") {
        return GpuVisibility::All;
    }
    if trimmed.eq_ignore_ascii_case("none") {
        return GpuVisibility::NoneVisible;
    }

    let tokens: Vec<String> = trimmed
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.is_empty() {
        GpuVisibility::NoneVisible
    } else {
        GpuVisibility::Only(tokens)
    }
}

/// Keep only the GPUs the job can see.
pub fn filter_to_visible(stats: &GpuStats, visibility: &GpuVisibility) -> GpuStats {
    let mut out = GpuStats {
        job_scoped: visibility.is_job_scoped(),
        ..Default::default()
    };

    let tokens = match visibility {
        GpuVisibility::All => {
            let mut all = stats.clone();
            all.job_scoped = false;
            return all;
        }
        GpuVisibility::NoneVisible => return out,
        GpuVisibility::Only(tokens) => tokens,
    };

    for i in 0..stats.gpu_count() {
        let index = stats.indices.get(i).copied().unwrap_or(i as u32);
        let uuid = stats.uuids.get(i).map(String::as_str).unwrap_or("");

        let matched = tokens.iter().any(|token| {
            // Numeric form: compare as numbers so "01" and "1" agree, and so no
            // string is allocated per comparison.
            if let Ok(wanted) = token.parse::<u32>() {
                return wanted == index;
            }
            // CUDA permits abbreviated UUIDs, e.g. "GPU-a1b2" for the full form.
            !uuid.is_empty()
                && (token.starts_with("GPU-") || token.starts_with("MIG-"))
                && uuid.starts_with(token)
        });

        if !matched {
            continue;
        }

        out.indices.push(index);
        out.uuids.push(uuid.to_string());
        out.utilization.push(copy_at(&stats.utilization, i));
        out.memory_used.push(copy_at(&stats.memory_used, i));
        out.memory_total.push(copy_at(&stats.memory_total, i));
        out.temperature.push(copy_at(&stats.temperature, i));
        out.power.push(copy_at(&stats.power, i));
    }

    out
}

fn copy_at<T: Copy + Default>(v: &[T], i: usize) -> T {
    v.get(i).copied().unwrap_or_default()
}

/// Polls `nvidia-smi` at a bounded rate, caching between polls.
#[derive(Debug)]
pub struct GpuSampler {
    /// `None` until the first probe. `Some(false)` disables all future polling.
    available: Option<bool>,
    interval: Duration,
    last_poll: Option<Instant>,
    cached: GpuStats,
    visibility: GpuVisibility,
}

impl GpuSampler {
    pub fn new(interval_sec: f64) -> Self {
        Self::with_visibility(interval_sec, visibility_from_env())
    }

    pub fn with_visibility(interval_sec: f64, visibility: GpuVisibility) -> Self {
        Self {
            available: None,
            interval: Duration::from_secs_f64(interval_sec.max(0.0)),
            last_poll: None,
            cached: GpuStats {
                job_scoped: visibility.is_job_scoped(),
                ..Default::default()
            },
            visibility,
        }
    }

    /// True once we have established there are no GPUs to poll.
    pub fn is_disabled(&self) -> bool {
        self.available == Some(false)
    }

    fn empty(&self) -> GpuStats {
        GpuStats {
            job_scoped: self.visibility.is_job_scoped(),
            ..Default::default()
        }
    }

    /// Return current GPU stats, polling only if the cadence says it is time.
    pub fn sample(&mut self) -> GpuStats {
        if self.available == Some(false) {
            return self.empty();
        }

        // The scheduler told us this job has no GPUs; never fork nvidia-smi.
        if self.visibility == GpuVisibility::NoneVisible {
            self.available = Some(false);
            return self.empty();
        }

        let now = Instant::now();
        let due = match self.last_poll {
            None => true,
            Some(last) => now.duration_since(last) >= self.interval,
        };

        if !due {
            return self.cached.clone();
        }

        self.last_poll = Some(now);

        match run_nvidia_smi() {
            Some(raw) => {
                let stats = filter_to_visible(&raw, &self.visibility);
                // A host with the driver present but no GPUs in scope is still
                // worth disabling: nothing to report, and every poll costs a fork.
                self.available = Some(stats.gpu_count() > 0);
                self.cached = stats.clone();
                stats
            }
            None => {
                self.available = Some(false);
                self.cached = self.empty();
                self.empty()
            }
        }
    }
}

/// Run nvidia-smi once. `None` means it is unavailable or failed.
fn run_nvidia_smi() -> Option<GpuStats> {
    let output = Command::new("nvidia-smi")
        .arg(
            "--query-gpu=index,utilization.gpu,memory.used,memory.total,\
             temperature.gpu,power.draw,uuid",
        )
        .arg("--format=csv,noheader,nounits")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(parse_nvidia_smi_csv(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Parse `nvidia-smi --format=csv,noheader,nounits` output.
///
/// Fields the driver reports as `[N/A]` (common for power draw on older cards,
/// and for everything under MIG) parse as 0 rather than dropping the row, so all
/// the vectors always stay the same length as the GPU count.
///
/// The trailing UUID column is optional so that a driver which does not supply
/// it still yields usable readings — UUID-form `CUDA_VISIBLE_DEVICES` just
/// cannot be resolved in that case.
pub fn parse_nvidia_smi_csv(stdout: &str) -> GpuStats {
    let mut stats = GpuStats::default();

    for (row, line) in stdout.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let values: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if values.len() < 6 {
            continue;
        }

        stats
            .indices
            .push(values[0].parse::<u32>().unwrap_or(row as u32));
        stats.utilization.push(parse_f64_or_zero(values[1]));
        // Reported in MiB.
        stats
            .memory_used
            .push(parse_f64_or_zero(values[2]) as u64 * 1024 * 1024);
        stats
            .memory_total
            .push(parse_f64_or_zero(values[3]) as u64 * 1024 * 1024);
        stats.temperature.push(parse_f64_or_zero(values[4]));
        stats.power.push(parse_f64_or_zero(values[5]));
        stats
            .uuids
            .push(values.get(6).map(|s| s.to_string()).unwrap_or_default());
    }

    stats
}

fn parse_f64_or_zero(s: &str) -> f64 {
    s.trim().parse::<f64>().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_GPUS: &str = "\
0, 45, 1234, 8192, 65, 120.5, GPU-aaaa1111-2222-3333-4444-555555555555
1, 90, 4096, 8192, 72, 210.0, GPU-bbbb1111-2222-3333-4444-555555555555
";

    #[test]
    fn test_gpu_stats_default() {
        let stats = GpuStats::default();
        assert_eq!(stats.gpu_count(), 0);
        assert!(stats.memory_used.is_empty());
        assert!(!stats.job_scoped);
    }

    #[test]
    fn test_parse_nvidia_smi_csv() {
        let stats = parse_nvidia_smi_csv(TWO_GPUS);

        assert_eq!(stats.gpu_count(), 2);
        assert_eq!(stats.indices, vec![0, 1]);
        assert_eq!(stats.utilization, vec![45.0, 90.0]);
        assert_eq!(stats.memory_used[0], 1234 * 1024 * 1024);
        assert_eq!(stats.memory_total[1], 8192 * 1024 * 1024);
        assert_eq!(stats.temperature, vec![65.0, 72.0]);
        assert_eq!(stats.power, vec![120.5, 210.0]);
        assert!(stats.uuids[0].starts_with("GPU-aaaa"));
    }

    #[test]
    fn test_parse_nvidia_smi_csv_without_uuid_column() {
        // Six columns, as an older driver might emit.
        let stats = parse_nvidia_smi_csv("0, 45, 1234, 8192, 65, 120.5\n");
        assert_eq!(stats.gpu_count(), 1);
        assert_eq!(stats.uuids, vec![String::new()]);
    }

    #[test]
    fn test_parse_nvidia_smi_csv_handles_na() {
        // Power draw is frequently [N/A]; the row must still be kept so the
        // vectors stay aligned with the GPU index.
        let stats = parse_nvidia_smi_csv("0, 45, 1234, 8192, 65, [N/A], GPU-x\n");
        assert_eq!(stats.gpu_count(), 1);
        assert_eq!(stats.power, vec![0.0]);
    }

    #[test]
    fn test_parse_nvidia_smi_csv_respects_reported_index() {
        // nvidia-smi's index column is authoritative and need not start at 0.
        let stats = parse_nvidia_smi_csv("2, 10, 1, 2, 3, 4, GPU-c\n3, 20, 1, 2, 3, 4, GPU-d\n");
        assert_eq!(stats.indices, vec![2, 3]);
    }

    #[test]
    fn test_parse_nvidia_smi_csv_ignores_short_rows() {
        assert_eq!(parse_nvidia_smi_csv("0, 45\n").gpu_count(), 0);
        assert_eq!(parse_nvidia_smi_csv("").gpu_count(), 0);
    }

    #[test]
    fn test_parse_visible_devices() {
        assert_eq!(parse_visible_devices(None), GpuVisibility::All);
        assert_eq!(parse_visible_devices(Some("all")), GpuVisibility::All);
        assert_eq!(parse_visible_devices(Some("")), GpuVisibility::NoneVisible);
        assert_eq!(
            parse_visible_devices(Some("   ")),
            GpuVisibility::NoneVisible
        );
        assert_eq!(
            parse_visible_devices(Some("none")),
            GpuVisibility::NoneVisible
        );
        assert_eq!(
            parse_visible_devices(Some("0,2")),
            GpuVisibility::Only(vec!["0".into(), "2".into()])
        );
        assert_eq!(
            parse_visible_devices(Some(" 1 , 3 ")),
            GpuVisibility::Only(vec!["1".into(), "3".into()])
        );
    }

    #[test]
    fn test_visibility_is_job_scoped() {
        assert!(!GpuVisibility::All.is_job_scoped());
        assert!(GpuVisibility::NoneVisible.is_job_scoped());
        assert!(GpuVisibility::Only(vec!["0".into()]).is_job_scoped());
    }

    #[test]
    fn test_filter_by_index() {
        // The core of the fix: a job given GPU 1 on a 2-GPU node must not be
        // credited with GPU 0's utilisation.
        let all = parse_nvidia_smi_csv(TWO_GPUS);
        let filtered = filter_to_visible(&all, &GpuVisibility::Only(vec!["1".into()]));

        assert_eq!(filtered.gpu_count(), 1);
        assert_eq!(filtered.indices, vec![1]);
        assert_eq!(filtered.utilization, vec![90.0]);
        assert_eq!(filtered.memory_used, vec![4096 * 1024 * 1024]);
        assert!(filtered.job_scoped);
    }

    #[test]
    fn test_filter_by_full_uuid() {
        let all = parse_nvidia_smi_csv(TWO_GPUS);
        let filtered = filter_to_visible(
            &all,
            &GpuVisibility::Only(vec!["GPU-bbbb1111-2222-3333-4444-555555555555".into()]),
        );

        assert_eq!(filtered.indices, vec![1]);
        assert_eq!(filtered.utilization, vec![90.0]);
    }

    #[test]
    fn test_filter_by_abbreviated_uuid() {
        // CUDA accepts a unique UUID prefix.
        let all = parse_nvidia_smi_csv(TWO_GPUS);
        let filtered = filter_to_visible(&all, &GpuVisibility::Only(vec!["GPU-aaaa".into()]));

        assert_eq!(filtered.indices, vec![0]);
        assert_eq!(filtered.utilization, vec![45.0]);
    }

    #[test]
    fn test_filter_all_keeps_everything_and_is_not_job_scoped() {
        let all = parse_nvidia_smi_csv(TWO_GPUS);
        let filtered = filter_to_visible(&all, &GpuVisibility::All);

        assert_eq!(filtered.gpu_count(), 2);
        assert!(
            !filtered.job_scoped,
            "without CUDA_VISIBLE_DEVICES we cannot claim job scope"
        );
    }

    #[test]
    fn test_filter_none_yields_nothing() {
        let all = parse_nvidia_smi_csv(TWO_GPUS);
        let filtered = filter_to_visible(&all, &GpuVisibility::NoneVisible);

        assert_eq!(filtered.gpu_count(), 0);
        assert!(filtered.job_scoped);
    }

    #[test]
    fn test_filter_unknown_token_matches_nothing() {
        let all = parse_nvidia_smi_csv(TWO_GPUS);
        let filtered = filter_to_visible(&all, &GpuVisibility::Only(vec!["7".into()]));
        assert_eq!(filtered.gpu_count(), 0);
    }

    #[test]
    fn test_filter_keeps_all_vectors_aligned() {
        let all = parse_nvidia_smi_csv(TWO_GPUS);
        let filtered = filter_to_visible(&all, &GpuVisibility::Only(vec!["0".into()]));

        let n = filtered.gpu_count();
        assert_eq!(filtered.indices.len(), n);
        assert_eq!(filtered.memory_used.len(), n);
        assert_eq!(filtered.memory_total.len(), n);
        assert_eq!(filtered.temperature.len(), n);
        assert_eq!(filtered.power.len(), n);
    }

    #[test]
    fn test_sampler_never_forks_when_no_gpus_assigned() {
        // CUDA_VISIBLE_DEVICES="" means this job has no GPUs, so there is no
        // reason to run nvidia-smi even once.
        let mut sampler = GpuSampler::with_visibility(5.0, GpuVisibility::NoneVisible);
        let stats = sampler.sample();

        assert_eq!(stats.gpu_count(), 0);
        assert!(stats.job_scoped);
        assert!(sampler.is_disabled());
    }

    #[test]
    fn test_sampler_disables_itself_without_gpus() {
        // On a machine with no nvidia-smi (CI, macOS dev boxes, most of Gadi)
        // the first sample must mark the sampler disabled so we stop forking.
        let mut sampler = GpuSampler::with_visibility(5.0, GpuVisibility::All);
        let stats = sampler.sample();

        if sampler.is_disabled() {
            assert_eq!(stats.gpu_count(), 0);
            assert_eq!(sampler.sample().gpu_count(), 0);
        }
    }

    #[test]
    fn test_sampler_caches_between_polls() {
        // A very long interval means the second call must not re-poll.
        let mut sampler = GpuSampler::with_visibility(3600.0, GpuVisibility::All);
        sampler.available = Some(true);
        sampler.cached = GpuStats {
            indices: vec![0],
            uuids: vec!["GPU-x".into()],
            utilization: vec![50.0],
            memory_used: vec![1],
            memory_total: vec![2],
            temperature: vec![3.0],
            power: vec![4.0],
            job_scoped: false,
        };
        sampler.last_poll = Some(Instant::now());

        assert_eq!(sampler.sample().utilization, vec![50.0]);
    }
}
