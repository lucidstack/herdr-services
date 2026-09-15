//! Thin client for the herdr CLI and socket API.
//!
//! Every call carries a timeout; the daemon must never block on herdr.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::process::run_checked;

#[derive(Debug, Clone)]
pub struct Herdr {
    pub bin: PathBuf,
    pub socket: PathBuf,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub worktree: Option<Worktree>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Worktree {
    pub checkout_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pane {
    pub pane_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub foreground_cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pong {
    pub version: String,
    pub protocol: u32,
}

impl Herdr {
    /// Build a client from the environment herdr injects into plugin commands
    /// (`HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`).
    pub fn from_env(timeout: Duration) -> Result<Self> {
        let bin = std::env::var_os("HERDR_BIN_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("herdr"));
        let socket = std::env::var_os("HERDR_SOCKET_PATH")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HERDR_SOCKET_PATH is not set; run inside herdr"))?;
        Ok(Self {
            bin,
            socket,
            timeout,
        })
    }

    /// True while the herdr socket file exists.
    pub fn socket_present(&self) -> bool {
        self.socket.exists()
    }

    pub fn workspaces(&self) -> Result<Vec<Workspace>> {
        let result = self.cli(&["workspace", "list"])?;
        parse_field(result, "workspaces")
    }

    pub fn panes(&self) -> Result<Vec<Pane>> {
        let result = self.cli(&["pane", "list"])?;
        parse_field(result, "panes")
    }

    /// Shell PID of a pane, `None` when herdr has no process info for it.
    pub fn shell_pid(&self, pane_id: &str) -> Result<Option<u32>> {
        let result = self.cli(&["pane", "process-info", "--pane", pane_id])?;
        Ok(result
            .get("process_info")
            .and_then(|p| p.get("shell_pid"))
            .and_then(Value::as_u64)
            .map(|pid| pid as u32))
    }

    /// Set and clear workspace metadata tokens in one report. `ttl_ms` applies
    /// to the set tokens. At most 16 keys per call (herdr's limit).
    pub fn report_metadata(
        &self,
        workspace_id: &str,
        source: &str,
        set: &[(String, String)],
        clear: &[String],
        ttl_ms: u64,
    ) -> Result<()> {
        if set.is_empty() && clear.is_empty() {
            return Ok(());
        }
        let ttl = ttl_ms.to_string();
        let mut args: Vec<String> = vec![
            "workspace".into(),
            "report-metadata".into(),
            workspace_id.into(),
            "--source".into(),
            source.into(),
        ];
        for (key, value) in set {
            args.push("--token".into());
            args.push(format!("{key}={value}"));
        }
        for key in clear {
            args.push("--clear-token".into());
            args.push(key.clone());
        }
        if !set.is_empty() {
            args.push("--ttl-ms".into());
            args.push(ttl);
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.cli(&refs)?;
        Ok(())
    }

    /// `herdr server reload-config`.
    pub fn reload_config(&self) -> Result<()> {
        let mut command = Command::new(&self.bin);
        command.args(["server", "reload-config"]);
        command.env("HERDR_SOCKET_PATH", &self.socket);
        run_checked(command, self.timeout)?;
        Ok(())
    }

    /// Open a persistent event stream for the given event types. The returned
    /// reader yields one raw JSON line per event; EOF means herdr went away.
    #[cfg(unix)]
    pub fn subscribe(&self, types: &[&str]) -> Result<EventStream> {
        use std::os::unix::net::UnixStream;
        let mut stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect {}", self.socket.display()))?;
        stream.set_write_timeout(Some(self.timeout))?;
        let subscriptions: Vec<Value> = types
            .iter()
            .map(|t| serde_json::json!({ "type": t }))
            .collect();
        let request = serde_json::json!({
            "id": "herdr-services-events",
            "method": "events.subscribe",
            "params": { "subscriptions": subscriptions },
        });
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        stream.write_all(line.as_bytes())?;
        stream.set_read_timeout(Some(self.timeout))?;
        let mut reader = BufReader::new(stream);
        let mut ack = String::new();
        reader
            .read_line(&mut ack)
            .context("read subscribe acknowledgement")?;
        let ack: Value = serde_json::from_str(ack.trim()).context("subscribe ack is not JSON")?;
        if let Some(err) = ack.get("error") {
            bail!("events.subscribe: {err}");
        }
        // Events arrive whenever; no read timeout from here on.
        reader.get_ref().set_read_timeout(None)?;
        Ok(EventStream { reader })
    }

    pub fn open_plugin_pane(
        &self,
        plugin_id: &str,
        entrypoint: &str,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        let mut args = vec![
            "plugin",
            "pane",
            "open",
            "--plugin",
            plugin_id,
            "--entrypoint",
            entrypoint,
            "--focus",
        ];
        if let Some(ws) = workspace_id {
            args.extend(["--workspace", ws]);
        }
        self.cli(&args)?;
        Ok(())
    }

    /// Socket `ping`; fails when herdr is gone or unresponsive within the timeout.
    pub fn ping(&self) -> Result<Pong> {
        let result = self.socket_call("ping", serde_json::json!({}))?;
        serde_json::from_value(result).context("unexpected pong shape")
    }

    /// Run a herdr CLI command and return the `result` object of its JSON reply.
    fn cli(&self, args: &[&str]) -> Result<Value> {
        let mut command = Command::new(&self.bin);
        command.args(args);
        command.env("HERDR_SOCKET_PATH", &self.socket);
        let stdout = run_checked(command, self.timeout)?;
        // Mutating commands (report-metadata, server reload-config) print nothing on success.
        if stdout.trim().is_empty() {
            return Ok(Value::Null);
        }
        let reply: Value = serde_json::from_str(stdout.trim())
            .with_context(|| format!("herdr {}: reply is not JSON", args.join(" ")))?;
        if let Some(err) = reply.get("error") {
            bail!("herdr {}: {}", args.join(" "), err);
        }
        reply
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("herdr {}: reply has no result", args.join(" ")))
    }

    #[cfg(unix)]
    fn socket_call(&self, method: &str, params: Value) -> Result<Value> {
        use std::os::unix::net::UnixStream;
        let mut stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect {}", self.socket.display()))?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        let request =
            serde_json::json!({"id": "herdr-services", "method": method, "params": params});
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        stream.write_all(line.as_bytes())?;
        let mut reader = BufReader::new(stream);
        let mut reply = String::new();
        reader.read_line(&mut reply).context("read socket reply")?;
        let reply: Value =
            serde_json::from_str(reply.trim()).context("socket reply is not JSON")?;
        if let Some(err) = reply.get("error") {
            bail!("herdr {method}: {err}");
        }
        reply
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("herdr {method}: reply has no result"))
    }

    #[cfg(not(unix))]
    fn socket_call(&self, _method: &str, _params: Value) -> Result<Value> {
        bail!("socket API is only supported on Unix in v0.1")
    }
}

fn parse_field<T: for<'de> Deserialize<'de>>(result: Value, field: &str) -> Result<T> {
    let value = result
        .get(field)
        .cloned()
        .ok_or_else(|| anyhow!("herdr reply has no `{field}`"))?;
    serde_json::from_value(value).with_context(|| format!("decode `{field}`"))
}

/// A live `events.subscribe` connection.
#[cfg(unix)]
pub struct EventStream {
    reader: BufReader<std::os::unix::net::UnixStream>,
}

#[cfg(unix)]
impl EventStream {
    /// Next event's name (e.g. `pane_created`), or `None` at EOF (herdr closed the socket).
    pub fn next_event(&mut self) -> Result<Option<String>> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).context("read event")?;
            if n == 0 {
                return Ok(None);
            }
            if let Some(name) = event_name(&line) {
                return Ok(Some(name));
            }
        }
    }
}

