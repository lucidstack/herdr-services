//! `herdr-services doctor`: environment, herdr reachability, scanner timing,
//! and a full attributed scan printed as a table. Doubles as the smoke test.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::attribute::{self, Attribution, PaneRef, Topology, WorkspaceRef};
use crate::config::Config;
use crate::herdr::{herdr_config_path, Herdr, PluginDirs};
use crate::scan;
use crate::sidebar;
use crate::state::{probe, Liveness};

pub fn run() -> Result<()> {
    let mut failures = 0;
    let mut check = |ok: bool, name: &str, detail: String| {
        println!("{} {name}: {detail}", if ok { "OK  " } else { "FAIL" });
        if !ok {
            failures += 1;
        }
    };

    // Environment
    for var in [
        "HERDR_BIN_PATH",
        "HERDR_SOCKET_PATH",
        "HERDR_PLUGIN_STATE_DIR",
        "HERDR_PLUGIN_CONFIG_DIR",
    ] {
        let value = std::env::var(var).ok();
        check(
            value.is_some() || var.starts_with("HERDR_PLUGIN"),
            var,
            value.unwrap_or_else(|| "(unset)".into()),
        );
    }

    let herdr = Herdr::from_env(Duration::from_secs(4))?;
    match herdr.ping() {
        Ok(pong) => check(
            true,
            "herdr ping",
            format!("version {} protocol {}", pong.version, pong.protocol),
        ),
        Err(err) => check(false, "herdr ping", format!("{err:#}")),
    }

    let dirs = PluginDirs::from_env(&herdr.socket).ok();
    let config = match &dirs {
        Some(d) => Config::load(&d.config_file())?,
        None => Config::default(),
    };
    if let Some(d) = &dirs {
        check(true, "session state dir", d.state.display().to_string());
        match crate::daemon::live_daemon_pid(d) {
            Some(pid) => check(true, "daemon", format!("running, pid {pid}")),
            None => check(
                true,
                "daemon",
                "not running (ensure-daemon starts it)".into(),
            ),
        }
    }

    // herdr config block
    match herdr_config_path().and_then(|p| Ok((p.clone(), std::fs::read_to_string(&p)?))) {
        Ok((path, text)) => {
            let detail = if sidebar::has_block(&text) {
                "managed [ui.sidebar.spaces] block installed"
            } else if sidebar::foreign_spaces_table(&text) {
                "[ui.sidebar.spaces] owned by user or another plugin; run `configure --print` and paste"
            } else {
                "no sidebar rows yet; run the `configure` action"
            };
            check(
                true,
                "herdr config",
                format!("{} — {detail}", path.display()),
            );
        }
        Err(err) => check(false, "herdr config", format!("{err:#}")),
    }

    // Scanner
    let scanner = scan::platform_scanner()?;
    let started = Instant::now();
    let snapshot = match scanner.scan(config.command_timeout()) {
        Ok(s) => {
            check(
                true,
                "scan",
                format!(
                    "{} backend, {} listeners, {} processes in {} ms",
                    scanner.name(),
                    s.listeners.len(),
                    s.processes.len(),
                    started.elapsed().as_millis()
                ),
            );
            s
        }
        Err(err) => {
            check(false, "scan", format!("{err:#}"));
            return finish(failures);
        }
    };

    // Topology
    let workspaces = herdr.workspaces()?;
    let panes = herdr.panes()?;
    let mut pane_refs = Vec::new();
    for pane in &panes {
        let shell_pid = herdr.shell_pid(&pane.pane_id).unwrap_or(None);
        let mut cwds = Vec::new();
        for cwd in [&pane.cwd, &pane.foreground_cwd].into_iter().flatten() {
            if !cwds.contains(cwd) {
                cwds.push(cwd.clone());
            }
        }
        pane_refs.push(PaneRef {
            pane_id: pane.pane_id.clone(),
            workspace_id: pane.workspace_id.clone(),
            shell_pid,
            cwds,
        });
    }
    let with_pid = pane_refs.iter().filter(|p| p.shell_pid.is_some()).count();
    check(
        with_pid > 0 || panes.is_empty(),
        "pane shell pids",
        format!("{with_pid}/{} panes", panes.len()),
    );
    let topology = Topology::new(
        workspaces
            .into_iter()
            .map(|w| WorkspaceRef {
                workspace_id: w.workspace_id,
                label: w.label,
                checkout_path: w.worktree.map(|t| t.checkout_path),
            })
            .collect(),
        pane_refs,
    );
    check(
        true,
        "attribution roots",
        format!("{}", topology.roots().len()),
    );

    // Table
    println!();
    println!(
        "{:<7} {:<7} {:<22} {:<10} {:<28} via",
        "port", "pid", "process", "live", "workspace"
    );
    let mut rows: Vec<_> = snapshot.listeners.iter().collect();
    rows.sort_by_key(|l| (l.port, l.pid));
    rows.dedup_by_key(|l| (l.port, l.pid));
    for l in rows {
        let command = snapshot.command_of(l.pid).unwrap_or("");
        if !config.accepts(l.port, &l.process_name, command) {
            continue;
        }
        let hit = attribute::attribute(l.pid, &snapshot, &topology);
        let (ws, via) = match &hit {
            Some(h) => {
                let label = topology.label_of(&h.workspace_id).unwrap_or("");
                let via = match &h.attribution {
                    Attribution::Pane { pane_id } => format!("pane {pane_id}"),
                    Attribution::Cwd => "cwd".into(),
                    Attribution::Command => "command".into(),
                    Attribution::Manual => "manual".into(),
                };
                (format!("{} ({label})", h.workspace_id), via)
            }
            None => ("—".into(), "unattributed".into()),
        };
        let live = match probe(&l.addr, l.port, Duration::from_millis(500)) {
            Liveness::Up => "up",
            Liveness::Down => "down",
            Liveness::Unknown => "?",
        };
        println!(
            "{:<7} {:<7} {:<22} {:<10} {:<28} {}",
            l.port,
            l.pid,
            truncate(&l.process_name, 22),
            live,
            truncate(&ws, 28),
            via
        );
    }
    println!();
    finish(failures)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n - 1).collect();
        t.push('…');
        t
    }
}

fn finish(failures: usize) -> Result<()> {
    if failures > 0 {
        anyhow::bail!("{failures} check(s) failed");
    }
    Ok(())
}
