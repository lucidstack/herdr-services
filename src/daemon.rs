//! The detector daemon: scan loop, attribution, `state.json`, sidebar rows,
//! and a small control socket for the picker and the `rescan` action.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::advertise::{self, Advertisements};
use crate::attribute::{self, Attribution, PaneRef, Topology, WorkspaceRef};
use crate::config::Config;
use crate::herdr::{Herdr, PluginDirs};
use crate::names;
use crate::scan::docker::{self, Container};
use crate::scan::{self, Scanner, Snapshot};
use crate::sidebar;
use crate::state::{now_ms, Observed, State, SCHEMA};

const TOPOLOGY_EVENTS: &[&str] = &[
    "pane.created",
    "pane.exited",
    "pane.closed",
    "workspace.closed",
];
const EVENT_DEBOUNCE: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Requests accepted on the control socket, one JSON object per line.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Rescan,
    Kill {
        workspace: String,
        port: u16,
        #[serde(default)]
        pid: Option<u32>,
        #[serde(default)]
        container: Option<String>,
        #[serde(default)]
        signal: Signal,
    },
    AddManual {
        workspace_id: String,
        label: String,
        port: u16,
        #[serde(default)]
        url: Option<String>,
    },
    RemoveManual {
        label: String,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    #[default]
    Term,
    Kill,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

enum Wake {
    Control(Request, Sender<Reply>),
    Event,
    Advertised {
        pane_id: String,
        matched_line: String,
    },
    HerdrGone,
}

pub struct Daemon {
    herdr: Herdr,
    dirs: PluginDirs,
    config: Config,
    scanner: Box<dyn Scanner>,
    state: State,
    reporter: sidebar::Reporter,
    shell_pids: HashMap<String, Option<u32>>,
    consecutive_failures: u32,
    shutting_down: bool,
    /// Last docker error message, so it is logged once rather than every scan.
    docker_error: Option<String>,
    verbose: bool,
    /// Advertised URLs from pane output (SPEC §3.1), keyed by workspace/port.
    advertisements: Advertisements,
    /// Pane id → workspace id, refreshed every scan; resolves `Wake::Advertised`.
    pane_workspaces: HashMap<String, String>,
    /// Pane ids the output watcher is currently subscribed to (sorted).
    watched_panes: Vec<String>,
    /// Socket of the running output watcher, kept only to force it closed
    /// when the pane set changes.
    output_watcher: Option<UnixStream>,
    /// Set once `main_loop` creates the wake channel; lets scan code spawn
    /// the output watcher with a sender back into the loop.
    event_tx: Option<Sender<Wake>>,
}

pub fn run(verbose: bool) -> Result<()> {
    let config_timeout = Duration::from_millis(4000);
    let herdr = Herdr::from_env(config_timeout)?;
    let dirs = PluginDirs::from_env(&herdr.socket)?;
    let config = Config::load(&dirs.config_file())?;
    let herdr = Herdr {
        timeout: config.command_timeout(),
        ..herdr
    };

    if let Some(pid) = live_daemon_pid(&dirs) {
        bail!("daemon already running (pid {pid})");
    }
    std::fs::write(dirs.pid_file(), std::process::id().to_string())?;
    std::fs::write(
        dirs.socket_marker(),
        herdr.socket.to_string_lossy().as_bytes(),
    )?;

    let scanner = scan::platform_scanner()?;
    let mut daemon = Daemon {
        herdr,
        dirs,
        config,
        scanner,
        state: State {
            schema: SCHEMA,
            daemon_pid: std::process::id(),
            ..State::default()
        },
        reporter: sidebar::Reporter::default(),
        shell_pids: HashMap::new(),
        consecutive_failures: 0,
        shutting_down: false,
        docker_error: None,
        verbose,
        advertisements: Advertisements::default(),
        pane_workspaces: HashMap::new(),
        watched_panes: Vec::new(),
        output_watcher: None,
        event_tx: None,
    };
    let result = daemon.main_loop();
    daemon.cleanup();
    result
}

impl Daemon {
    fn log(&self, msg: &str) {
        if self.verbose {
            eprintln!("[herdr-services] {msg}");
        }
    }

    fn main_loop(&mut self) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        spawn_control_listener(&self.dirs.control_socket(), tx.clone())?;
        spawn_event_watcher(self.herdr.clone(), tx.clone());
        self.event_tx = Some(tx);
        self.state.scan_backend = self.scanner.name().to_string();

        let mut next_scan = Instant::now();
        let mut pending_event: Option<Instant> = None;
        loop {
            let due = match pending_event {
                Some(at) => at.min(next_scan),
                None => next_scan,
            };
            let wait = due.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(Wake::Control(request, reply)) => {
                    let response = self.handle(request);
                    let stop = matches!(
                        response,
                        Reply {
                            ok: true,
                            pid: None,
                            error: None
                        }
                    ) && self.shutting_down;
                    let _ = reply.send(response);
                    if stop {
                        return Ok(());
                    }
                    continue;
                }
                Ok(Wake::Event) => {
                    pending_event.get_or_insert(Instant::now() + EVENT_DEBOUNCE);
                    continue;
                }
                Ok(Wake::Advertised {
                    pane_id,
                    matched_line,
                }) => {
                    self.apply_advertisement(&pane_id, &matched_line);
                    pending_event.get_or_insert(Instant::now() + EVENT_DEBOUNCE);
                    continue;
                }
                Ok(Wake::HerdrGone) => {
                    self.log("herdr socket closed; exiting");
                    return Ok(());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            if !self.herdr.socket_present() {
                self.log("herdr socket file gone; exiting");
                return Ok(());
            }
            pending_event = None;
            self.scan_once();
            next_scan = Instant::now() + self.interval_after_result();
        }
    }
}

/// `shutting_down` is set by a `shutdown` request; kept out of the struct
/// initialiser above for readability.
impl Daemon {
    fn interval_after_result(&self) -> Duration {
        if self.consecutive_failures == 0 {
            return self.config.interval();
        }
        let factor = 2u32.saturating_pow(self.consecutive_failures.min(5));
        (self.config.interval() * factor).min(MAX_BACKOFF)
    }

    fn scan_once(&mut self) {
        let started = Instant::now();
        match self.scan_and_apply() {
            Ok(()) => {
                self.consecutive_failures = 0;
                self.state.last_error = None;
            }
            Err(err) => {
                self.consecutive_failures += 1;
                self.state.last_error = Some(format!("{err:#}"));
                self.log(&format!("scan failed: {err:#}"));
            }
        }
        self.state.last_scan_duration_ms = started.elapsed().as_millis() as u64;
        self.state.updated_ms = now_ms();
        if let Err(err) = self.state.write_atomic(&self.dirs.state_file()) {
            self.log(&format!("write state: {err:#}"));
        }
    }

    fn scan_and_apply(&mut self) -> Result<()> {
        let timeout = self.config.command_timeout();
        let snapshot = self.scanner.scan(timeout).context("listener scan")?;
        let containers = self.containers(timeout);
        let topology = self.topology()?;
        self.pane_workspaces = topology
            .panes
            .iter()
            .map(|p| (p.pane_id.clone(), p.workspace_id.clone()))
            .collect();
        self.advertisements.retain_panes(&self.pane_workspaces);
        self.sync_output_watcher(&topology);
        let observed = self.observe(&snapshot, &containers, &topology);
        self.state.workspace_labels = topology
            .workspaces
            .iter()
            .map(|w| (w.workspace_id.clone(), w.label.clone()))
            .collect();
        self.state.apply_scan(observed, now_ms());
        self.state.probe_liveness(PROBE_TIMEOUT);
        self.report_sidebar()
    }

    /// Published container ports; a missing or unreachable docker is not a scan
    /// failure, it just yields nothing (and is logged once per state change).
    fn containers(&mut self, timeout: Duration) -> Vec<Container> {
        if !self.config.docker.enabled || !docker::available() {
            return Vec::new();
        }
        match docker::containers(timeout) {
            Ok(containers) => {
                if self.docker_error.take().is_some() {
                    self.log("docker reachable again");
                }
                containers
            }
            Err(err) => {
                let msg = format!("{err:#}");
                if self.docker_error.as_deref() != Some(msg.as_str()) {
                    self.log(&format!("docker unavailable: {msg}"));
                    self.docker_error = Some(msg);
                }
                Vec::new()
            }
        }
    }

    /// Current workspaces and panes with shell PIDs (cached per pane id).
    fn topology(&mut self) -> Result<Topology> {
        let workspaces = self.herdr.workspaces().context("workspace list")?;
        let panes = self.herdr.panes().context("pane list")?;
        let live_ids: Vec<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
        self.shell_pids
            .retain(|id, _| live_ids.contains(&id.as_str()));
        let mut refs = Vec::with_capacity(panes.len());
        for pane in &panes {
            let shell_pid = match self.shell_pids.get(&pane.pane_id) {
                Some(pid) => *pid,
                None => {
                    let pid = self.herdr.shell_pid(&pane.pane_id).unwrap_or(None);
                    self.shell_pids.insert(pane.pane_id.clone(), pid);
                    pid
                }
            };
            let mut cwds = Vec::new();
            for cwd in [&pane.cwd, &pane.foreground_cwd].into_iter().flatten() {
                if !cwds.contains(cwd) {
                    cwds.push(cwd.clone());
                }
            }
            refs.push(PaneRef {
                pane_id: pane.pane_id.clone(),
                workspace_id: pane.workspace_id.clone(),
                shell_pid,
                cwds,
            });
        }
        let workspaces = workspaces
            .into_iter()
            .map(|w| WorkspaceRef {
                workspace_id: w.workspace_id,
                label: w.label,
                checkout_path: w.worktree.map(|t| t.checkout_path),
            })
            .collect();
        Ok(Topology::new(workspaces, refs))
    }

    /// Filter and attribute listeners; unattributed ones go to `state.unattributed`.
    fn observe(
        &mut self,
        snapshot: &Snapshot,
        containers: &[Container],
        topology: &Topology,
    ) -> Vec<Observed> {
        let mut observed = Vec::new();
        let mut unattributed = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for listener in &snapshot.listeners {
            if !seen.insert((listener.pid, listener.port)) {
                continue;
            }
            let command = snapshot.command_of(listener.pid).unwrap_or("");
            let hit = attribute::attribute(listener.pid, snapshot, topology);
            let from_pane = matches!(
                hit.as_ref().map(|h| &h.attribution),
                Some(Attribution::Pane { .. })
            );
            if !self
                .config
                .accepts(listener.port, &listener.process_name, command, from_pane)
            {
                continue;
            }
            let cwd = snapshot
                .cwds
                .get(&listener.pid)
                .map(|p| p.to_string_lossy().into_owned());
            match hit {
                Some(hit) => {
                    let previous_pid = self
                        .state
                        .services_for(&hit.workspace_id)
                        .find(|s| s.port == listener.port)
                        .and_then(|s| s.pid);
                    if previous_pid.is_some() && previous_pid != Some(listener.pid) {
                        self.advertisements.forget(&hit.workspace_id, listener.port);
                    }
                    let advertised_url = self.advertisements.get(&hit.workspace_id, listener.port);
                    let advertised = advertised_url.is_some();
                    let url = advertised_url.unwrap_or_else(|| self.config.url_for(listener.port));
                    observed.push(Observed {
                        workspace_id: hit.workspace_id,
                        port: listener.port,
                        host_hint: listener.addr.clone(),
                        pid: Some(listener.pid),
                        container: None,
                        process_name: names::display_name(&listener.process_name, command),
                        argv_summary: summarise(command),
                        cwd,
                        url,
                        advertised,
                        attribution: hit.attribution,
                    })
                }
                None => unattributed.push(crate::state::Unattributed {
                    port: listener.port,
                    host_hint: listener.addr.clone(),
                    pid: Some(listener.pid),
                    process_name: names::display_name(&listener.process_name, command),
                    argv_summary: summarise(command),
                    cwd,
                    container: None,
                }),
            }
        }
        for container in containers {
            let hit = container
                .working_dir
                .as_deref()
                .and_then(|dir| attribute::attribute_container(dir, topology));
            let argv = format!("docker {} ({})", container.name, container.image);
            let cwd = container
                .working_dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned());
            for port in &container.ports {
                // Published ports are user intent; only the name filters apply.
                if !self
                    .config
                    .accepts(port.host_port, &container.service, &argv, true)
                {
                    continue;
                }
                match &hit {
                    Some(hit) => observed.push(Observed {
                        workspace_id: hit.workspace_id.clone(),
                        port: port.host_port,
                        host_hint: port.host_addr.clone(),
                        pid: None,
                        container: Some(container.name.clone()),
                        process_name: container.service.clone(),
                        argv_summary: argv.clone(),
                        cwd: cwd.clone(),
                        url: self.config.url_for(port.host_port),
                        advertised: false,
                        attribution: hit.attribution.clone(),
                    }),
                    None => unattributed.push(crate::state::Unattributed {
                        port: port.host_port,
                        host_hint: port.host_addr.clone(),
                        pid: None,
                        process_name: container.service.clone(),
                        argv_summary: argv.clone(),
                        cwd: cwd.clone(),
                        container: Some(container.name.clone()),
                    }),
                }
            }
        }
        // One row per (workspace, port): host processes win over containers, pane
        // attribution over the rest (stable sort keeps first-seen order otherwise).
        observed.sort_by(|a, b| {
            (a.workspace_id.as_str(), a.port, a.pid.is_none()).cmp(&(
                b.workspace_id.as_str(),
                b.port,
                b.pid.is_none(),
            ))
        });
        observed.dedup_by(|b, a| a.workspace_id == b.workspace_id && a.port == b.port);
        unattributed.sort_by_key(|u| (u.port, u.pid));
        self.state.unattributed = unattributed;
        observed
    }

    fn report_sidebar(&mut self) -> Result<()> {
        if !self.config.sidebar.enabled {
            return Ok(());
        }
        let max_rows = usize::from(self.config.sidebar.max_rows);
        let ttl = self.config.sidebar_ttl_ms();
        let ids: Vec<String> = self
            .state
            .workspace_ids()
            .iter()
            .map(|s| s.to_string())
            .collect();
        for ws in &ids {
            let services: Vec<&crate::state::Service> = self.state.services_for(ws).collect();
            let rows = sidebar::rows_for(&services, max_rows);
            if self.reporter.report(&self.herdr, ws, rows, ttl)? {
                self.log(&format!("reported sidebar rows for {ws}"));
            }
        }
        let live: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.reporter.clear_missing(&self.herdr, &live)
    }

    /// Resubscribe `pane.output_matched` when the pane set has changed since
    /// the last scan (new/closed panes): shut down the old subscription and
    /// open a fresh one covering exactly the panes seen this scan.
    fn sync_output_watcher(&mut self, topology: &Topology) {
        let mut pane_ids: Vec<String> = topology.panes.iter().map(|p| p.pane_id.clone()).collect();
        pane_ids.sort();
        if pane_ids == self.watched_panes {
            return;
        }
        if let Some(old) = self.output_watcher.take() {
            let _ = old.shutdown(std::net::Shutdown::Both);
        }
        self.watched_panes = pane_ids.clone();
        let Some(tx) = self.event_tx.clone() else {
            return;
        };
        self.output_watcher = spawn_output_watcher(&self.herdr, &pane_ids, tx);
    }

    /// A `pane.output_matched` push arrived between scans: resolve the pane's
    /// workspace from the cache the last scan built and record the URL.
    fn apply_advertisement(&mut self, pane_id: &str, matched_line: &str) {
        let Some(workspace_id) = self.pane_workspaces.get(pane_id).cloned() else {
            return;
        };
        let Some((port, url)) = advertise::parse_url(matched_line) else {
            return;
        };
        if self
            .advertisements
            .record(workspace_id, port, url, pane_id.to_string())
        {
            self.log(&format!("advertised url for {pane_id}:{port}"));
        }
    }

    fn handle(&mut self, request: Request) -> Reply {
        match request {
            Request::Ping => Reply {
                ok: true,
                pid: Some(std::process::id()),
                error: None,
            },
            Request::Rescan => {
                self.scan_once();
                Reply {
                    ok: true,
                    pid: None,
                    error: None,
                }
            }
            Request::Kill {
                workspace,
                port,
                pid,
                container,
                signal,
            } => match self.kill(&workspace, port, pid, container.as_deref(), signal) {
                Ok(()) => {
                    self.scan_once();
                    Reply {
                        ok: true,
                        pid: None,
                        error: None,
                    }
                }
                Err(err) => Reply {
                    ok: false,
                    pid: None,
                    error: Some(format!("{err:#}")),
                },
            },
            Request::AddManual {
                workspace_id,
                label,
                port,
                url,
            } => {
                let url = url.unwrap_or_else(|| self.config.url_for(port));
                self.state
                    .upsert_manual(workspace_id, label, port, url, now_ms());
                self.scan_once();
                Reply {
                    ok: true,
                    pid: None,
                    error: None,
                }
            }
            Request::RemoveManual { label } => {
                if self.state.remove_manual(&label) {
                    self.scan_once();
                    Reply {
                        ok: true,
                        pid: None,
                        error: None,
                    }
                } else {
                    Reply {
                        ok: false,
                        pid: None,
                        error: Some(format!("no entry labelled {label:?}")),
                    }
                }
            }
            Request::Shutdown => {
                self.shutting_down = true;
                Reply {
                    ok: true,
                    pid: None,
                    error: None,
                }
            }
        }
    }

    /// Stop the thing behind `(workspace, port)`. For a host process the caller's
    /// `pid` must still match (PID-reuse guard); for a container the caller's
    /// `container` name must match and `docker stop`/`docker kill` is used.
    fn kill(
        &self,
        workspace: &str,
        port: u16,
        pid: Option<u32>,
        container: Option<&str>,
        signal: Signal,
    ) -> Result<()> {
        let service = self
            .state
            .services_for(workspace)
            .find(|s| s.port == port)
            .ok_or_else(|| anyhow!("no attributed service {workspace}:{port}; rescan and retry"))?;
        match (&service.container, service.pid) {
            (Some(name), _) => {
                if container != Some(name.as_str()) {
                    bail!("{workspace}:{port} is container {name}, not {container:?}; rescan and retry");
                }
                docker_stop(name, signal, self.config.command_timeout())
            }
            (None, Some(current)) => {
                if pid != Some(current) {
                    bail!("{workspace}:{port} is pid {current}, not {pid:?}; rescan and retry");
                }
                send_signal(current, signal)
            }
            (None, None) => bail!("{workspace}:{port} has no live process"),
        }
    }

    fn cleanup(&mut self) {
        let _ = std::fs::remove_file(self.dirs.pid_file());
        let _ = std::fs::remove_file(self.dirs.control_socket());
        if self.herdr.socket_present() {
            let ids: Vec<String> = self
                .state
                .workspace_ids()
                .iter()
                .map(|s| s.to_string())
                .collect();
            for ws in ids {
                let n = usize::from(self.config.sidebar.max_rows);
                let clear: Vec<String> = (0..n).map(sidebar::token_key).collect();
                let _ = self
                    .herdr
                    .report_metadata(&ws, sidebar::SOURCE, &[], &clear, 0);
            }
        }
    }
}

