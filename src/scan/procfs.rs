//! Linux backend: `/proc/net/tcp{,6}` for listeners, `/proc/<pid>/{fd,stat,cmdline,cwd}`
//! for ownership and metadata. No external commands.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use super::{Listener, ProcessInfo, Scanner, Snapshot};

pub struct ProcfsScanner;

impl Scanner for ProcfsScanner {
    fn name(&self) -> &'static str {
        "procfs"
    }

    fn scan(&self, _timeout: Duration) -> Result<Snapshot> {
        scan_root(Path::new("/proc"))
    }
}

/// A listening socket before it has been mapped to a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawListener {
    pub inode: u64,
    pub addr: String,
    pub port: u16,
}

pub fn scan_root(proc_root: &Path) -> Result<Snapshot> {
    let mut raw = Vec::new();
    for file in ["net/tcp", "net/tcp6"] {
        if let Ok(text) = fs::read_to_string(proc_root.join(file)) {
            raw.extend(parse_net_tcp(&text));
        }
    }
    let inode_to_listener: HashMap<u64, &RawListener> = raw.iter().map(|l| (l.inode, l)).collect();

    let mut processes = HashMap::new();
    let mut listeners = Vec::new();
    let mut cwds = HashMap::new();
    let entries =
        fs::read_dir(proc_root).with_context(|| format!("read {}", proc_root.display()))?;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        let Some((ppid, name)) = fs::read_to_string(dir.join("stat"))
            .ok()
            .and_then(|s| parse_stat(&s))
        else {
            continue;
        };
        let command = fs::read(dir.join("cmdline"))
            .map(|b| cmdline_to_string(&b))
            .unwrap_or_default();
        processes.insert(pid, ProcessInfo { ppid, command });

        if inode_to_listener.is_empty() {
            continue;
        }
        let mut owns_listener = false;
        if let Ok(fds) = fs::read_dir(dir.join("fd")) {
            for fd in fds.flatten() {
                let Ok(target) = fs::read_link(fd.path()) else {
                    continue;
                };
                let Some(inode) = socket_inode(&target) else {
                    continue;
                };
                if let Some(l) = inode_to_listener.get(&inode) {
                    owns_listener = true;
                    listeners.push(Listener {
                        pid,
                        port: l.port,
                        addr: l.addr.clone(),
                        process_name: name.clone(),
                    });
                }
            }
        }
        if owns_listener {
            if let Ok(cwd) = fs::read_link(dir.join("cwd")) {
                cwds.insert(pid, cwd);
            }
        }
    }
    listeners.sort_by_key(|l| (l.pid, l.port));
    listeners.dedup();
    Ok(Snapshot {
        listeners,
        processes,
        cwds,
    })
}

/// Parse `/proc/net/tcp` or `tcp6`; keep rows in state `0A` (LISTEN).
pub fn parse_net_tcp(text: &str) -> Vec<RawListener> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let _sl = cols.next()?;
            let local = cols.next()?;
            let _remote = cols.next()?;
            let state = cols.next()?;
            if state != "0A" {
                return None;
            }
            // uid at index 7, inode at index 9 (0-based from `sl`).
            let inode = cols.nth(5)?.parse().ok()?;
            let (addr, port) = parse_hex_addr(local)?;
            Some(RawListener { inode, addr, port })
        })
        .collect()
}

/// `0100007F:1F90` → (`127.0.0.1`, 8080); 32-hex-digit form → IPv6 (`[::1]` etc.).
fn parse_hex_addr(s: &str) -> Option<(String, u16)> {
    let (addr, port) = s.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let host = match addr.len() {
        8 => {
            let v = u32::from_str_radix(addr, 16).ok()?;
            let b = v.to_le_bytes();
            if v == 0 {
                "*".to_string()
            } else {
                format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
            }
        }
        32 => {
            // Four little-endian 32-bit words.
            let mut bytes = [0u8; 16];
            for (i, chunk) in addr.as_bytes().chunks(8).enumerate() {
                let word = u32::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
                bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
            }
            let ip = std::net::Ipv6Addr::from(bytes);
            if ip.is_unspecified() {
                "*".to_string()
            } else {
                format!("[{ip}]")
            }
        }
        _ => return None,
    };
    Some((host, port))
}

