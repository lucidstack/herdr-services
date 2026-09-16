//! Service registry: merge rules (SPEC §3.4) and the `state.json` file the
//! picker reads.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::attribute::Attribution;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Detected,
    Advertised,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    #[default]
    Unknown,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Service {
    pub workspace_id: String,
    pub port: u16,
    pub host_hint: String,
    pub pid: Option<u32>,
    /// Docker container name; set for container-published ports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    pub process_name: String,
    pub argv_summary: String,
    pub cwd: Option<String>,
    pub url: String,
    pub label: Option<String>,
    pub source: Source,
    pub attribution: Attribution,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    #[serde(default)]
    pub liveness: Liveness,
    /// Consecutive scans in which this detected service was absent.
    #[serde(default)]
    pub missed_scans: u8,
}

/// One listener (process or container) the scanner saw and attribution accepted.
#[derive(Debug, Clone)]
pub struct Observed {
    pub workspace_id: String,
    pub port: u16,
    pub host_hint: String,
    /// Host process PID; `None` for containers.
    pub pid: Option<u32>,
    /// Docker container name when the listener is a published container port.
    pub container: Option<String>,
    pub process_name: String,
    pub argv_summary: String,
    pub cwd: Option<String>,
    pub url: String,
    pub advertised: bool,
    pub attribution: Attribution,
}

/// Unattributed listener, kept for the "other" group of the all-workspaces view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unattributed {
    pub port: u16,
    pub host_hint: String,
    pub pid: Option<u32>,
    pub process_name: String,
    pub argv_summary: String,
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    pub schema: u32,
    pub daemon_pid: u32,
    pub updated_ms: u64,
    pub scan_backend: String,
    pub last_scan_ms: u64,
    pub last_scan_duration_ms: u64,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Workspace id → label, for display.
    pub workspace_labels: BTreeMap<String, String>,
    pub services: Vec<Service>,
    pub unattributed: Vec<Unattributed>,
}

pub const SCHEMA: u32 = 1;
/// A detected service survives this many consecutive scans without a listener.
pub const MISS_GRACE: u8 = 2;

impl State {
    /// Apply one scan. Detected services absent from `observed` accrue a miss and
    /// are dropped after [`MISS_GRACE`]; manual services never disappear.
    pub fn apply_scan(&mut self, observed: Vec<Observed>, now_ms: u64) {
        for s in &mut self.services {
            if s.source != Source::Manual {
                s.missed_scans = s.missed_scans.saturating_add(1);
            }
        }
        for o in observed {
            match self
                .services
                .iter_mut()
                .find(|s| s.workspace_id == o.workspace_id && s.port == o.port)
            {
                Some(existing) => {
                    let identity_changed =
                        existing.pid != o.pid || existing.container != o.container;
                    existing.pid = o.pid;
                    existing.container = o.container;
                    existing.process_name = o.process_name;
                    existing.argv_summary = o.argv_summary;
                    existing.cwd = o.cwd;
                    existing.host_hint = o.host_hint;
                    existing.attribution = o.attribution;
                    existing.last_seen_ms = now_ms;
                    existing.missed_scans = 0;
                    if o.advertised {
                        existing.source = Source::Advertised;
                        existing.url = o.url;
                    } else if identity_changed && existing.source == Source::Advertised {
                        // An advertised URL belonged to the old process; fall back to the inferred one.
                        existing.source = Source::Detected;
                        existing.url = o.url;
                    } else if existing.source == Source::Detected {
                        existing.url = o.url;
                    }
                }
                None => self.services.push(Service {
                    workspace_id: o.workspace_id,
                    port: o.port,
                    host_hint: o.host_hint,
                    pid: o.pid,
                    container: o.container,
                    process_name: o.process_name,
                    argv_summary: o.argv_summary,
                    cwd: o.cwd,
                    url: o.url,
                    label: None,
                    source: if o.advertised {
                        Source::Advertised
                    } else {
                        Source::Detected
                    },
                    attribution: o.attribution,
                    first_seen_ms: now_ms,
                    last_seen_ms: now_ms,
                    liveness: Liveness::Unknown,
                    missed_scans: 0,
                }),
            }
        }
        self.services
            .retain(|s| s.source == Source::Manual || s.missed_scans < MISS_GRACE);
        for s in &mut self.services {
            if s.missed_scans > 0 && s.source != Source::Manual {
                s.pid = None;
                s.container = None;
            }
        }
        // Newest listener first, then by port for stability.
        self.services.sort_by(|a, b| {
            b.first_seen_ms
                .cmp(&a.first_seen_ms)
                .then(a.port.cmp(&b.port))
        });
        self.last_scan_ms = now_ms;
        self.updated_ms = now_ms;
    }

