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
        let (Some((host_start, host_end)), Some((target_start, target_end))) =
            (parse_port_range(host_port), parse_port_range(target))
        else {
            continue;
        };
        if host_end - host_start != target_end - target_start {
            continue;
        }
        let host_addr = if host_addr == "0.0.0.0" || host_addr == "[::]" {
            "*".to_string()
        } else {
            host_addr.to_string()
        };
        for (host_port, container_port) in (host_start..=host_end).zip(target_start..=target_end) {
            if out.iter().any(|p| p.host_port == host_port) {
                continue;
            }
            out.push(PublishedPort {
                host_addr: host_addr.clone(),
                host_port,
                container_port,
            });
        }
    }
    out
}

fn parse_port_range(text: &str) -> Option<(u16, u16)> {
    let (start, end) = text.split_once('-').unwrap_or((text, text));
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    (start > 0 && start <= end).then_some((start, end))
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

    #[test]
    fn expands_docker_port_ranges_and_collapses_dual_stack_bindings() {
        let ports = parse_ports("0.0.0.0:8080-8083->8080-8083/tcp, [::]:8080-8083->8080-8083/tcp");
        assert_eq!(ports.len(), 4);
        for (port, expected) in ports.iter().zip(8080..=8083) {
            assert_eq!(port.host_addr, "*");
            assert_eq!((port.host_port, port.container_port), (expected, expected));
        }
        let containers = parse_ps(
            r#"{"ID":"sbt","Names":"backend-sbt-1","Labels":"com.docker.compose.service=sbt","Ports":"0.0.0.0:8080-8083->8080-8083/tcp, [::]:8080-8083->8080-8083/tcp"}"#,
        );
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].service, "sbt");
        assert_eq!(containers[0].ports, ports);
    }

    #[test]
    fn handles_remapped_ranges_and_single_ports_together() {
        let ports = parse_ports("127.0.0.1:18080-18081->8080-8081/tcp, 0.0.0.0:65535->65535/tcp");
        assert_eq!(ports.len(), 3);
        assert_eq!((ports[1].host_port, ports[1].container_port), (18081, 8081));
        assert_eq!(ports[0].host_addr, "127.0.0.1");
        assert_eq!(ports[2].host_port, 65535);
    }

    #[test]
    fn skips_invalid_or_unpublished_port_ranges() {
        for text in [
            "0.0.0.0:8083-8080->8080-8083/tcp",
            "0.0.0.0:8080-8083->8080-8082/tcp",
            "0.0.0.0:0->8080/tcp",
            "0.0.0.0:65536->8080/tcp",
            "0.0.0.0:bad->8080/tcp",
            "0.0.0.0:8080-8083->8080-8083/udp",
            "8080-8083/tcp",
        ] {
            assert!(parse_ports(text).is_empty(), "{text}");
        }
    }
}
