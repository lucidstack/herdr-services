//! The popup picker (SPEC §2.1): reads `state.json`, lists the current
//! workspace's services (or every workspace), and acts on them through the
//! daemon's control socket.
//!
//! Keys: `↑/↓ j/k ctrl-n/p` move · `⏎` open · `y` copy URL · `x` kill ·
//! `X` SIGKILL · `r` rescan · `a` all workspaces · `/` filter · `q`/`esc` close.

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute, queue};

use crate::config::Config;
use crate::daemon::{self, Reply, Request, Signal};
use crate::herdr::{Herdr, PluginDirs};
use crate::sidebar::glyph;
use crate::state::{now_ms, Liveness, Service, State};

const POLL: Duration = Duration::from_millis(250);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// One selectable row.
struct Row {
    workspace_id: String,
    port: u16,
    name: String,
    url: String,
    label: Option<String>,
    glyph: &'static str,
    liveness: Liveness,
    age: String,
    pid: Option<u32>,
    container: Option<String>,
    /// Unattributed listeners can be opened and copied but not killed.
    killable: bool,
}

/// Rendered line: either a group header or a row index.
enum Line {
    Header(String),
    Row(usize),
}

struct Picker {
    dirs: PluginDirs,
    config: Config,
    workspace: Option<String>,
    all: bool,
    filter: Option<String>,
    selected: usize,
    scroll: usize,
    status: Option<(String, bool)>,
    kill_confirmed_once: bool,
    pending_kill: Option<(usize, Signal)>,
    filter_editing: bool,
    state: State,
    state_mtime: Option<SystemTime>,
    rows: Vec<Row>,
    lines: Vec<Line>,
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

pub fn run() -> Result<()> {
    let herdr = Herdr::from_env(Duration::from_secs(4))?;
    let dirs = PluginDirs::from_env(&herdr.socket)?;
    let config = Config::load(&dirs.config_file())?;
    let workspace = current_workspace();
    if daemon::live_daemon_pid(&dirs).is_none() {
        daemon::ensure_daemon(&dirs, false)?;
    }
    let mut picker = Picker {
        dirs,
        config,
        workspace,
        all: false,
        filter: None,
        selected: 0,
        scroll: 0,
        status: None,
        kill_confirmed_once: false,
        pending_kill: None,
        filter_editing: false,
        state: State::default(),
        state_mtime: None,
        rows: Vec::new(),
        lines: Vec::new(),
    };
    picker.all = picker.workspace.is_none();
    picker.reload(true);
    let _guard = TerminalGuard::enter()?;
    picker.event_loop()
}

/// Workspace of the invoking context: env, then the plugin context JSON.
fn current_workspace() -> Option<String> {
    if let Ok(ws) = std::env::var("HERDR_WORKSPACE_ID") {
        if !ws.is_empty() {
            return Some(ws);
        }
    }
    let json = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    value
        .get("workspace_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

impl Picker {
    fn event_loop(&mut self) -> Result<()> {
        loop {
            self.render()?;
            if event::poll(POLL)? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if !self.handle_key(key)? {
                            return Ok(());
                        }
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            } else {
                self.reload(false);
            }
        }
    }

    /// Re-read `state.json` when it changed (or `force`), rebuild rows.
    fn reload(&mut self, force: bool) {
        let path = self.dirs.state_file();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if !force && mtime == self.state_mtime {
            return;
        }
        self.state_mtime = mtime;
        if let Ok(state) = State::read(&path) {
            self.state = state;
        }
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let now = now_ms();
        let filter = self.filter.clone().unwrap_or_default().to_lowercase();
        let matches = |r: &Row| {
            filter.is_empty()
                || r.port.to_string().contains(&filter)
                || r.name.to_lowercase().contains(&filter)
                || r.url.to_lowercase().contains(&filter)
                || r.label
                    .as_deref()
                    .is_some_and(|l| l.to_lowercase().contains(&filter))
                || r.workspace_id.to_lowercase().contains(&filter)
        };
        let to_row = |s: &Service| Row {
            workspace_id: s.workspace_id.clone(),
            port: s.port,
            name: s.process_name.clone(),
            url: s.url.clone(),
            label: s.label.clone(),
            glyph: glyph(s),
            liveness: s.liveness,
            age: age(now, s.first_seen_ms),
            pid: s.pid,
            container: s.container.clone(),
            killable: s.pid.is_some() || s.container.is_some(),
        };
        let mut rows = Vec::new();
        let mut lines = Vec::new();
        let services: Vec<&Service> = match (&self.all, &self.workspace) {
            (false, Some(ws)) => self.state.services_for(ws).collect(),
            _ => self.state.services.iter().collect(),
        };
        if self.all {
            let mut ids = self.state.workspace_ids();
            ids.sort_by_key(|id| {
                self.state
                    .workspace_labels
                    .get(*id)
                    .cloned()
                    .unwrap_or_default()
            });
            for ws in ids {
                let group: Vec<Row> = services
                    .iter()
                    .filter(|s| s.workspace_id == ws)
                    .map(|s| to_row(s))
                    .filter(matches)
                    .collect();
                if group.is_empty() {
                    continue;
                }
                let label = self
                    .state
                    .workspace_labels
                    .get(ws)
                    .cloned()
                    .unwrap_or_default();
                lines.push(Line::Header(format!("{label} ({ws})")));
                for r in group {
                    lines.push(Line::Row(rows.len()));
                    rows.push(r);
                }
            }
            let other: Vec<Row> = self
                .state
                .unattributed
                .iter()
                .map(|u| Row {
                    workspace_id: String::new(),
                    port: u.port,
                    name: u.process_name.clone(),
                    url: self.config.url_for(u.port),
                    label: None,
                    glyph: "○",
                    liveness: Liveness::Unknown,
                    age: String::new(),
                    pid: u.pid,
                    container: u.container.clone(),
                    killable: false,
                })
                .filter(matches)
                .collect();
            if !other.is_empty() {
                lines.push(Line::Header("other".into()));
                for r in other {
                    lines.push(Line::Row(rows.len()));
                    rows.push(r);
                }
            }
        } else {
            for r in services.iter().map(|s| to_row(s)).filter(matches) {
                lines.push(Line::Row(rows.len()));
                rows.push(r);
            }
        }
        self.rows = rows;
        self.lines = lines;
        if self.rows.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.rows.len() - 1);
        }
    }

