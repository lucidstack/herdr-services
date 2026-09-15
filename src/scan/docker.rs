//! Container port mappings via `docker ps`. Compose labels carry the project's
//! working directory, which feeds the ordinary cwd attribution rule.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::process::run_with_timeout;

const LABEL_WORKING_DIR: &str = "com.docker.compose.project.working_dir";
const LABEL_SERVICE: &str = "com.docker.compose.service";
const LABEL_PROJECT: &str = "com.docker.compose.project";

/// One host-published TCP port of a container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedPort {
    pub host_addr: String,
    pub host_port: u16,
    pub container_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub id: String,
    pub name: String,
    /// Compose service name when present, else the container name.
    pub service: String,
    pub project: Option<String>,
    pub working_dir: Option<PathBuf>,
    pub image: String,
    pub ports: Vec<PublishedPort>,
}

#[derive(Debug, Deserialize)]
struct PsRow {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Names")]
    names: String,
    #[serde(rename = "Image", default)]
    image: String,
    #[serde(rename = "Labels", default)]
    labels: String,
    #[serde(rename = "Ports", default)]
    ports: String,
}

/// Whether a `docker` binary is on PATH.
pub fn available() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("docker").is_file()))
}

/// Running containers that publish at least one TCP port. Errors when the
/// docker CLI is missing or its daemon is unreachable; callers treat that as
/// "no containers" and surface it in `doctor`.
pub fn containers(timeout: Duration) -> Result<Vec<Container>> {
    let mut command = Command::new("docker");
    command.args(["ps", "--no-trunc", "--format", "{{json .}}"]);
    let output = run_with_timeout(command, timeout)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "docker ps: {}",
            stderr.trim().lines().next().unwrap_or("failed")
        );
    }
    let text = String::from_utf8(output.stdout).context("docker ps: stdout is not UTF-8")?;
    Ok(parse_ps(&text))
}

/// Parse `docker ps --format '{{json .}}'` lines; containers without published
/// TCP ports are dropped.
pub fn parse_ps(text: &str) -> Vec<Container> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<PsRow>(line.trim()).ok())
        .filter_map(|row| {
            let ports = parse_ports(&row.ports);
            if ports.is_empty() {
                return None;
            }
            let labels = parse_labels(&row.labels);
            let name = row.names.split(',').next().unwrap_or("").to_string();
            let service = labels
                .get(LABEL_SERVICE)
                .cloned()
                .unwrap_or_else(|| name.clone());
            Some(Container {
                id: row.id,
                name,
                service,
                project: labels.get(LABEL_PROJECT).cloned(),
                working_dir: labels.get(LABEL_WORKING_DIR).map(PathBuf::from),
                image: row.image,
                ports,
            })
        })
        .collect()
}

fn parse_labels(text: &str) -> HashMap<String, String> {
    text.split(',')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// `0.0.0.0:32770->4000/tcp, [::]:32770->4000/tcp, 8008/tcp` →
/// one entry per distinct host port (IPv4 and IPv6 bindings collapse).
pub fn parse_ports(text: &str) -> Vec<PublishedPort> {
    let mut out: Vec<PublishedPort> = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        let Some((host, target)) = part.split_once("->") else {
            continue;
        };
        let Some(target) = target.strip_suffix("/tcp") else {
            continue;
        };
        let Some((host_addr, host_port)) = host.rsplit_once(':') else {
            continue;
        };
        let (Ok(host_port), Ok(container_port)) = (host_port.parse(), target.parse()) else {
            continue;
        };
        if out.iter().any(|p| p.host_port == host_port) {
            continue;
        }
        let host_addr = if host_addr == "0.0.0.0" || host_addr == "[::]" {
            "*".to_string()
        } else {
            host_addr.to_string()
        };
        out.push(PublishedPort {
            host_addr,
            host_port,
            container_port,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = concat!(
        r#"{"ID":"b08011af0b61","Names":"acme-wt-x-simulator-1","Image":"acme-wt-x:latest","Labels":"com.docker.compose.service=simulator,com.docker.compose.project=acme-wt-x,com.docker.compose.project.working_dir=/Users/u/projects/worktrees/acme-api/x","Ports":"0.0.0.0:32770->4000/tcp, [::]:32770->4000/tcp"}"#,
        "\n",
        r#"{"ID":"017c6020022f","Names":"acme-wt-x-timescaledb-1","Image":"timescale/timescaledb","Labels":"com.docker.compose.service=timescaledb,com.docker.compose.project.working_dir=/Users/u/projects/worktrees/acme-api/x","Ports":"8008/tcp, 8081/tcp, 0.0.0.0:32768->5432/tcp"}"#,
        "\n",
        r#"{"ID":"deadbeef","Names":"plain","Image":"busybox","Labels":"","Ports":""}"#,
        "\n",
        r#"{"ID":"cafe","Names":"udp-only","Image":"x","Labels":"","Ports":"0.0.0.0:5353->5353/udp"}"#,
        "\n",
    );

    #[test]
    fn parses_compose_labels_and_published_tcp_ports() {
        let cs = parse_ps(PS);
        assert_eq!(
            cs.len(),
            2,
            "containers without published tcp ports are dropped"
        );
        let sim = &cs[0];
        assert_eq!(sim.service, "simulator");
        assert_eq!(sim.project.as_deref(), Some("acme-wt-x"));
        assert_eq!(
            sim.working_dir.as_deref(),
            Some(std::path::Path::new(
                "/Users/u/projects/worktrees/acme-api/x"
            ))
        );
        assert_eq!(
            sim.ports,
            vec![PublishedPort {
                host_addr: "*".into(),
                host_port: 32770,
                container_port: 4000
            }]
        );
        let ts = &cs[1];
        assert_eq!(ts.ports.len(), 1);
        assert_eq!(
            (ts.ports[0].host_port, ts.ports[0].container_port),
            (32768, 5432)
        );
        assert!(ts.project.is_none());
    }

    #[test]
    fn container_name_is_the_fallback_service_name() {
        let cs = parse_ps(
            r#"{"ID":"1","Names":"redis-dev","Image":"redis","Labels":"","Ports":"127.0.0.1:6380->6379/tcp"}"#,
        );
        assert_eq!(cs[0].service, "redis-dev");
        assert_eq!(cs[0].ports[0].host_addr, "127.0.0.1");
        assert!(cs[0].working_dir.is_none());
    }

    #[test]
    fn garbage_lines_are_skipped() {
        assert!(parse_ps("not json\n\n").is_empty());
    }
}