/// Pushed events arrive as `{"event":"pane_created","data":{…}}`; anything else
/// on the stream (acks, keep-alives) yields `None`.
pub fn event_name(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    value.get("event")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_name_reads_pushed_frames_only() {
        let frame = r#"{"data":{"pane":{"pane_id":"w1:p3"}},"event":"pane_created"}"#;
        assert_eq!(event_name(frame).as_deref(), Some("pane_created"));
        assert_eq!(
            event_name(r#"{"id":"e","result":{"type":"subscription_started"}}"#),
            None
        );
        assert_eq!(event_name("not json"), None);
    }

    #[test]
    fn session_key_is_stable_and_path_sensitive() {
        let a = session_key(Path::new("/a/herdr.sock"));
        assert_eq!(a, session_key(Path::new("/a/herdr.sock")));
        assert_ne!(a, session_key(Path::new("/b/herdr.sock")));
        assert_eq!(a.len(), 16);
    }
}

/// Plugin-owned directories. State is per herdr *session*: herdr's plugin state
/// dir is shared by every session (`src/plugin_paths.rs`), so the daemon,
/// pidfile and `state.json` live in a subdirectory keyed by the socket path.
/// The control socket lives in a short runtime dir because Unix socket paths
/// are capped at ~104 bytes and the state dir alone is longer than that.
#[derive(Debug, Clone)]
pub struct PluginDirs {
    /// Plugin-wide state root (`HERDR_PLUGIN_STATE_DIR`).
    pub root: PathBuf,
    /// This session's state directory.
    pub state: PathBuf,
    pub config: PathBuf,
    /// Session key; names the control socket in the runtime dir.
    pub key: String,
}

impl PluginDirs {
    pub fn from_env(socket: &Path) -> Result<Self> {
        let root = std::env::var_os("HERDR_PLUGIN_STATE_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HERDR_PLUGIN_STATE_DIR is not set; run as a herdr plugin"))?;
        let config = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.clone());
        let key = session_key(socket);
        let state = root.join("sessions").join(&key);
        std::fs::create_dir_all(&state).with_context(|| format!("create {}", state.display()))?;
        Ok(Self {
            root,
            state,
            config,
            key,
        })
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state.join("state.json")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.state.join("daemon.pid")
    }

    /// `<runtime>/herdr-services/<key>.sock`. Creates the directory (0700).
    pub fn control_socket(&self) -> PathBuf {
        let dir = runtime_dir();
        if std::fs::create_dir_all(&dir).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        dir.join(format!("{}.sock", self.key))
    }

    /// The herdr socket this session directory belongs to.
    pub fn socket_marker(&self) -> PathBuf {
        self.state.join("herdr.sock.path")
    }

    pub fn log_file(&self) -> PathBuf {
        self.state.join("daemon.log")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }
}

/// `$XDG_RUNTIME_DIR/herdr-services`, else `<temp>/herdr-services-<uid>`.
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("herdr-services");
    }
    #[cfg(unix)]
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = 0;
    std::env::temp_dir().join(format!("herdr-services-{uid}"))
}

/// Short stable key for a socket path (FNV-1a, hex).
pub fn session_key(socket: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in socket.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// herdr's own `config.toml`: `HERDR_CONFIG_PATH`, else `$XDG_CONFIG_HOME/herdr/config.toml`,
/// else `~/.config/herdr/config.toml`.
pub fn herdr_config_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("HERDR_CONFIG_PATH") {
        return Ok(PathBuf::from(p));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("herdr").join("config.toml"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("herdr")
        .join("config.toml"))
}
