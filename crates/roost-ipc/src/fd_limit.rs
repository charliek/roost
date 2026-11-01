//! Raising the soft `RLIMIT_NOFILE` a long-running Roost process starts
//! with. Every tab costs a handful of descriptors, and macOS hands a
//! process a soft limit of 256, which caps a session at about 60 tabs.

/// The soft limit a raise aims for. It is macOS's `OPEN_MAX`: a soft
/// limit of `RLIM_INFINITY` or above `kern.maxfilesperproc` fails there.
/// It is a cap rather than the hard limit because every child inherits
/// the soft limit and a PTY spawn can't restore a lower one for the
/// shell: a soft limit in the hundreds of thousands slows programs that
/// close every fd up to it, and `select()` breaks past 1024 anyway.
const OPEN_FILE_TARGET: u64 = 10240;

/// Raise this process's soft `RLIMIT_NOFILE` to
/// `max(soft, min(hard, 10240))`, never lowering it. Returns the old and
/// the new soft limit, equal when nothing needed raising.
pub fn raise_open_file_limit() -> std::io::Result<(u64, u64)> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes one `rlimit`, which is what it is given.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let soft = limit.rlim_cur;
    let Some(target) = open_file_target(soft, limit.rlim_max) else {
        return Ok((soft, soft));
    };
    let raised = libc::rlimit {
        rlim_cur: target,
        rlim_max: limit.rlim_max,
    };
    // SAFETY: `setrlimit` reads one `rlimit`, which is what it is given.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((soft, target))
}

/// The soft limit to set, or `None` when `soft` is already there.
fn open_file_target(soft: u64, hard: u64) -> Option<u64> {
    let target = soft.max(hard.min(OPEN_FILE_TARGET));
    (target > soft).then_some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_low_soft_limit_rises_to_the_target_or_the_hard_limit() {
        assert_eq!(open_file_target(256, u64::MAX), Some(10240));
        assert_eq!(open_file_target(256, 524_288), Some(10240));
        assert_eq!(open_file_target(1024, 4096), Some(4096));
        assert_eq!(open_file_target(256, 10240), Some(10240));
    }

    #[test]
    fn a_soft_limit_at_or_above_the_target_or_the_hard_limit_is_left_alone() {
        assert_eq!(open_file_target(10240, u64::MAX), None);
        assert_eq!(open_file_target(65_536, 524_288), None);
        assert_eq!(open_file_target(1024, 1024), None);
        assert_eq!(open_file_target(u64::MAX, u64::MAX), None);
    }
}
