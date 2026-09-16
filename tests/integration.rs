//! Integration test against a real `herdr` binary (SPEC §5). Starts a
//! throwaway named headless session (`herdr --session <name> server`, no
//! PTY needed), creates a workspace, runs a real HTTP server in its pane,
//! and drives the compiled `herdr-services` binary exactly as the plugin
//! manifest does (`ensure-daemon`, `rescan`) — then asserts against
//! `state.json`, the same file the picker and sidebar read.
//!
//! Needs `herdr` and `python3` on `PATH`; ignored by default so `cargo
//! test`/`just ci` stay hermetic. Run explicitly with:
//!
//! ```sh
//! cargo test --test integration -- --ignored --nocapture
//! ```
//!
//! Scope note: this covers attribution-via-pane-ancestry, advertised-url
//! capture and confirmation, and a service disappearing once its process
//! exits (the same mechanism `kill` relies on) — it does not drive the
//! popup picker's TUI itself, which has no practical headless test path.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

const HERDR_ENV_VARS: &[&str] = &[
    "HERDR_SOCKET_PATH",
    "HERDR_CLIENT_SOCKET_PATH",
    "HERDR_SESSION",
    "HERDR_PANE_ID",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
    "HERDR_ENV",
];

/// A throwaway named `herdr` headless session, cleaned up on drop even if an
/// assertion panics.
struct HerdrSession {
    child: Child,
    name: String,
    socket: PathBuf,
    dir: PathBuf,
}

impl HerdrSession {
    fn start(name: &str, config_dir: &std::path::Path) -> Self {
        let mut command = Command::new("herdr");
        command.args(["--session", name, "server"]);
        for var in HERDR_ENV_VARS {
            command.env_remove(var);
        }
        command.env("HERDR_CONFIG_PATH", config_dir.join("config.toml"));
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let child = command
            .spawn()
            .expect("spawn `herdr --session <name> server` (is `herdr` on PATH?)");
        let dir = herdr_config_home().join("sessions").join(name);
        let socket = dir.join("herdr.sock");
        let session = Self {
            child,
            name: name.to_string(),
            socket,
            dir,
        };
        session.wait_for_socket();
        session
    }