    /// Returns false when the picker should close.
    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        // Pending kill confirmation swallows the next key.
        if let Some((index, signal)) = self.pending_kill.take() {
            if matches!(
                key.code,
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
            ) {
                self.kill_confirmed_once = true;
                self.kill(index, signal);
            } else {
                self.status = Some(("kill cancelled".into(), false));
            }
            return Ok(true);
        }
        // Filter editing: text entry until Enter (keep) or Esc (clear).
        if self.filter_editing {
            let filter = self.filter.get_or_insert_with(String::new);
            match key.code {
                KeyCode::Esc => {
                    self.filter = None;
                    self.filter_editing = false;
                    self.rebuild();
                }
                KeyCode::Enter => {
                    if filter.is_empty() {
                        self.filter = None;
                    }
                    self.filter_editing = false;
                }
                KeyCode::Backspace => {
                    filter.pop();
                    self.rebuild();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    filter.clear();
                    self.rebuild();
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(false);
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    filter.push(c);
                    self.rebuild();
                }
                KeyCode::Up | KeyCode::Down => self.move_selection(key.code == KeyCode::Down),
                _ => {}
            }
            return Ok(true);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc if self.filter.is_some() => {
                self.filter = None;
                self.rebuild();
            }
            KeyCode::Esc | KeyCode::Char('q') => return Ok(false),
            KeyCode::Char('c') if ctrl => return Ok(false),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(false),
            KeyCode::Char('p') if ctrl => self.move_selection(false),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(true),
            KeyCode::Char('n') if ctrl => self.move_selection(true),
            KeyCode::Enter => self.open_selected(),
            KeyCode::Char('y') => self.copy_selected(),
            KeyCode::Char('x') => self.request_kill(Signal::Term),
            KeyCode::Char('X') => self.request_kill(Signal::Kill),
            KeyCode::Char('r') => self.rescan(),
            KeyCode::Char('a') => {
                self.all = !self.all;
                self.selected = 0;
                self.rebuild();
            }
            KeyCode::Char('/') => {
                self.filter.get_or_insert_with(String::new);
                self.filter_editing = true;
            }
            _ => {}
        }
        Ok(true)
    }

    fn move_selection(&mut self, down: bool) {
        if self.rows.is_empty() {
            return;
        }
        if down {
            self.selected = (self.selected + 1).min(self.rows.len() - 1);
        } else {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    fn open_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let url = row.url.clone();
        match open_url(&url) {
            Ok(()) => self.status = Some((format!("opened {url}"), false)),
            Err(err) => self.status = Some((format!("open failed: {err:#}"), true)),
        }
    }

    fn copy_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let url = row.url.clone();
        match copy_to_clipboard(&url) {
            Ok(how) => self.status = Some((format!("copied {url} ({how})"), false)),
            Err(err) => self.status = Some((format!("copy failed: {err:#}"), true)),
        }
    }

    fn request_kill(&mut self, signal: Signal) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if !row.killable {
            self.status = Some((
                "not attributed to a workspace; kill it yourself".into(),
                true,
            ));
            return;
        }
        if self.config.picker.confirm_kill && !self.kill_confirmed_once {
            let what = match (&row.container, row.pid) {
                (Some(c), _) => format!("container {c}"),
                (None, Some(pid)) => format!("{} (pid {pid})", row.name),
                _ => row.name.clone(),
            };
            let verb = match signal {
                Signal::Term => "stop",
                Signal::Kill => "SIGKILL",
            };
            self.status = Some((format!("{verb} {what}? y/n"), true));
            self.pending_kill = Some((self.selected, signal));
            return;
        }
        self.kill(self.selected, signal);
    }

    fn kill(&mut self, index: usize, signal: Signal) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        let request = Request::Kill {
            workspace: row.workspace_id.clone(),
            port: row.port,
            pid: row.pid,
            container: row.container.clone(),
            signal,
        };
        let name = row.name.clone();
        let row_is_container = row.container.is_some();
        match self.control(&request) {
            Ok(reply) if reply.ok => {
                let sent = match signal {
                    Signal::Term => "SIGTERM",
                    Signal::Kill => "SIGKILL",
                };
                let target = if row_is_container {
                    "docker stop/kill"
                } else {
                    sent
                };
                self.status = Some((format!("sent {target} to {name}"), false));
                self.reload(true);
            }
            Ok(reply) => {
                self.status = Some((reply.error.unwrap_or_else(|| "kill failed".into()), true));
            }
            Err(err) => self.status = Some((format!("daemon unreachable: {err:#}"), true)),
        }
    }

    fn rescan(&mut self) {
        match self.control(&Request::Rescan) {
            Ok(reply) if reply.ok => {
                self.status = Some(("rescanned".into(), false));
                self.reload(true);
            }
            Ok(reply) => self.status = Some((reply.error.unwrap_or_default(), true)),
            Err(err) => self.status = Some((format!("daemon unreachable: {err:#}"), true)),
        }
    }

    fn control(&self, request: &Request) -> Result<Reply> {
        daemon::control(&self.dirs.control_socket(), request, CONTROL_TIMEOUT)
    }

    fn render(&mut self) -> Result<()> {
        let (width, height) = terminal::size()?;
        let width = width as usize;
        let height = height as usize;
        let mut out = io::stdout();
        queue!(out, Clear(ClearType::All), cursor::MoveTo(0, 0))?;

        // Header
        let scope = if self.all {
            "all workspaces".to_string()
        } else {
            let ws = self.workspace.as_deref().unwrap_or("");
            self.state
                .workspace_labels
                .get(ws)
                .cloned()
                .unwrap_or_else(|| ws.to_string())
        };
        let listening = self
            .rows
            .iter()
            .filter(|r| r.liveness == Liveness::Up)
            .count();
        let right = format!("{listening} listening");
        let left = format!(" services · {scope}");
        let pad = width.saturating_sub(left.chars().count() + right.chars().count() + 1);
        queue!(
            out,
            SetAttribute(Attribute::Bold),
            Print(&left),
            SetAttribute(Attribute::Reset),
            Print(" ".repeat(pad)),
            SetForegroundColor(Color::DarkGrey),
            Print(&right),
            ResetColor,
            Print("\r\n"),
            SetForegroundColor(Color::DarkGrey),
            Print("─".repeat(width)),
            ResetColor,
            Print("\r\n"),
        )?;

        // Body
        let footer_lines = 3;
        let body_height = height.saturating_sub(2 + footer_lines).max(1);
        let selected_line = self
            .lines
            .iter()
            .position(|l| matches!(l, Line::Row(i) if *i == self.selected))
            .unwrap_or(0);
        if selected_line < self.scroll {
            self.scroll = selected_line;
        } else if selected_line >= self.scroll + body_height {
            self.scroll = selected_line + 1 - body_height;
        }
        let name_w = self
            .rows
            .iter()
            .map(|r| r.name.chars().count())
            .max()
            .unwrap_or(4)
            .clamp(4, 20);
        let url_w = self
            .rows
            .iter()
            .map(|r| r.url.chars().count())
            .max()
            .unwrap_or(0)
            .min(40);
        let mut drawn = 0;
        for line in self.lines.iter().skip(self.scroll).take(body_height) {
            match line {
                Line::Header(text) => {
                    queue!(
                        out,
                        SetAttribute(Attribute::Bold),
                        Print(format!(" {text}")),
                        SetAttribute(Attribute::Reset),
                        Print("\r\n")
                    )?;
                }
                Line::Row(i) => {
                    let row = &self.rows[*i];
                    let selected = *i == self.selected;
                    let colour = match row.glyph {
                        "●" => Color::Green,
                        "◌" => Color::Red,
                        _ => Color::DarkGrey,
                    };
                    let mut text = format!(
                        " {:<name_w$}  :{:<5}  {:<url_w$}",
                        truncate(&row.name, name_w),
                        row.port,
                        truncate(&row.url, url_w),
                    );
                    if let Some(label) = &row.label {
                        text.push_str(&format!("  {label}"));
                    }
                    if let Some(c) = &row.container {
                        text.push_str(&format!("  ⧉ {}", truncate(c, 24)));
                    }
                    let text = truncate(&text, width.saturating_sub(6));
                    let pad = width
                        .saturating_sub(3 + text.chars().count() + row.age.chars().count() + 1);
                    if selected {
                        queue!(out, SetAttribute(Attribute::Reverse))?;
                    }
                    queue!(
                        out,
                        Print(" "),
                        SetForegroundColor(colour),
                        Print(row.glyph),
                        ResetColor,
                    )?;
                    if selected {
                        queue!(out, SetAttribute(Attribute::Reverse))?;
                    }
                    queue!(
                        out,
                        Print(&text),
                        Print(" ".repeat(pad)),
                        SetForegroundColor(Color::DarkGrey),
                        Print(&row.age),
                        Print(" "),
                        ResetColor,
                        SetAttribute(Attribute::Reset),
                        Print("\r\n")
                    )?;
                }
            }
            drawn += 1;
        }
        if self.rows.is_empty() {
            let msg = if self.state.services.is_empty() && self.state.last_scan_ms == 0 {
                "waiting for the first scan…"
            } else if self.filter.is_some() {
                "no matches"
            } else {
                "no services detected in this workspace (a for all)"
            };
            queue!(
                out,
                SetForegroundColor(Color::DarkGrey),
                Print(format!(" {msg}")),
                ResetColor,
                Print("\r\n")
            )?;
            drawn += 1;
        }
        for _ in drawn..body_height {
            queue!(out, Print("\r\n"))?;
        }

        // Footer
        queue!(
            out,
            SetForegroundColor(Color::DarkGrey),
            Print("─".repeat(width)),
            ResetColor,
            Print("\r\n")
        )?;
        match (&self.status, &self.filter, self.filter_editing) {
            (Some((msg, is_error)), _, _) => {
                let colour = if *is_error {
                    Color::Yellow
                } else {
                    Color::Green
                };
                queue!(
                    out,
                    SetForegroundColor(colour),
                    Print(format!(" {}", truncate(msg, width - 2))),
                    ResetColor
                )?;
            }
            (None, Some(f), true) => {
                queue!(
                    out,
                    Print(format!(" /{f}")),
                    SetAttribute(Attribute::Reverse),
                    Print(" "),
                    SetAttribute(Attribute::Reset)
                )?;
            }
            (None, Some(f), false) => {
                queue!(
                    out,
                    SetForegroundColor(Color::DarkGrey),
                    Print(format!(" filter: {f}  (/ edit, esc clear)")),
                    ResetColor
                )?;
            }
            (None, None, _) => {
                if let Some(err) = &self.state.last_error {
                    queue!(
                        out,
                        SetForegroundColor(Color::Yellow),
                        Print(format!(" daemon: {}", truncate(err, width - 10))),
                        ResetColor
                    )?;
                }
            }
        }
        queue!(out, Print("\r\n"))?;
        let all_label = if self.all { "this ws" } else { "all" };
        let full = format!(
            " ⏎ open   y copy url   x kill   X kill -9   r rescan   a {all_label}   / filter   q close"
        );
        let compact = format!(
            " ⏎ open  y copy  x kill  X kill-9  r rescan  a {all_label}  / filter  q close"
        );
        let help = if full.chars().count() <= width {
            full
        } else {
            compact
        };
        queue!(
            out,
            SetForegroundColor(Color::DarkGrey),
            Print(truncate(&help, width)),
            ResetColor
        )?;
        out.flush()?;
        Ok(())
    }
}

