//! Process table via `ps -axo pid=,ppid=,command=` (macOS, and a fallback elsewhere).

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;

use super::ProcessInfo;
use crate::process::run_checked;

pub fn process_table(timeout: Duration) -> Result<HashMap<u32, ProcessInfo>> {
    let mut command = Command::new("ps");
    command.args(["-axo", "pid=,ppid=,command="]);
    let out = run_checked(command, timeout)?;
    Ok(parse_ps(&out))
}

/// Parse `pid ppid command…` rows; malformed rows are skipped.
pub fn parse_ps(text: &str) -> HashMap<u32, ProcessInfo> {
    let mut table = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        let command = parts.collect::<Vec<_>>().join(" ");
        table.insert(pid, ProcessInfo { ppid, command });
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_padded_rows_and_skips_garbage() {
        let text = "  181     1 /System/Library/CoreServices/loginwindow console\n\
                    1297     1 /opt/homebrew/opt/redis/bin/redis-server 127.0.0.1:6379\n\
                    PID PPID COMMAND\n\
                    66860 51260 omp\n";
        let table = parse_ps(text);
        assert_eq!(table.len(), 3);
        assert_eq!(table[&1297].ppid, 1);
        assert_eq!(
            table[&1297].command,
            "/opt/homebrew/opt/redis/bin/redis-server 127.0.0.1:6379"
        );
        assert_eq!(table[&66860].ppid, 51260);
        assert_eq!(table[&66860].command, "omp");
    }
}
