//! Node identity.
//!
//! Every record carries the hostname so a merged, multi-node view can attribute
//! each reading to the node it came from. The short form (up to the first dot)
//! is used throughout: it is what PBS puts in `PBS_NODEFILE`, what users see in
//! `qstat`, and it keeps filenames manageable.

/// The short hostname of this node, e.g. `gadi-cpu-clx-0123`.
///
/// Falls back through several sources so this works on a compute node, in a
/// container, and on a macOS development machine. Returns `"unknown"` rather
/// than failing — a missing hostname should degrade the telemetry, not stop it.
pub fn hostname() -> String {
    // Most reliable on Linux: the kernel's own idea of the hostname.
    if let Ok(name) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        if let Some(short) = shorten(&name) {
            return short;
        }
    }

    // Set by bash, though not always exported.
    if let Ok(name) = std::env::var("HOSTNAME") {
        if let Some(short) = shorten(&name) {
            return short;
        }
    }

    // macOS and anything else with the utility on PATH.
    if let Ok(output) = std::process::Command::new("hostname").output() {
        if output.status.success() {
            let name = String::from_utf8_lossy(&output.stdout);
            if let Some(short) = shorten(&name) {
                return short;
            }
        }
    }

    "unknown".to_string()
}

/// Trim whitespace and drop any domain suffix. `None` if nothing is left.
fn shorten(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let short = trimmed.split('.').next().unwrap_or(trimmed);
    if short.is_empty() {
        None
    } else {
        Some(short.to_string())
    }
}

/// A form of `name` that is safe to embed in a filename.
///
/// Hostnames are already restricted to a safe character set in practice, but a
/// stray `/` from a misconfigured node would silently write telemetry into the
/// wrong directory — or fail to write it at all.
pub fn sanitise_for_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();

    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shorten() {
        assert_eq!(
            shorten("gadi-cpu-clx-0123\n").as_deref(),
            Some("gadi-cpu-clx-0123")
        );
        assert_eq!(
            shorten("gadi-cpu-clx-0123.gadi.nci.org.au").as_deref(),
            Some("gadi-cpu-clx-0123")
        );
        assert_eq!(shorten("  host  ").as_deref(), Some("host"));
        assert_eq!(shorten(""), None);
        assert_eq!(shorten("   \n"), None);
        assert_eq!(shorten("."), None);
    }

    #[test]
    fn test_sanitise_for_filename() {
        assert_eq!(
            sanitise_for_filename("gadi-cpu-clx-0123"),
            "gadi-cpu-clx-0123"
        );
        assert_eq!(
            sanitise_for_filename("node.example.com"),
            "node.example.com"
        );
        // A slash would otherwise redirect the output into another directory.
        assert_eq!(sanitise_for_filename("bad/name"), "bad_name");
        assert_eq!(sanitise_for_filename("../escape"), ".._escape");
        assert_eq!(sanitise_for_filename(""), "unknown");
    }

    #[test]
    fn test_hostname_is_short_and_never_empty() {
        let h = hostname();
        assert!(!h.is_empty(), "hostname must always resolve to something");
        assert!(
            !h.contains('.'),
            "hostname must be the short form, got {h:?}"
        );
        assert_eq!(sanitise_for_filename(&h), h, "hostname must be file-safe");
    }
}