fn summarise(command: &str) -> String {
    const MAX: usize = 120;
    if command.chars().count() <= MAX {
        return command.to_string();
    }
    let mut s: String = command.chars().take(MAX - 1).collect();
    s.push('…');
    s
}

/// `docker stop` (graceful) or `docker kill` for a container by name.
fn docker_stop(name: &str, signal: Signal, timeout: Duration) -> Result<()> {
    let mut command = std::process::Command::new("docker");
    match signal {
        Signal::Term => command.args(["stop", name]),
        Signal::Kill => command.args(["kill", name]),
    };
    // `docker stop` waits up to 10 s for the container by default.
    crate::process::run_checked(command, timeout.max(Duration::from_secs(15)))
        .with_context(|| format!("stop container {name}"))?;
    Ok(())
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: Signal) -> Result<()> {
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: plain syscall with a validated pid; no memory is shared.
    let rc = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("kill {pid}"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _signal: Signal) -> Result<()> {
    bail!("kill is not supported on this platform")
}

#[cfg(unix)]
fn spawn_control_listener(path: &Path, tx: Sender<Wake>) -> Result<()> {
    use std::os::unix::net::UnixListener;
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let tx = tx.clone();
                thread::spawn(move || {
                    let _ = serve_control(stream, tx);
                });
            }
        })?;
    Ok(())
}