/// `/proc/<pid>/stat` → (ppid, comm). comm may contain spaces/parentheses, so
/// split on the last `)`.
pub fn parse_stat(text: &str) -> Option<(u32, String)> {
    let start = text.find('(')?;
    let end = text.rfind(')')?;
    let name = text[start + 1..end].to_string();
    let mut rest = text[end + 1..].split_whitespace();
    let _state = rest.next()?;
    let ppid = rest.next()?.parse().ok()?;
    Some((ppid, name))
}

fn cmdline_to_string(bytes: &[u8]) -> String {
    bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn socket_inode(target: &Path) -> Option<u64> {
    let s = target.to_str()?;
    s.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const NET_TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 123456 1 0000000000000000 100 0 0 10 0\n\
   1: 00000000:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 123457 1 0000000000000000 100 0 0 10 0\n\
   2: 0100007F:E1C6 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 123458 1 0000000000000000 20 4 30 10 -1\n";

    const NET_TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 00000000000000000000000001000000:18EB 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 99 1 0000000000000000 100 0 0 10 0\n";

    #[test]
    fn keeps_only_listen_rows_and_decodes_ipv4() {
        let rows = parse_net_tcp(NET_TCP);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            RawListener {
                inode: 123456,
                addr: "127.0.0.1".into(),
                port: 8080
            }
        );
        assert_eq!(
            rows[1],
            RawListener {
                inode: 123457,
                addr: "*".into(),
                port: 3000
            }
        );
    }

    #[test]
    fn decodes_ipv6_loopback() {
        let rows = parse_net_tcp(NET_TCP6);
        assert_eq!(
            rows,
            vec![RawListener {
                inode: 99,
                addr: "[::1]".into(),
                port: 6379
            }]
        );
    }

    #[test]
    fn stat_handles_parentheses_in_comm() {
        let (ppid, name) =
            parse_stat("1234 (node (vite)) S 1000 1234 1234 0 -1 4194560 ...").unwrap();
        assert_eq!(ppid, 1000);
        assert_eq!(name, "node (vite)");
    }

    #[test]
    fn cmdline_joins_nul_separated_argv() {
        assert_eq!(
            cmdline_to_string(b"python3\0-m\0http.server\0"),
            "python3 -m http.server"
        );
    }

    #[test]
    fn socket_inode_extracts_number() {
        assert_eq!(socket_inode(Path::new("socket:[4242]")), Some(4242));
        assert_eq!(socket_inode(Path::new("/dev/null")), None);
    }

    #[test]
    fn scan_root_maps_inodes_to_pids_in_fake_proc() {
        let root = std::env::temp_dir().join(format!("herdr-services-proc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("net")).unwrap();
        fs::write(root.join("net/tcp"), NET_TCP).unwrap();
        let p = root.join("4242");
        fs::create_dir_all(p.join("fd")).unwrap();
        fs::write(p.join("stat"), "4242 (python3) S 77 4242 4242 0 -1 0").unwrap();
        fs::write(p.join("cmdline"), b"python3\0-m\0http.server\0").unwrap();
        std::os::unix::fs::symlink("socket:[123457]", p.join("fd/3")).unwrap();
        std::os::unix::fs::symlink("/srv/app", p.join("cwd")).unwrap();
        // Non-numeric entry and a process without sockets must be tolerated.
        fs::create_dir_all(root.join("self")).unwrap();
        let q = root.join("77");
        fs::create_dir_all(q.join("fd")).unwrap();
        fs::write(q.join("stat"), "77 (bash) S 1 77 77 0 -1 0").unwrap();

        let snap = scan_root(&root).unwrap();
        assert_eq!(snap.listeners.len(), 1);
        let l = &snap.listeners[0];
        assert_eq!(
            (l.pid, l.port, l.addr.as_str(), l.process_name.as_str()),
            (4242, 3000, "*", "python3")
        );
        assert_eq!(snap.cwds[&4242], PathBuf::from("/srv/app"));
        assert_eq!(snap.processes[&4242].ppid, 77);
        assert_eq!(snap.processes[&4242].command, "python3 -m http.server");
        assert_eq!(snap.ancestors(4242), vec![77]);
        let _ = fs::remove_dir_all(&root);
    }
}