    pub fn services_for<'a>(
        &'a self,
        workspace_id: &'a str,
    ) -> impl Iterator<Item = &'a Service> + 'a {
        self.services
            .iter()
            .filter(move |s| s.workspace_id == workspace_id)
    }

    pub fn workspace_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self
            .services
            .iter()
            .map(|s| s.workspace_id.as_str())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Upsert a manual entry by label (SPEC §2.3). A label already on file
    /// moves to the new `(workspace_id, port, url)`. Otherwise, if a service
    /// already occupies `(workspace_id, port)` (detected or advertised), the
    /// label attaches to it — the manual label wins for display, but the row
    /// keeps disappearing like any other detected service once its listener
    /// goes away. Only a genuinely new `(workspace_id, port)` creates a
    /// standalone manual service, which never disappears on its own.
    pub fn upsert_manual(
        &mut self,
        workspace_id: String,
        label: String,
        port: u16,
        url: String,
        now_ms: u64,
    ) {
        self.services.retain_mut(|s| {
            if s.label.as_deref() == Some(label.as_str()) {
                if s.source == Source::Manual {
                    // A standalone manual entry has no reason to exist without its label.
                    return false;
                }
                s.label = None;
            }
            true
        });
        if let Some(existing) = self
            .services
            .iter_mut()
            .find(|s| s.workspace_id == workspace_id && s.port == port)
        {
            existing.label = Some(label);
            existing.last_seen_ms = now_ms;
            if existing.source == Source::Manual {
                existing.url = url;
            }
            return;
        }
        self.services.push(Service {
            workspace_id,
            port,
            host_hint: "*".into(),
            pid: None,
            container: None,
            process_name: String::new(),
            argv_summary: String::new(),
            cwd: None,
            url,
            label: Some(label),
            source: Source::Manual,
            attribution: Attribution::Manual,
            first_seen_ms: now_ms,
            last_seen_ms: now_ms,
            liveness: Liveness::Unknown,
            missed_scans: 0,
        });
    }

    /// Remove a manual label. A standalone manual service is deleted outright;
    /// a label attached to a detected/advertised service is just cleared.
    /// Returns `true` if a label was found and removed.
    pub fn remove_manual(&mut self, label: &str) -> bool {
        let Some(index) = self
            .services
            .iter()
            .position(|s| s.label.as_deref() == Some(label))
        else {
            return false;
        };
        if self.services[index].source == Source::Manual {
            self.services.remove(index);
        } else {
            self.services[index].label = None;
        }
        true
    }

    /// TCP connect to each service; sets `liveness`.
    pub fn probe_liveness(&mut self, timeout: Duration) {
        for s in &mut self.services {
            s.liveness = probe(&s.host_hint, s.port, timeout);
        }
    }

    /// Read `state.json`; used by the picker and tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn read(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    pub fn write_atomic(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(self)?;
        {
            let mut file =
                std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            file.write_all(&json)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
    }
}

