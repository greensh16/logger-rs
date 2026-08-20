//! Network counters from `/proc/net/dev`.
//!
//! These are **node-wide**: `/proc/net/dev` has no notion of which job owns
//! which byte. On an exclusive node that is a fair approximation of the job's
//! traffic; on a shared queue it includes everyone else's. Samples carry a
//! `net_is_node_scoped` flag so the dashboard can say so.

use anyhow::Result;

/// Network statistics aggregated across all interfaces.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkStats {
    pub recv_bytes: u64,
    pub sent_bytes: u64,
    pub recv_packets: u64,
    pub sent_packets: u64,
}

/// Read network statistics from `/proc/net/dev`.
pub fn read_network_stats() -> Result<NetworkStats> {
    #[cfg(target_os = "linux")]
    {
        let content = std::fs::read_to_string("/proc/net/dev")?;
        Ok(parse_proc_net_dev(&content, should_count_interface))
    }

    #[cfg(not(target_os = "linux"))]
    {
        Ok(NetworkStats::default())
    }
}

/// Decide whether an interface's counters should be included.
///
/// Bonded setups are the reason this is not just "skip lo": a bond and its
/// slave interfaces both appear in /proc/net/dev and carry the same traffic, so
/// counting all of them double-counts. Slaves are identified by the presence of
/// `/sys/class/net/{iface}/master`.
#[cfg(target_os = "linux")]
fn should_count_interface(iface: &str) -> bool {
    if !is_physical_interface_name(iface) {
        return false;
    }
    // A slave of a bond/bridge — its traffic is already counted on the master.
    !std::path::Path::new(&format!("/sys/class/net/{}/master", iface)).exists()
}

/// Name-based filter for interfaces that never carry real job traffic.
pub fn is_physical_interface_name(iface: &str) -> bool {
    const SKIP_PREFIXES: [&str; 7] = ["lo", "veth", "docker", "virbr", "br-", "tun", "tap"];

    if iface.is_empty() {
        return false;
    }

    !SKIP_PREFIXES
        .iter()
        .any(|prefix| iface == *prefix || iface.starts_with(prefix))
}

/// Parse the body of `/proc/net/dev`, including only interfaces `keep` accepts.
///
/// Layout (two header lines, then one line per interface):
/// ```text
/// Inter-|   Receive                    |  Transmit
///  face |bytes packets errs drop fifo frame compressed multicast|bytes packets ...
///     lo: 1234567 12345 0 0 0 0 0 0  1234567 12345 ...
/// ```
pub fn parse_proc_net_dev<F>(content: &str, keep: F) -> NetworkStats
where
    F: Fn(&str) -> bool,
{
    let mut stats = NetworkStats::default();

    for line in content.lines().skip(2) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Some((iface, rest)) = line.split_once(':') else {
            continue;
        };
        let iface = iface.trim();

        if !keep(iface) {
            continue;
        }

        let values: Vec<&str> = rest.split_whitespace().collect();
        if values.len() < 10 {
            continue;
        }

        // Receive: bytes(0), packets(1). Transmit: bytes(8), packets(9).
        if let Ok(v) = values[0].parse::<u64>() {
            stats.recv_bytes = stats.recv_bytes.saturating_add(v);
        }
        if let Ok(v) = values[1].parse::<u64>() {
            stats.recv_packets = stats.recv_packets.saturating_add(v);
        }
        if let Ok(v) = values[8].parse::<u64>() {
            stats.sent_bytes = stats.sent_bytes.saturating_add(v);
        }
        if let Ok(v) = values[9].parse::<u64>() {
            stats.sent_packets = stats.sent_packets.saturating_add(v);
        }
    }

    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000000   1000    0    0    0     0          0         0  1000000    1000    0    0    0     0       0          0
  eth0: 2000000   2000    0    0    0     0          0         0  3000000    3000    0    0    0     0       0          0
  ib0:  4000000   4000    0    0    0     0          0         0  5000000    5000    0    0    0     0       0          0
";

    #[test]
    fn test_parse_proc_net_dev_skips_loopback() {
        let stats = parse_proc_net_dev(SAMPLE, is_physical_interface_name);

        // eth0 + ib0, with lo excluded.
        assert_eq!(stats.recv_bytes, 2_000_000 + 4_000_000);
        assert_eq!(stats.sent_bytes, 3_000_000 + 5_000_000);
        assert_eq!(stats.recv_packets, 2_000 + 4_000);
        assert_eq!(stats.sent_packets, 3_000 + 5_000);
    }

    #[test]
    fn test_parse_proc_net_dev_respects_filter() {
        // Simulating a bond: count only the master, not the slaves.
        let stats = parse_proc_net_dev(SAMPLE, |iface| iface == "eth0");
        assert_eq!(stats.recv_bytes, 2_000_000);
        assert_eq!(stats.sent_bytes, 3_000_000);

        let none = parse_proc_net_dev(SAMPLE, |_| false);
        assert_eq!(none, NetworkStats::default());
    }

    #[test]
    fn test_is_physical_interface_name() {
        assert!(is_physical_interface_name("eth0"));
        assert!(is_physical_interface_name("ib0"));
        assert!(is_physical_interface_name("bond0"));
        assert!(is_physical_interface_name("enp1s0"));

        assert!(!is_physical_interface_name("lo"));
        assert!(!is_physical_interface_name("veth1234"));
        assert!(!is_physical_interface_name("docker0"));
        assert!(!is_physical_interface_name("br-abc123"));
        assert!(!is_physical_interface_name("virbr0"));
        assert!(!is_physical_interface_name(""));
    }

    #[test]
    fn test_parse_proc_net_dev_tolerates_truncated_lines() {
        let content = "h1\nh2\n  eth0: 1 2 3\n";
        assert_eq!(
            parse_proc_net_dev(content, is_physical_interface_name),
            NetworkStats::default()
        );
    }

    #[test]
    fn test_network_stats_default() {
        let stats = NetworkStats::default();
        assert_eq!(stats.recv_bytes, 0);
        assert_eq!(stats.sent_bytes, 0);
    }

    #[test]
    fn test_read_network_stats_succeeds() {
        // Previously this asserted `recv_bytes >= 0` on a u64, which is
        // vacuously true. Just check the call works on this platform.
        assert!(read_network_stats().is_ok());
    }
}