    fn wait_for_socket(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.socket.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "herdr session {:?} never created its socket at {}",
            self.name,
            self.socket.display()
        );
    }

    /// Run a herdr CLI command against this session and return its `result`.
    fn cli(&self, args: &[&str]) -> Value {
        let output = Command::new("herdr")
            .args(args)
            .env("HERDR_SOCKET_PATH", &self.socket)
            .output()
            .expect("run herdr cli");
        assert!(
            output.status.success(),
            "herdr {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        if text.trim().is_empty() {
            return Value::Null;
        }
        let reply: Value = serde_json::from_str(text.trim())
            .unwrap_or_else(|_| panic!("herdr {args:?} did not print JSON: {text}"));
        reply.get("result").cloned().unwrap_or(Value::Null)
    }

    /// `pane read` prints the pane's raw text content directly, not the JSON
    /// envelope every other subcommand uses.
    fn cli_raw(&self, args: &[&str]) -> String {
        let output = Command::new("herdr")
            .args(args)
            .env("HERDR_SOCKET_PATH", &self.socket)
            .output()
            .expect("run herdr cli");
        assert!(
            output.status.success(),
            "herdr {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

impl Drop for HerdrSession {
    fn drop(&mut self) {
        let _ = Command::new("herdr")
            .arg("server")
            .arg("stop")
            .env("HERDR_SOCKET_PATH", &self.socket)
            .output();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn herdr_config_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("herdr");
    }
    PathBuf::from(std::env::var_os("HOME").expect("HOME is set")).join(".config/herdr")
}

/// Every `herdr-services` invocation for this test, isolated to its own
/// state directory rather than the shared real one.
struct Plugin {
    socket: PathBuf,
    state_dir: PathBuf,
}

impl Plugin {
    fn run(&self, args: &[&str]) {
        let output = Command::new(env!("CARGO_BIN_EXE_herdr-services"))
            .args(args)
            .env("HERDR_SOCKET_PATH", &self.socket)
            .env("HERDR_BIN_PATH", "herdr")
            .env("HERDR_PLUGIN_STATE_DIR", &self.state_dir)
            .output()
            .expect("run herdr-services");
        assert!(
            output.status.success(),
            "herdr-services {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// `state.json` from whichever single session directory this isolated
    /// state root has created.
    fn state(&self) -> Value {
        let sessions = self.state_dir.join("sessions");
        let entry = std::fs::read_dir(&sessions)
            .unwrap_or_else(|err| panic!("read {}: {err}", sessions.display()))
            .filter_map(Result::ok)
            .find(|e| e.path().join("state.json").exists())
            .unwrap_or_else(|| {
                panic!(
                    "no session dir with state.json under {}",
                    sessions.display()
                )
            });
        let text =
            std::fs::read_to_string(entry.path().join("state.json")).expect("read state.json");
        serde_json::from_str(&text).expect("state.json is valid JSON")
    }
}

/// Poll `f` until it returns `Some`, or panic with `what` after `timeout`.
fn poll<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = f() {
            return value;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn service<'a>(state: &'a Value, workspace_id: &str, port: u16) -> Option<&'a Value> {
    state["services"].as_array()?.iter().find(|s| {
        s["workspace_id"].as_str() == Some(workspace_id) && s["port"].as_u64() == Some(port as u64)
    })
}

#[test]
#[ignore = "needs a real `herdr` binary and `python3` on PATH"]
fn detects_attributes_and_confirms_an_advertised_url_then_notices_it_disappear() {
    let root = std::env::temp_dir().join(format!("herdr-services-it-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let session_name = format!("hs-it-{}", std::process::id());
    let session = HerdrSession::start(&session_name, &root);

    let created = session.cli(&[
        "workspace",
        "create",
        "--cwd",
        root.to_str().unwrap(),
        "--label",
        "it",
    ]);
    let workspace_id = created["workspace"]["workspace_id"]
        .as_str()
        .expect("workspace_id")
        .to_string();
    let pane_id = created["root_pane"]["pane_id"]
        .as_str()
        .expect("pane_id")
        .to_string();

    session.cli(&[
        "pane",
        "run",
        &pane_id,
        "python3 -m http.server --bind 127.0.0.1 0",
    ]);

    let port_pattern = Regex::new(r"port (\d+)").unwrap();
    let port: u16 = poll(
        "http.server to print its port",
        Duration::from_secs(10),
        || {
            let text = session.cli_raw(&["pane", "read", &pane_id, "--source", "recent"]);
            port_pattern
                .captures(&text)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse().ok())
        },
    );

    let plugin = Plugin {
        socket: session.socket.clone(),
        state_dir: root.join("plugin-state"),
    };
    plugin.run(&["ensure-daemon"]);

    poll(
        "the service to be detected with an advertised url",
        Duration::from_secs(30),
        || {
            plugin.run(&["rescan"]);
            let state = plugin.state();
            let matched = service(&state, &workspace_id, port).is_some_and(|s| {
                s["source"] == "advertised" && s["url"] == format!("http://127.0.0.1:{port}/")
            });
            matched.then_some(())
        },
    );
    let state = plugin.state();
    let s = service(&state, &workspace_id, port).expect("service present");
    assert_eq!(
        s["attribution"]["kind"], "pane",
        "attributed via pane ancestry"
    );
    assert_eq!(s["attribution"]["pane_id"], pane_id);

    // Stop the server the same way a user would (Ctrl-C in the pane) and
    // confirm the row disappears once the listener scan stops seeing it —
    // the same mechanism a picker `kill` relies on downstream of the process
    // actually exiting.
    session.cli(&["pane", "send-keys", &pane_id, "c-c"]);
    poll(
        "the service to disappear after the process exits",
        Duration::from_secs(30),
        || {
            plugin.run(&["rescan"]);
            let state = plugin.state();
            service(&state, &workspace_id, port).is_none().then_some(())
        },
    );

    let _ = std::fs::remove_dir_all(&root);
}