#[cfg(unix)]
fn serve_control(stream: std::os::unix::net::UnixStream, tx: Sender<Wake>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let reply = match serde_json::from_str::<Request>(line.trim()) {
        Ok(request) => {
            let (reply_tx, reply_rx) = mpsc::channel();
            tx.send(Wake::Control(request, reply_tx))
                .map_err(|_| anyhow!("daemon loop gone"))?;
            reply_rx
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or(Reply {
                    ok: false,
                    pid: None,
                    error: Some("daemon busy".into()),
                })
        }
        Err(err) => Reply {
            ok: false,
            pid: None,
            error: Some(format!("bad request: {err}")),
        },
    };
    let mut out = serde_json::to_string(&reply)?;
    out.push('\n');
    let mut stream = stream;
    stream.write_all(out.as_bytes())?;
    Ok(())
}

/// Subscribe to topology events; every event wakes the loop, EOF ends it.
fn spawn_event_watcher(herdr: Herdr, tx: Sender<Wake>) {
    thread::Builder::new()
        .name("events".into())
        .spawn(move || {
            let mut stream = match herdr.subscribe(TOPOLOGY_EVENTS) {
                Ok(s) => s,
                Err(_) => {
                    let _ = tx.send(Wake::HerdrGone);
                    return;
                }
            };
            loop {
                match stream.next_event() {
                    Ok(Some(_)) => {
                        if tx.send(Wake::Event).is_err() {
                            return;
                        }
                    }
                    Ok(None) | Err(_) => {
                        let _ = tx.send(Wake::HerdrGone);
                        return;
                    }
                }
            }
        })
        .ok();
}

