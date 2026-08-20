//! CPU percentage calculations from cgroup CPU-time counters.

/// Calculate per-CPU busy percentage from `cpuacct.usage_percpu` deltas.
///
/// # Arguments
/// * `prev` - Previous per-CPU values (nanoseconds)
/// * `curr` - Current per-CPU values (nanoseconds)
/// * `dt_sec` - Wall-clock time between the two measurements
///
/// # Returns
/// Vector of CPU percentages (0-100), one per CPU **on the node** — not per
/// allowed CPU. Index `i` is node CPU `i`, which is what makes it safe to look
/// up by CPU id.
pub fn calculate_percpu_busy_pct(prev: &[u64], curr: &[u64], dt_sec: f64) -> Vec<f64> {
    let max_len = std::cmp::max(prev.len(), curr.len());
    let mut result = Vec::with_capacity(max_len);

    for i in 0..max_len {
        let prev_ns = prev.get(i).copied().unwrap_or(0);
        let curr_ns = curr.get(i).copied().unwrap_or(0);

        // saturating_sub handles both counter wraparound and the cgroup being
        // replaced underneath us (which resets the counters to zero).
        let delta_ns = curr_ns.saturating_sub(prev_ns);

        let pct = if dt_sec > 0.0 {
            (delta_ns as f64 / (dt_sec * 1_000_000_000.0)) * 100.0
        } else {
            0.0
        };

        // A single CPU cannot be busy more than 100% of wall time.
        result.push(pct.clamp(0.0, 100.0));
    }

    result
}

/// Calculate aggregate CPU percentage from a total CPU-time counter.
///
/// Preferred over summing [`calculate_percpu_busy_pct`] because that clamps each
/// core to 100% first, which biases the total downwards whenever timing skew
/// pushes an individual core slightly over. Also the only option on cgroup v2,
/// which exposes no per-CPU breakdown.
///
/// The result is *not* clamped to 100: 400% means four fully busy cores.
pub fn calculate_total_cpu_pct(prev_ns: u64, curr_ns: u64, dt_sec: f64) -> f64 {
    if dt_sec <= 0.0 {
        return 0.0;
    }

    let delta_ns = curr_ns.saturating_sub(prev_ns);
    let pct = (delta_ns as f64 / (dt_sec * 1_000_000_000.0)) * 100.0;

    if pct.is_finite() && pct > 0.0 {
        pct
    } else {
        0.0
    }
}

/// Sum CPU percentages across all CPUs.
pub fn sum_cpu_pct(percpu: &[f64]) -> f64 {
    percpu.iter().sum()
}

/// CPU efficiency: what fraction of the booked cores the job actually used.
///
/// This is the number users care about — "am I wasting my allocation" — and is
/// distinct from `cpu_pct_sum`, which is an absolute core count times 100.
/// Returns 0.0 rather than infinity when no cores are booked.
pub fn cpu_efficiency_pct(cpu_pct_sum: f64, booked_cores: usize) -> f64 {
    if booked_cores == 0 {
        return 0.0;
    }
    cpu_pct_sum / booked_cores as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_percpu_basic() {
        // 1 second interval; CPU 0 used 500ms (50%), CPU 1 used 800ms (80%).
        let prev = vec![1_000_000_000, 2_000_000_000];
        let curr = vec![1_500_000_000, 2_800_000_000];

        let result = calculate_percpu_busy_pct(&prev, &curr, 1.0);

        assert_eq!(result.len(), 2);
        assert!((result[0] - 50.0).abs() < 0.01);
        assert!((result[1] - 80.0).abs() < 0.01);
    }

    #[test]
    fn test_calculate_percpu_zero_delta() {
        let result = calculate_percpu_busy_pct(&[1_000_000_000], &[1_000_000_000], 1.0);
        assert_eq!(result[0], 0.0);
    }

    #[test]
    fn test_calculate_percpu_zero_dt() {
        // A zero-length window must not divide by zero.
        let result = calculate_percpu_busy_pct(&[0], &[1_000_000_000], 0.0);
        assert_eq!(result[0], 0.0);
    }

    #[test]
    fn test_calculate_percpu_wraparound() {
        // Counter reset (cgroup replaced) — treat as no usage rather than a
        // enormous negative-turned-huge delta.
        let prev = vec![u64::MAX - 1_000_000_000];
        let curr = vec![500_000_000];

        let result = calculate_percpu_busy_pct(&prev, &curr, 1.0);
        assert_eq!(result[0], 0.0);
    }

    #[test]
    fn test_sum_cpu_pct() {
        assert_eq!(sum_cpu_pct(&[50.0, 80.0, 30.0, 100.0]), 260.0);
        assert_eq!(sum_cpu_pct(&[]), 0.0);
    }

    #[test]
    fn test_mismatched_lengths() {
        let prev = vec![1_000_000_000];
        let curr = vec![1_500_000_000, 2_000_000_000];

        let result = calculate_percpu_busy_pct(&prev, &curr, 1.0);

        assert_eq!(result.len(), 2);
        assert!((result[0] - 50.0).abs() < 0.01);
        // No previous value for CPU 1, so delta is the whole counter: 2.0s over
        // 1.0s wall = 200%, clamped to 100%.
        assert_eq!(result[1], 100.0);
    }

    #[test]
    fn test_calculate_total_cpu_pct() {
        // 4 cores fully busy for 1 second = 4s of CPU time = 400%.
        assert!((calculate_total_cpu_pct(0, 4_000_000_000, 1.0) - 400.0).abs() < 0.01);
        // Unlike the per-CPU version this must NOT clamp to 100.
        assert!(calculate_total_cpu_pct(0, 4_000_000_000, 1.0) > 100.0);
    }

    #[test]
    fn test_calculate_total_cpu_pct_edge_cases() {
        assert_eq!(calculate_total_cpu_pct(0, 1_000_000_000, 0.0), 0.0);
        assert_eq!(calculate_total_cpu_pct(0, 1_000_000_000, -1.0), 0.0);
        // Counter reset.
        assert_eq!(calculate_total_cpu_pct(5_000_000_000, 0, 1.0), 0.0);
    }

    #[test]
    fn test_cpu_efficiency_pct() {
        // 2 cores' worth of work on a 4-core booking = 50% efficient.
        assert!((cpu_efficiency_pct(200.0, 4) - 50.0).abs() < 0.01);
        // Fully using the booking.
        assert!((cpu_efficiency_pct(400.0, 4) - 100.0).abs() < 0.01);
        // No division by zero.
        assert_eq!(cpu_efficiency_pct(100.0, 0), 0.0);
    }
}
