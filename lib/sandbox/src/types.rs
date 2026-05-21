use serde::{Deserialize, Serialize};

/// Project bootstrap mode for the first `/initialize` call. Mirrors the
/// wire format expected by `images/sandbox/server/src/main.rs::initialize`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SandboxMode {
    Scratch,
    /// Clones `repo_url` into `<project>/repos/<name>` and scans for
    /// CLAUDE.md / `.claude/commands/*.md`.
    Repo {
        repo_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
}

/// K8s applies these to pod resources and the PVC; the local backend logs
/// them but doesn't enforce them.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SandboxSpecs {
    /// e.g. `"500m"` or `"2"`.
    pub cpu: String,
    /// e.g. `"512Mi"` or `"2Gi"`.
    pub memory: String,
    /// e.g. `"10Gi"`.
    pub disk_size: String,
}

/// Parses K8s-style quantity strings (e.g. `"10Gi"`, `"1.5Gi"`,
/// `"500m"`, `"2"`) into raw bytes. Returns `None` for unrecognised
/// units so callers can fall back to a string-equality check rather
/// than silently misinterpret a typo. Suffix support is intentionally
/// narrow — we only ever set disk sizes from `SandboxSpecs::disk_size`,
/// which K8s itself validates against the same vocabulary. Mantissas
/// may be integer or decimal (`1.5Gi` is a valid K8s quantity); we
/// route through `f64` for the multiplication since byte counts well
/// below `2^53` (the f64 integer-precision ceiling, ~8 PiB) are
/// exact, comfortably above any realistic sandbox disk request.
/// Shared between the core service layer (resize guardrails) and the
/// k8s admin client (PVC patch idempotency) so both compare sizes
/// identically.
pub fn parse_k8s_quantity(s: &str) -> Option<u128> {
    let trimmed = s.trim();
    let (num_str, mult): (&str, u128) = if let Some(prefix) = trimmed.strip_suffix("Ki") {
        (prefix, 1024)
    } else if let Some(prefix) = trimmed.strip_suffix("Mi") {
        (prefix, 1024u128.pow(2))
    } else if let Some(prefix) = trimmed.strip_suffix("Gi") {
        (prefix, 1024u128.pow(3))
    } else if let Some(prefix) = trimmed.strip_suffix("Ti") {
        (prefix, 1024u128.pow(4))
    } else if let Some(prefix) = trimmed.strip_suffix("Pi") {
        (prefix, 1024u128.pow(5))
    } else if let Some(prefix) = trimmed.strip_suffix('K') {
        (prefix, 1_000)
    } else if let Some(prefix) = trimmed.strip_suffix('M') {
        (prefix, 1_000_000)
    } else if let Some(prefix) = trimmed.strip_suffix('G') {
        (prefix, 1_000_000_000)
    } else if let Some(prefix) = trimmed.strip_suffix('T') {
        (prefix, 1_000_000_000_000)
    } else if let Some(prefix) = trimmed.strip_suffix('P') {
        (prefix, 1_000_000_000_000_000)
    } else {
        (trimmed, 1)
    };
    let n: f64 = num_str.trim().parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    let bytes = n * mult as f64;
    if !bytes.is_finite() || bytes < 0.0 {
        return None;
    }
    Some(bytes as u128)
}

/// Mirrors the Kubernetes view; the local backend fills `None` for
/// cluster-only fields.
#[derive(Clone, Debug, Serialize)]
pub struct Sandbox {
    pub name: String,

    /// K8s: `"Provisioning"` then `"Ready"`. Local: `"Ready"` once the
    /// spawned process is accepting connections.
    pub phase: String,

    pub ready: bool,

    /// e.g. `http://127.0.0.1:34567`. `None` until ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// K8s-only headless service FQDN.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_fqdn: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_k8s_quantity_handles_binary_suffixes() {
        assert_eq!(parse_k8s_quantity("1Ki"), Some(1024));
        assert_eq!(parse_k8s_quantity("1Mi"), Some(1024 * 1024));
        assert_eq!(
            parse_k8s_quantity("10Gi"),
            Some(10u128 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn parse_k8s_quantity_handles_decimal_suffixes() {
        assert_eq!(parse_k8s_quantity("1K"), Some(1_000));
        assert_eq!(parse_k8s_quantity("2M"), Some(2_000_000));
    }

    #[test]
    fn parse_k8s_quantity_handles_bare_integer() {
        assert_eq!(parse_k8s_quantity("42"), Some(42));
    }

    #[test]
    fn parse_k8s_quantity_returns_none_for_garbage() {
        assert_eq!(parse_k8s_quantity("not a quantity"), None);
        assert_eq!(parse_k8s_quantity("10X"), None);
    }

    #[test]
    fn parse_k8s_quantity_orders_decimal_units_correctly() {
        // Catches the `10G` vs `10Gi` ordering — 10Gi > 10G.
        let g = parse_k8s_quantity("10G").unwrap();
        let gi = parse_k8s_quantity("10Gi").unwrap();
        assert!(gi > g);
    }

    #[test]
    fn parse_k8s_quantity_handles_fractional_mantissa() {
        // K8s accepts `1.5Gi`; rejecting it would misclassify a valid
        // disk grow as a shrink via the string-equality fallback.
        let one_and_a_half_gi = parse_k8s_quantity("1.5Gi").unwrap();
        let one_gi = parse_k8s_quantity("1Gi").unwrap();
        let two_gi = parse_k8s_quantity("2Gi").unwrap();
        assert!(one_and_a_half_gi > one_gi);
        assert!(one_and_a_half_gi < two_gi);
        assert_eq!(one_and_a_half_gi, 1024u128 * 1024 * 1024 * 3 / 2);
    }

    #[test]
    fn parse_k8s_quantity_handles_sub_unit_fractions() {
        // `0.5G` → 500_000_000 bytes; this used to fall through to the
        // string-fallback path because the integer parser rejected
        // anything with a decimal point.
        assert_eq!(parse_k8s_quantity("0.5G"), Some(500_000_000));
    }

    #[test]
    fn parse_k8s_quantity_rejects_negative_mantissa() {
        // K8s quantities are non-negative; reject so disk_size_grows
        // doesn't treat a typo as a legitimate shrink-from-negative.
        assert_eq!(parse_k8s_quantity("-1Gi"), None);
    }
}