/// Subscribe to `pane.output_matched` for `pane_ids`; each match becomes a
/// `Wake::Advertised`. Returns a clone of the underlying socket so the caller
/// can force this subscription closed (and the thread ended) once the pane
/// set changes and a fresh one is needed.
fn spawn_output_watcher(
    herdr: &Herdr,
    pane_ids: &[String],
    tx: Sender<Wake>,
) -> Option<std::os::unix::net::UnixStream> {
    if pane_ids.is_empty() {
        return None;
    }
    let mut stream = herdr.subscribe_pane_output(pane_ids).ok()?;
    let handle = stream.try_clone_socket().ok()?;
    thread::Builder::new()
        .name("output-watcher".into())
        .spawn(move || loop {
            match stream.next_output_matched() {
                Ok(Some((pane_id, matched_line))) => {
                    if tx
                        .send(Wake::Advertised {
                            pane_id,
                            matched_line,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) | Err(_) => return,
            }
        })
        .ok()?;
    Some(handle)
}

/// Send one request to a running daemon and return its reply.
#[cfg(unix)]
pub fn control(socket: &Path, request: &Request, timeout: Duration) -> Result<Reply> {
    use std::os::unix::net::UnixStream;
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    serde_json::from_str(reply.trim()).context("bad daemon reply")
}

/// PID of a daemon that answers on this session's control socket, if any.
pub fn live_daemon_pid(dirs: &PluginDirs) -> Option<u32> {
    let socket = dirs.control_socket();
    if !socket.exists() {
        return None;
    }
    control(&socket, &Request::Ping, Duration::from_secs(2))
        .ok()
        .filter(|r| r.ok)
        .and_then(|r| r.pid)
}

/// Start a detached daemon unless one is already alive. Returns the daemon PID.
pub fn ensure_daemon(dirs: &PluginDirs, verbose: bool) -> Result<u32> {
    if let Some(pid) = live_daemon_pid(dirs) {
        return Ok(pid);
    }
    let _ = std::fs::remove_file(dirs.pid_file());
    let _ = std::fs::remove_file(dirs.control_socket());
    let exe = std::env::current_exe().context("current exe")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dirs.log_file())
        .with_context(|| format!("open {}", dirs.log_file().display()))?;
    let mut command = std::process::Command::new(exe);
    command.arg("daemon");
    if verbose {
        command.arg("--verbose");
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    detach(&mut command);
    let child = command.spawn().context("spawn daemon")?;
    let pid = child.id();
    // Wait briefly for the control socket so callers can talk to it right away.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if live_daemon_pid(dirs).is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(pid)
}

#[cfg(unix)]
fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid() is async-signal-safe and touches no shared state.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach(_command: &mut std::process::Command) {}

/// Remove session directories whose herdr socket no longer exists.
pub fn gc_sessions(dirs: &PluginDirs) -> Result<usize> {
    let mut removed = 0;
    let Ok(entries) = std::fs::read_dir(dirs.sessions_dir()) else {
        return Ok(0);
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir == dirs.state {
            continue;
        }
        let marker = dir.join("herdr.sock.path");
        let Ok(socket) = std::fs::read_to_string(&marker) else {
            continue;
        };
        if !PathBuf::from(socket.trim()).exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_as_ndjson() {
        let r: Request =
            serde_json::from_str(r#"{"method":"kill","workspace":"w8","port":3000,"pid":42}"#)
                .unwrap();
        match r {
            Request::Kill {
                workspace,
                port,
                pid,
                container,
                signal,
            } => {
                assert_eq!(
                    (workspace.as_str(), port, pid, container, signal),
                    ("w8", 3000, Some(42), None, Signal::Term)
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        let r: Request = serde_json::from_str(
            r#"{"method":"kill","workspace":"wS","port":32771,"container":"x-postgres-1","signal":"kill"}"#,
        )
        .unwrap();
        assert!(matches!(
            r,
            Request::Kill {
                pid: None,
                container: Some(_),
                signal: Signal::Kill,
                ..
            }
        ));
        assert!(matches!(
            serde_json::from_str::<Request>(r#"{"method":"rescan"}"#).unwrap(),
            Request::Rescan
        ));
        assert!(serde_json::from_str::<Request>(r#"{"method":"reboot"}"#).is_err());
    }

    #[test]
    fn gc_removes_only_sessions_whose_socket_is_gone() {
        let root = std::env::temp_dir().join(format!("herdr-services-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let live_socket = root.join("live.sock");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&live_socket, b"").unwrap();
        let sessions = root.join("sessions");
        for (name, socket) in [
            ("stale", "/nonexistent/herdr.sock"),
            ("live", live_socket.to_str().unwrap()),
        ] {
            let dir = sessions.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("herdr.sock.path"), socket).unwrap();
        }
        // A directory without a marker (e.g. mid-creation) is left alone.
        std::fs::create_dir_all(sessions.join("unmarked")).unwrap();
        let dirs = PluginDirs {
            root: root.clone(),
            state: sessions.join("current"),
            config: root.clone(),
            key: "test".into(),
        };

        assert_eq!(gc_sessions(&dirs).unwrap(), 1);
        assert!(!sessions.join("stale").exists());
        assert!(sessions.join("live").exists());
        assert!(sessions.join("unmarked").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn summarise_caps_long_commands() {
        let long = "x".repeat(300);
        let s = summarise(&long);
        assert_eq!(s.chars().count(), 120);
        assert!(s.ends_with('…'));
        assert_eq!(summarise("short"), "short");
    }
}
