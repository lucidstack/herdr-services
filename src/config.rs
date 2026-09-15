//! Plugin configuration (`$HERDR_PLUGIN_CONFIG_DIR/config.toml`).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;

/// First port of the IANA dynamic/ephemeral range.
pub const EPHEMERAL_PORT_START: u16 = 49152;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub scan: Scan,
    pub urls: Urls,
    pub sidebar: Sidebar,
    pub picker: Picker,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Scan {
    pub interval_seconds: u64,
    pub command_timeout_ms: u64,
    pub min_port: u16,
    pub ignore_processes: Vec<String>,
    /// If non-empty, only these process names are considered.
    pub allow_processes: Vec<String>,
    /// Hide listeners on ports >= 49152 (agent tooling, MCP servers, LSPs).
    pub hide_ephemeral: bool,
    /// Regexes matched against the full command line; a match hides the listener.
    #[serde(deserialize_with = "deserialize_regexes")]
    pub ignore_commands: Vec<Regex>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Urls {
    pub default_scheme: String,
    pub https_ports: Vec<u16>,
    pub prefer_hostname: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sidebar {
    pub enabled: bool,
    /// Number of `svc_N` tokens reported per workspace (1–16).
    pub max_rows: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Picker {
    pub confirm_kill: bool,
}

impl Default for Scan {
    fn default() -> Self {
        Self {
            interval_seconds: 15,
            command_timeout_ms: 4000,
            min_port: 1024,
            ignore_processes: [
                "rapportd",
                "sharingd",
                "ControlCenter",
                "com.docker.backend",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            allow_processes: Vec::new(),
            hide_ephemeral: true,
            ignore_commands: Vec::new(),
        }
    }
}

impl Default for Urls {
    fn default() -> Self {
        Self {
            default_scheme: "http".into(),
            https_ports: vec![443, 8443],
            prefer_hostname: true,
        }
    }
}

impl Default for Sidebar {
    fn default() -> Self {
        Self {
            enabled: true,
            max_rows: 8,
        }
    }
}

impl Default for Picker {
    fn default() -> Self {
        Self { confirm_kill: true }
    }
}

fn deserialize_regexes<'de, D>(deserializer: D) -> Result<Vec<Regex>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let patterns = Vec::<String>::deserialize(deserializer)?;
    patterns
        .iter()
        .map(|p| Regex::new(p).map_err(serde::de::Error::custom))
        .collect()
}

impl Config {
    /// Load the config file, or defaults when it does not exist.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("parse {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let cfg: Self = toml::from_str(text)?;
        anyhow::ensure!(
            (1..=16).contains(&cfg.sidebar.max_rows),
            "sidebar.max_rows must be between 1 and 16"
        );
        Ok(cfg)
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.scan.interval_seconds.max(1))
    }

    pub fn command_timeout(&self) -> Duration {
        Duration::from_millis(self.scan.command_timeout_ms.max(100))
    }

    /// Sidebar token TTL: six intervals, so a dead daemon's rows fade on their own.
    pub fn sidebar_ttl_ms(&self) -> u64 {
        (self.interval().as_millis() as u64) * 6
    }

    /// Whether a detected listener passes the port, name and command-line filters.
    pub fn accepts(&self, port: u16, process_name: &str, command: &str) -> bool {
        if port < self.scan.min_port {
            return false;
        }
        if self.scan.hide_ephemeral && port >= EPHEMERAL_PORT_START {
            return false;
        }
        if self
            .scan
            .ignore_commands
            .iter()
            .any(|re| re.is_match(command))
        {
            return false;
        }
        if !self.scan.allow_processes.is_empty() {
            return self.scan.allow_processes.iter().any(|p| p == process_name);
        }
        !self.scan.ignore_processes.iter().any(|p| p == process_name)
    }

    pub fn url_for(&self, port: u16) -> String {
        let scheme = if self.urls.https_ports.contains(&port) {
            "https"
        } else {
            self.urls.default_scheme.as_str()
        };
        format!("{scheme}://localhost:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_when_sections_are_partial() {
        let cfg = Config::parse("[scan]\ninterval_seconds = 5\n").unwrap();
        assert_eq!(cfg.scan.interval_seconds, 5);
        assert_eq!(cfg.scan.min_port, 1024);
        assert_eq!(cfg.sidebar.max_rows, 8);
        assert_eq!(cfg.sidebar_ttl_ms(), 30_000);
    }

    #[test]
    fn unknown_keys_and_bad_row_caps_are_rejected() {
        assert!(Config::parse("[scan]\ninterva = 5\n").is_err());
        assert!(Config::parse("[sidebar]\nmax_rows = 0\n").is_err());
        assert!(Config::parse("[sidebar]\nmax_rows = 17\n").is_err());
        assert!(Config::parse("[scan]\nignore_commands = [\"(\"]\n").is_err());
    }

    #[test]
    fn filters_apply_in_order() {
        let cfg = Config::default();
        assert!(cfg.accepts(3000, "node", "node server.js"));
        assert!(!cfg.accepts(7000, "ControlCenter", ""));
        assert!(!cfg.accepts(80, "nginx", "nginx"));
        assert!(
            !cfg.accepts(51234, "omp", "omp worker"),
            "ephemeral hidden by default"
        );

        let cfg =
            Config::parse("[scan]\nhide_ephemeral = false\nignore_commands = [\"mcp-server\"]\n")
                .unwrap();
        assert!(cfg.accepts(51234, "node", "node app.js"));
        assert!(!cfg.accepts(3000, "node", "node mcp-server.js"));

        let cfg = Config::parse("[scan]\nallow_processes = [\"puma\"]\n").unwrap();
        assert!(cfg.accepts(3000, "puma", ""));
        assert!(!cfg.accepts(3000, "node", ""));
    }

    #[test]
    fn https_ports_get_https_scheme() {
        let cfg = Config::default();
        assert_eq!(cfg.url_for(3000), "http://localhost:3000");
        assert_eq!(cfg.url_for(8443), "https://localhost:8443");
    }
}
