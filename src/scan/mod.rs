//! Listener enumeration behind one trait, with a backend per platform.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

#[cfg(target_os = "macos")]
pub mod lsof;
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod procfs;
pub mod ps;

/// One TCP socket in LISTEN state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub pid: u32,
    pub port: u16,
    /// Bound address as reported by the OS (`*`, `127.0.0.1`, `[::1]`, …).
    pub addr: String,
    pub process_name: String,
}

/// What we know about a process from the process table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessInfo {
    pub ppid: u32,
    /// Full command line, argv joined with spaces.
    pub command: String,
}

/// Result of one scan: listeners plus the process metadata needed to attribute them.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub listeners: Vec<Listener>,
    /// Whole process table (pid → parent, command line). Needed for ancestry walks.
    pub processes: HashMap<u32, ProcessInfo>,
    /// Working directories of the listening PIDs only.
    pub cwds: HashMap<u32, PathBuf>,
}

impl Snapshot {
    pub fn parent_of(&self, pid: u32) -> Option<u32> {
        self.processes.get(&pid).map(|p| p.ppid)
    }

    pub fn command_of(&self, pid: u32) -> Option<&str> {
        self.processes.get(&pid).map(|p| p.command.as_str())
    }

    /// Ancestors of `pid`, nearest first, excluding `pid` itself. Stops at PID 0/1
    /// or after 64 hops (cycle guard).
    pub fn ancestors(&self, pid: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut cur = pid;
        for _ in 0..64 {
            match self.parent_of(cur) {
                Some(parent) if parent > 1 && parent != cur => {
                    out.push(parent);
                    cur = parent;
                }
                _ => break,
            }
        }
        out
    }
}

pub trait Scanner: Send {
    fn name(&self) -> &'static str;
    /// Enumerate listeners; `timeout` bounds each external command.
    fn scan(&self, timeout: Duration) -> Result<Snapshot>;
}

pub fn platform_scanner() -> Result<Box<dyn Scanner>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(lsof::LsofScanner))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(procfs::ProcfsScanner))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!("unsupported platform: only macOS and Linux are supported in v0.1")
    }
}

/// Split `host:port` where host may be `*`, an IPv4 or a bracketed IPv6.
pub fn split_host_port(s: &str) -> Option<(String, u16)> {
    let (host, port) = s.rsplit_once(':')?;
    let port = port.parse().ok()?;
    Some((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestors_walk_stops_at_init_and_cycles() {
        let mut snap = Snapshot::default();
        snap.processes.insert(
            10,
            ProcessInfo {
                ppid: 5,
                command: String::new(),
            },
        );
        snap.processes.insert(
            5,
            ProcessInfo {
                ppid: 2,
                command: String::new(),
            },
        );
        snap.processes.insert(
            2,
            ProcessInfo {
                ppid: 1,
                command: String::new(),
            },
        );
        assert_eq!(snap.ancestors(10), vec![5, 2]);

        snap.processes.insert(
            7,
            ProcessInfo {
                ppid: 8,
                command: String::new(),
            },
        );
        snap.processes.insert(
            8,
            ProcessInfo {
                ppid: 7,
                command: String::new(),
            },
        );
        assert!(snap.ancestors(7).len() <= 64);
    }

    #[test]
    fn host_port_splitting_handles_ipv6_and_wildcard() {
        assert_eq!(split_host_port("*:3000"), Some(("*".into(), 3000)));
        assert_eq!(split_host_port("[::1]:6379"), Some(("[::1]".into(), 6379)));
        assert_eq!(
            split_host_port("127.0.0.1:5432"),
            Some(("127.0.0.1".into(), 5432))
        );
        assert_eq!(split_host_port("nonsense"), None);
    }
}
