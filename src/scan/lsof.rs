//! macOS backend: `lsof -F` for listeners and cwd, `ps` for the process table.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};

use super::{split_host_port, Listener, Scanner, Snapshot};
use crate::process::run_with_timeout;

pub struct LsofScanner;

impl Scanner for LsofScanner {
    fn name(&self) -> &'static str {
        "lsof"
    }

    fn scan(&self, timeout: Duration) -> Result<Snapshot> {
        let listeners = parse_listeners(&list_listeners(timeout)?);
        let pids: Vec<u32> = listeners
            .iter()
            .map(|l| l.pid)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let processes = super::ps::process_table(timeout).context("process table")?;
        let cwds = if pids.is_empty() {
            HashMap::new()
        } else {
            parse_cwds(&list_cwds(&pids, timeout)?)
        };
        Ok(Snapshot {
            listeners,
            processes,
            cwds,
        })
    }
}

fn list_listeners(timeout: Duration) -> Result<String> {
    let mut command = Command::new("lsof");
    command.args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pcn"]);
    // lsof exits 1 when nothing matches; that is an empty result, not an error.
    let output = run_with_timeout(command, timeout)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn list_cwds(pids: &[u32], timeout: Duration) -> Result<String> {
    let joined = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut command = Command::new("lsof");
    command.args(["-a", "-p", &joined, "-d", "cwd", "-Fn"]);
    let output = run_with_timeout(command, timeout)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse `lsof -F pcn` output. Records: `p<pid>`, `c<command>`, `f<fd>`, `n<addr>`.
/// A process header is followed by its files; each `n` record under it is a listener.
/// Duplicate `(pid, port, addr)` rows (several fds on one socket) collapse to one.
pub fn parse_listeners(text: &str) -> Vec<Listener> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut pid: Option<u32> = None;
    let mut name = String::new();
    for line in text.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match tag {
            'p' => {
                pid = value.parse().ok();
                name.clear();
            }
            'c' => name = value.to_string(),
            'n' => {
                let (Some(pid), Some((addr, port))) = (pid, split_host_port(value)) else {
                    continue;
                };
                if seen.insert((pid, port, addr.clone())) {
                    out.push(Listener {
                        pid,
                        port,
                        addr,
                        process_name: name.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Parse `lsof -a -p … -d cwd -Fn`: `p<pid>` then `fcwd` then `n<path>`.
pub fn parse_cwds(text: &str) -> HashMap<u32, PathBuf> {
    let mut out = HashMap::new();
    let mut pid: Option<u32> = None;
    for line in text.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match tag {
            'p' => pid = value.parse().ok(),
            'n' => {
                if let Some(pid) = pid {
                    out.insert(pid, PathBuf::from(value));
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTENERS: &str = "p558\ncrapportd\nf15\nn*:49340\nf16\nn*:49340\nf21\nn*:65242\n\
p1297\ncredis-server\nf6\nn127.0.0.1:6379\nf7\nn[::1]:6379\n\
p1315\ncpostgres\nf7\nn[::1]:5432\nf8\nn127.0.0.1:5432\n\
p4242\ncPython\nf3\nn*:8000\n";

    #[test]
    fn listeners_collapse_duplicate_fds_but_keep_distinct_addresses() {
        let listeners = parse_listeners(LISTENERS);
        let rapportd: Vec<_> = listeners.iter().filter(|l| l.pid == 558).collect();
        assert_eq!(rapportd.len(), 2, "two distinct ports, three fds");
        let redis: Vec<_> = listeners.iter().filter(|l| l.pid == 1297).collect();
        assert_eq!(redis.len(), 2);
        assert_eq!(redis[0].addr, "127.0.0.1");
        assert_eq!(redis[1].addr, "[::1]");
        assert_eq!(redis[0].process_name, "redis-server");
        let py = listeners.iter().find(|l| l.pid == 4242).unwrap();
        assert_eq!(
            (py.port, py.addr.as_str(), py.process_name.as_str()),
            (8000, "*", "Python")
        );
    }

    #[test]
    fn cwd_records_map_pid_to_path() {
        let text = "p1297\nfcwd\nn/opt/homebrew/var/db/redis\np1315\nfcwd\nn/opt/homebrew/var/postgresql@16\n";
        let cwds = parse_cwds(text);
        assert_eq!(cwds[&1297], PathBuf::from("/opt/homebrew/var/db/redis"));
        assert_eq!(
            cwds[&1315],
            PathBuf::from("/opt/homebrew/var/postgresql@16")
        );
    }

    #[test]
    fn empty_output_yields_no_listeners() {
        assert!(parse_listeners("").is_empty());
    }
}