fn age(now_ms: u64, first_seen_ms: u64) -> String {
    if first_seen_ms == 0 || now_ms < first_seen_ms {
        return String::new();
    }
    let secs = (now_ms - first_seen_ms) / 1000;
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n - 1).collect();
        t.push('…');
        t
    }
}

/// Open a URL in the system browser, detached from the popup.
fn open_url(url: &str) -> Result<()> {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else {
        ("xdg-open", vec![url])
    };
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {program}"))?;
    Ok(())
}

/// Copy via a native clipboard tool when present, else OSC 52 through the
/// terminal. Returns a short description of the path taken.
fn copy_to_clipboard(text: &str) -> Result<&'static str> {
    let candidates: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (program, args) in candidates {
        if !on_path(program) {
            continue;
        }
        let mut child = std::process::Command::new(program)
            .args(*args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("spawn {program}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        if status.success() {
            return Ok(program);
        }
    }
    let encoded = base64(text.as_bytes());
    let mut out = io::stdout();
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()?;
    Ok("osc 52")
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir: PathBuf| dir.join(program).is_file())
    })
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_buckets() {
        assert_eq!(age(10_000, 0), "");
        assert_eq!(age(65_000, 5_000), "1m");
        assert_eq!(age(3_600_000 * 2 + 1000, 1000), "2h");
        assert_eq!(age(86_400_000 * 3, 0), "");
        assert_eq!(age(86_400_000 * 3 + 1, 1), "3d");
        assert_eq!(age(30_000, 1_000), "29s");
    }

    #[test]
    fn base64_matches_rfc_examples() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(
            base64(b"http://localhost:3000"),
            "aHR0cDovL2xvY2FsaG9zdDozMDAw"
        );
    }
}
