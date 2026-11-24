//! copybera runs only inside a container whose memory cgroup refuses past a hard cap. A long history walked uncapped
//! can take a workstation's memory faster than anyone can stop it, and a watchdog that samples RSS reacts too late: it
//! is not a cap. So the binary checks before it does anything (`--help` included): the memory limit of its cgroup (the
//! smallest `memory.max` from its own cgroup up to the root, or cgroup v1's `memory.limit_in_bytes`) must be below
//! the host's RAM (`MemTotal`). macOS has no cgroup, so copybera refuses there. `COPYBERA_ALLOW_UNCAPPED=1` runs it
//! anyway, with a warning.

use std::fs;
use std::path::Path;

/// The variable that lets copybera run without a cap.
pub const ALLOW_UNCAPPED: &str = "COPYBERA_ALLOW_UNCAPPED";

/// The exit code of a refused start (EX_CONFIG): the runner tells a refusal from a failed run.
pub const REFUSED_EXIT: u8 = 78;

/// A cap copybera may run under: the cgroup's limit and the host's RAM, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cap {
    pub limit: u64,
    pub ram: u64,
}

/// The cap copybera runs under, read from the filesystem at `root` (`/`; a test passes a folder of fixtures), or
/// why there is none.
pub fn cap(root: &Path) -> Result<Cap, String> {
    let ram = mem_total(root).ok_or("no /proc/meminfo MemTotal (not Linux?)")?;
    let limit = cgroup_limit(root).ok_or("its cgroup sets no memory limit")?;
    if limit >= ram {
        return Err(format!(
            "its cgroup's memory limit ({limit} bytes) is not below the host's RAM ({ram} bytes)"
        ));
    }
    Ok(Cap { limit, ram })
}

/// Ok when copybera may start: under a cap, or allowed uncapped (`allow`: `COPYBERA_ALLOW_UNCAPPED` is `1`), with a
/// warning to print. Err is the refusal.
pub fn require(root: &Path, allow: bool) -> Result<Option<String>, String> {
    match cap(root) {
        Ok(_) => Ok(None),
        Err(why) if allow => Ok(Some(format!(
            "copybera: warning: running without a memory cap ({ALLOW_UNCAPPED}=1): {why}"
        ))),
        Err(why) => Err(format!(
            "copybera: refused: copybera runs only inside a memory-capped container \
             (docker run --memory=X --memory-swap=X, X below the host's RAM), and {why}; \
             set {ALLOW_UNCAPPED}=1 to run it without one"
        )),
    }
}

fn mem_total(root: &Path) -> Option<u64> {
    let text = fs::read_to_string(root.join("proc/meminfo")).ok()?;
    let line = text.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kib.checked_mul(1024)
}

/// The smallest memory limit on the way from this process's cgroup to the hierarchy's root. A folder that is not
/// there (a container sees its own cgroup as the root, under the host's path) is skipped.
fn cgroup_limit(root: &Path) -> Option<u64> {
    let text = fs::read_to_string(root.join("proc/self/cgroup")).ok()?;
    let mut smallest: Option<u64> = None;
    for line in text.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(_), Some(controllers), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (base, file) = if controllers.is_empty() {
            (root.join("sys/fs/cgroup"), "memory.max")
        } else if controllers.split(',').any(|c| c == "memory") {
            (root.join("sys/fs/cgroup/memory"), "memory.limit_in_bytes")
        } else {
            continue;
        };
        let mut dir = base.join(path.trim_start_matches('/'));
        loop {
            if let Some(limit) = fs::read_to_string(dir.join(file))
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok())
            {
                smallest = Some(smallest.map_or(limit, |s| s.min(limit)));
            }
            if dir == base || !dir.pop() {
                break;
            }
        }
    }
    smallest
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn host(
        meminfo: Option<&str>,
        cgroup: Option<&str>,
        files: &[(&str, &str)],
    ) -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let write = |path: &str, text: &str| {
            let path = tmp.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        if let Some(meminfo) = meminfo {
            write("proc/meminfo", meminfo);
        }
        if let Some(cgroup) = cgroup {
            write("proc/self/cgroup", cgroup);
        }
        for (path, text) in files {
            write(path, text);
        }
        tmp
    }

    const RAM_16G: &str = "MemTotal:       16424484 kB\nMemFree:  1000 kB\n";

    #[test]
    fn a_container_capped_below_the_hosts_ram_may_run() {
        let h = host(
            Some(RAM_16G),
            Some("0::/\n"),
            &[("sys/fs/cgroup/memory.max", "1610612736\n")],
        );
        assert_eq!(
            cap(h.path()),
            Ok(Cap {
                limit: 1_610_612_736,
                ram: 16_424_484 * 1024
            })
        );
        assert_eq!(require(h.path(), false), Ok(None));
    }

    #[test]
    fn an_uncapped_cgroup_is_refused_unless_allowed() {
        let h = host(
            Some(RAM_16G),
            Some("0::/\n"),
            &[("sys/fs/cgroup/memory.max", "max\n")],
        );
        let refused = require(h.path(), false).unwrap_err();
        assert!(refused.contains("sets no memory limit"), "{refused}");
        assert!(refused.contains(ALLOW_UNCAPPED), "{refused}");
        let warned = require(h.path(), true).unwrap().unwrap();
        assert!(warned.contains("without a memory cap"), "{warned}");
    }

    #[test]
    fn a_cap_at_or_over_the_hosts_ram_is_no_cap() {
        let h = host(
            Some(RAM_16G),
            Some("0::/\n"),
            &[("sys/fs/cgroup/memory.max", &(32 * GIB).to_string())],
        );
        assert!(require(h.path(), false)
            .unwrap_err()
            .contains("not below the host's RAM"));
    }

    #[test]
    fn a_host_with_no_cgroup_is_refused() {
        // macOS: no /proc at all.
        let h = host(None, None, &[]);
        assert!(require(h.path(), false).unwrap_err().contains("MemTotal"));
        let h = host(Some(RAM_16G), None, &[]);
        assert!(require(h.path(), false).is_err());
    }

    #[test]
    fn the_smallest_limit_on_the_way_to_the_root_holds() {
        // A nested cgroup with no limit of its own under a capped parent; the hierarchy's root has no memory.max.
        let h = host(
            Some(RAM_16G),
            Some("0::/jobs/copybera\n"),
            &[
                ("sys/fs/cgroup/jobs/copybera/memory.max", "max\n"),
                ("sys/fs/cgroup/jobs/memory.max", &(2 * GIB).to_string()),
            ],
        );
        assert_eq!(cap(h.path()).unwrap().limit, 2 * GIB);
    }

    #[test]
    fn cgroup_v1_reads_the_memory_controllers_limit() {
        let h = host(
            Some(RAM_16G),
            Some("12:cpu,cpuacct:/docker/abc\n4:memory:/docker/abc\n"),
            &[(
                "sys/fs/cgroup/memory/memory.limit_in_bytes",
                &GIB.to_string(),
            )],
        );
        assert_eq!(cap(h.path()).unwrap().limit, GIB);
        let h = host(
            Some(RAM_16G),
            Some("4:memory:/\n"),
            &[(
                "sys/fs/cgroup/memory/memory.limit_in_bytes",
                "9223372036854771712\n",
            )],
        );
        assert!(
            require(h.path(), false).is_err(),
            "v1's unlimited is a number over the RAM"
        );
    }
}
