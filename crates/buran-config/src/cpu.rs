//! Effective CPU detection for automatic worker sizing.
//!
//! `processes: auto` (and an omitted `processes:`) resolve to the host's
//! parallelism, clamped by the cgroup CPU quota when one is set so the default
//! does the right thing inside a `--cpus`-limited container instead of seeing
//! every core on the host.

/// Number of workers for `processes: auto`: host parallelism capped by the
/// cgroup CPU quota (if any). Never below 1. No upper cap — a 512-core box
/// gets 512 workers by design.
pub fn auto_worker_count() -> u32 {
    let parallelism = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let effective = match cgroup_cpu_quota() {
        Some(quota) => parallelism.min(quota),
        None => parallelism,
    };
    u32::try_from(effective.max(1)).unwrap_or(u32::MAX)
}

/// CPU quota in whole cores from cgroup limits, rounded up. `None` when
/// unlimited or the limits are unreadable (bare metal, unknown layout) — the
/// caller then falls back to raw parallelism, i.e. no clamp.
///
/// `available_parallelism()` reads `sched_getaffinity`, which reflects cpuset
/// pinning but NOT the CFS quota (`docker --cpus`, k8s `limits.cpu`), so the
/// quota has to be read separately. cgroup v2 is tried first, then v1.
fn cgroup_cpu_quota() -> Option<usize> {
    // cgroup v2: "<quota> <period>" in microseconds, or "max <period>".
    if let Ok(raw) = std::fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        let mut parts = raw.split_whitespace();
        let quota = parts.next()?;
        if quota == "max" {
            return None; // unlimited
        }
        let quota: u64 = quota.parse().ok()?;
        let period: u64 = parts.next().unwrap_or("100000").parse().ok()?;
        return cores_from_quota(quota, period);
    }

    // cgroup v1: quota and period live in separate files. -1 quota = unlimited.
    let quota: i64 =
        std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us").ok()?.trim().parse().ok()?;
    if quota <= 0 {
        return None;
    }
    let period: i64 =
        std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us").ok()?.trim().parse().ok()?;
    cores_from_quota(quota as u64, period as u64)
}

/// Whole cores a `quota/period` (microseconds) allowance is worth, rounded up
/// so a fractional allowance (`--cpus=1.5`) still gets a whole extra worker.
fn cores_from_quota(quota: u64, period: u64) -> Option<usize> {
    if quota == 0 || period == 0 {
        return None;
    }
    Some(quota.div_ceil(period).max(1) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_worker_count_is_at_least_one() {
        assert!(auto_worker_count() >= 1);
    }

    #[test]
    fn cores_round_up_fractional_quota() {
        assert_eq!(cores_from_quota(100_000, 100_000), Some(1)); // 1.0 core
        assert_eq!(cores_from_quota(150_000, 100_000), Some(2)); // 1.5 -> 2
        assert_eq!(cores_from_quota(250_000, 100_000), Some(3)); // 2.5 -> 3
        assert_eq!(cores_from_quota(50_000, 100_000), Some(1)); // 0.5 -> floor 1
    }

    #[test]
    fn cores_reject_degenerate_values() {
        assert_eq!(cores_from_quota(0, 100_000), None);
        assert_eq!(cores_from_quota(100_000, 0), None);
    }
}