/// Connect probe. Wildcard and IPv4 hints go to 127.0.0.1; IPv6 hints to ::1.
pub fn probe(host_hint: &str, port: u16, timeout: Duration) -> Liveness {
    let addr: SocketAddr = match host_hint {
        h if h.starts_with('[') => {
            let inner = h.trim_start_matches('[').trim_end_matches(']');
            match inner.parse::<std::net::Ipv6Addr>() {
                Ok(ip) if !ip.is_unspecified() => SocketAddr::new(ip.into(), port),
                _ => SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), port),
            }
        }
        "*" | "" => SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port),
        h => match h.parse::<std::net::Ipv4Addr>() {
            Ok(ip) if !ip.is_unspecified() => SocketAddr::new(ip.into(), port),
            _ => SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port),
        },
    };
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => Liveness::Up,
        Err(_) => Liveness::Down,
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed(ws: &str, port: u16, pid: u32) -> Observed {
        Observed {
            workspace_id: ws.into(),
            port,
            host_hint: "*".into(),
            pid: Some(pid),
            container: None,
            process_name: "puma".into(),
            argv_summary: "puma".into(),
            cwd: None,
            url: format!("http://localhost:{port}"),
            advertised: false,
            attribution: Attribution::Cwd,
        }
    }

    #[test]
    fn observed_advertised_url_is_adopted_and_persists_until_pid_changes() {
        let mut state = State::default();
        let mut o = observed("w1", 3000, 10);
        o.advertised = true;
        o.url = "https://app.test:3000/".into();
        state.apply_scan(vec![o], 1);
        assert_eq!(state.services[0].source, Source::Advertised);
        assert_eq!(state.services[0].url, "https://app.test:3000/");
        // Same pid, no fresh advertisement this scan: the advertised url survives.
        state.apply_scan(vec![observed("w1", 3000, 10)], 2);
        assert_eq!(state.services[0].source, Source::Advertised);
        assert_eq!(state.services[0].url, "https://app.test:3000/");
        // New pid: the advertised url is invalidated in favour of the inferred one.
        state.apply_scan(vec![observed("w1", 3000, 99)], 3);
        assert_eq!(state.services[0].source, Source::Detected);
        assert_eq!(state.services[0].url, "http://localhost:3000");
    }

    #[test]
    fn detected_service_survives_one_missed_scan_then_disappears() {
        let mut state = State::default();
        state.apply_scan(vec![observed("w1", 3000, 10)], 1);
        assert_eq!(state.services.len(), 1);
        state.apply_scan(vec![], 2);
        assert_eq!(state.services.len(), 1, "grace for restarts");
        assert_eq!(state.services[0].pid, None);
        state.apply_scan(vec![], 3);
        assert!(state.services.is_empty());
    }

    #[test]
    fn reappearing_service_keeps_first_seen_and_resets_misses() {
        let mut state = State::default();
        state.apply_scan(vec![observed("w1", 3000, 10)], 1);
        state.apply_scan(vec![], 2);
        state.apply_scan(vec![observed("w1", 3000, 11)], 3);
        let s = &state.services[0];
        assert_eq!(
            (s.first_seen_ms, s.last_seen_ms, s.pid, s.missed_scans),
            (1, 3, Some(11), 0)
        );
    }

    #[test]
    fn manual_services_never_disappear_but_lose_pid() {
        let mut state = State::default();
        state.services.push(Service {
            workspace_id: "w1".into(),
            port: 6006,
            host_hint: "*".into(),
            pid: None,
            container: None,
            process_name: String::new(),
            argv_summary: String::new(),
            cwd: None,
            url: "http://localhost:6006".into(),
            label: Some("Storybook".into()),
            source: Source::Manual,
            attribution: Attribution::Manual,
            first_seen_ms: 0,
            last_seen_ms: 0,
            liveness: Liveness::Unknown,
            missed_scans: 0,
        });
        state.apply_scan(vec![observed("w1", 6006, 42)], 1);
        assert_eq!(state.services[0].pid, Some(42));
        assert_eq!(
            state.services[0].label.as_deref(),
            Some("Storybook"),
            "manual label wins"
        );
        for t in 2..10 {
            state.apply_scan(vec![], t);
        }
        assert_eq!(state.services.len(), 1);
        assert_eq!(
            state.services[0].pid,
            Some(42),
            "manual entries keep the last known pid"
        );
    }

    #[test]
    fn upsert_manual_creates_a_standalone_service_for_an_unseen_port() {
        let mut state = State::default();
        state.upsert_manual(
            "w1".into(),
            "Storybook".into(),
            6006,
            "http://localhost:6006".into(),
            1,
        );
        assert_eq!(state.services.len(), 1);
        let s = &state.services[0];
        assert_eq!(
            (s.source, s.label.as_deref()),
            (Source::Manual, Some("Storybook"))
        );
        // Re-adding the same label just moves it.
        state.upsert_manual(
            "w1".into(),
            "Storybook".into(),
            7007,
            "http://localhost:7007".into(),
            2,
        );
        assert_eq!(state.services.len(), 1);
        assert_eq!(state.services[0].port, 7007);
    }

    #[test]
    fn upsert_manual_attaches_a_label_to_an_already_detected_service() {
        let mut state = State::default();
        state.apply_scan(vec![observed("w1", 3000, 10)], 1);
        state.upsert_manual(
            "w1".into(),
            "API".into(),
            3000,
            "http://localhost:3000".into(),
            2,
        );
        assert_eq!(state.services.len(), 1, "merges into the existing row");
        assert_eq!(
            state.services[0].source,
            Source::Detected,
            "stays detected, not manual"
        );
        assert_eq!(state.services[0].label.as_deref(), Some("API"));
        // The process disappears like any other detected service, label included.
        state.apply_scan(vec![], 3);
        state.apply_scan(vec![], 4);
        assert!(state.services.is_empty());
    }

    #[test]
    fn remove_manual_deletes_a_standalone_entry_but_only_unlabels_a_detected_one() {
        let mut state = State::default();
        state.apply_scan(vec![observed("w1", 3000, 10)], 1);
        state.upsert_manual(
            "w1".into(),
            "API".into(),
            3000,
            "http://localhost:3000".into(),
            2,
        );
        state.upsert_manual(
            "w1".into(),
            "Storybook".into(),
            6006,
            "http://localhost:6006".into(),
            2,
        );
        assert!(state.remove_manual("Storybook"));
        assert_eq!(state.services.len(), 1, "standalone entry is gone");
        assert!(state.remove_manual("API"));
        assert_eq!(
            state.services.len(),
            1,
            "detected service stays, just unlabelled"
        );
        assert_eq!(state.services[0].label, None);
        assert!(!state.remove_manual("API"), "already removed");
    }

    #[test]
    fn advertised_url_is_invalidated_when_pid_changes() {
        let mut state = State::default();
        state.apply_scan(vec![observed("w1", 3000, 10)], 1);
        state.services[0].source = Source::Advertised;
        state.services[0].url = "https://app.test:3000/".into();
        state.apply_scan(vec![observed("w1", 3000, 10)], 2);
        assert_eq!(
            state.services[0].url, "https://app.test:3000/",
            "same pid keeps advertised url"
        );
        state.apply_scan(vec![observed("w1", 3000, 99)], 3);
        assert_eq!(state.services[0].url, "http://localhost:3000");
        assert_eq!(state.services[0].source, Source::Detected);
    }

    #[test]
    fn newest_first_ordering() {
        let mut state = State::default();
        state.apply_scan(vec![observed("w1", 3000, 10)], 1);
        state.apply_scan(vec![observed("w1", 3000, 10), observed("w1", 5173, 11)], 2);
        assert_eq!(state.services[0].port, 5173);
    }

    #[test]
    fn state_round_trips_through_disk() {
        let mut state = State::default();
        state.apply_scan(vec![observed("w1", 3000, 10)], 1);
        let dir = std::env::temp_dir().join(format!("herdr-services-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        state.write_atomic(&path).unwrap();
        let back = State::read(&path).unwrap();
        assert_eq!(back.services, state.services);
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_reports_down_on_closed_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(probe("*", port, Duration::from_millis(300)), Liveness::Up);
        drop(listener);
        assert_eq!(
            probe("127.0.0.1", port, Duration::from_millis(300)),
            Liveness::Down
        );
    }
}
