//! Deciding which workspace owns a listener (SPEC §3.3).
//!
//! Order, first hit wins: pane ancestry → cwd under a workspace root →
//! workspace root on the command line. Pure functions over a [`Topology`]
//! snapshot so the rules are unit-testable with fixtures.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::scan::Snapshot;

/// A herdr pane as needed for attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRef {
    pub pane_id: String,
    pub workspace_id: String,
    pub shell_pid: Option<u32>,
    pub cwds: Vec<PathBuf>,
}

/// A herdr workspace as needed for attribution and display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRef {
    pub workspace_id: String,
    pub label: String,
    pub checkout_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct Topology {
    pub workspaces: Vec<WorkspaceRef>,
    pub panes: Vec<PaneRef>,
    /// Directories treated as too broad to own anything (`$HOME`, `/`).
    pub excluded_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Attribution {
    Pane {
        pane_id: String,
    },
    Cwd,
    Command,
    /// Compose project `working_dir` label under a workspace root.
    Container,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attributed {
    pub workspace_id: String,
    pub attribution: Attribution,
}

impl Topology {
    pub fn new(workspaces: Vec<WorkspaceRef>, panes: Vec<PaneRef>) -> Self {
        let mut excluded_roots = vec![PathBuf::from("/")];
        if let Some(home) = std::env::var_os("HOME") {
            excluded_roots.push(PathBuf::from(home));
        }
        Self {
            workspaces,
            panes,
            excluded_roots,
        }
    }

    pub fn label_of(&self, workspace_id: &str) -> Option<&str> {
        self.workspaces
            .iter()
            .find(|w| w.workspace_id == workspace_id)
            .map(|w| w.label.as_str())
    }

    /// Shell PID → workspace/pane for the ancestry rule.
    fn shell_index(&self) -> HashMap<u32, &PaneRef> {
        self.panes
            .iter()
            .filter_map(|p| p.shell_pid.map(|pid| (pid, p)))
            .collect()
    }

    /// Candidate directory roots with their owning workspace, deepest first.
    /// Duplicates keep the first owner (worktree paths are listed before pane cwds).
    pub fn roots(&self) -> Vec<(PathBuf, &str)> {
        let candidates = self
            .workspaces
            .iter()
            .filter_map(|w| {
                w.checkout_path
                    .as_deref()
                    .map(|p| (p, w.workspace_id.as_str()))
            })
            .chain(self.panes.iter().flat_map(|pane| {
                pane.cwds
                    .iter()
                    .map(|c| (c.as_path(), pane.workspace_id.as_str()))
            }));
        let mut roots: Vec<(PathBuf, &str)> = Vec::new();
        for (path, ws) in candidates {
            let path = normalise(path);
            if self.excluded_roots.iter().any(|x| normalise(x) == path) {
                continue;
            }
            if roots.iter().any(|(p, _)| *p == path) {
                continue;
            }
            roots.push((path, ws));
        }
        roots.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        roots
    }
}

/// Attribute the process `pid` from `snapshot` to a workspace, if any rule matches.
pub fn attribute(pid: u32, snapshot: &Snapshot, topology: &Topology) -> Option<Attributed> {
    if let Some(hit) = by_ancestry(pid, snapshot, topology) {
        return Some(hit);
    }
    let roots = topology.roots();
    if let Some(cwd) = snapshot.cwds.get(&pid) {
        if let Some(ws) = deepest_root_containing(&roots, cwd) {
            return Some(Attributed {
                workspace_id: ws.to_string(),
                attribution: Attribution::Cwd,
            });
        }
    }
    if let Some(command) = snapshot.command_of(pid) {
        if let Some(ws) = root_on_command_line(&roots, command) {
            return Some(Attributed {
                workspace_id: ws.to_string(),
                attribution: Attribution::Command,
            });
        }
    }
    None
}

/// Attribute a container by its compose project directory (deepest workspace root).
pub fn attribute_container(working_dir: &Path, topology: &Topology) -> Option<Attributed> {
    let roots = topology.roots();
    deepest_root_containing(&roots, working_dir).map(|ws| Attributed {
        workspace_id: ws.to_string(),
        attribution: Attribution::Container,
    })
}

fn by_ancestry(pid: u32, snapshot: &Snapshot, topology: &Topology) -> Option<Attributed> {
    let shells = topology.shell_index();
    if shells.is_empty() {
        return None;
    }
    if let Some(pane) = shells.get(&pid) {
        return Some(hit(pane));
    }
    snapshot
        .ancestors(pid)
        .into_iter()
        .find_map(|ancestor| shells.get(&ancestor).map(|pane| hit(pane)))
}

fn hit(pane: &PaneRef) -> Attributed {
    Attributed {
        workspace_id: pane.workspace_id.clone(),
        attribution: Attribution::Pane {
            pane_id: pane.pane_id.clone(),
        },
    }
}

/// `roots` must be sorted deepest first (see [`Topology::roots`]).
fn deepest_root_containing<'a>(roots: &[(PathBuf, &'a str)], cwd: &Path) -> Option<&'a str> {
    let cwd = normalise(cwd);
    roots
        .iter()
        .find(|(root, _)| cwd.starts_with(root))
        .map(|(_, ws)| *ws)
}

/// A root appears on the command line on a path boundary: preceded by start,
/// whitespace, `=`, `:` or a quote, and followed by end, whitespace, `/`, a quote
/// or `:`. Mirrors Orca's `includesPathBoundary`.
fn root_on_command_line<'a>(roots: &[(PathBuf, &'a str)], command: &str) -> Option<&'a str> {
    roots
        .iter()
        .find(|(root, _)| {
            let needle = root.to_string_lossy();
            path_on_boundary(command, &needle)
        })
        .map(|(_, ws)| *ws)
}

pub fn path_on_boundary(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(idx) = haystack[from..].find(needle) {
        let start = from + idx;
        let end = start + needle.len();
        let before_ok = start == 0
            || haystack[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || matches!(c, '=' | ':' | '"' | '\''));
        let after_ok = end == haystack.len()
            || haystack[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_whitespace() || matches!(c, '/' | ':' | '"' | '\''));
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Strip trailing slashes and `.` components without touching the filesystem.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ProcessInfo, Snapshot};

    fn topology() -> Topology {
        let mut t = Topology::new(
            vec![
                WorkspaceRef {
                    workspace_id: "w8".into(),
                    label: "acme-api".into(),
                    checkout_path: Some("/home/u/projects/acme-api".into()),
                },
                WorkspaceRef {
                    workspace_id: "wN".into(),
                    label: "feature-x".into(),
                    checkout_path: Some("/home/u/projects/worktrees/acme-api/feature-x".into()),
                },
                WorkspaceRef {
                    workspace_id: "wG".into(),
                    label: "blog".into(),
                    checkout_path: None,
                },
            ],
            vec![
                PaneRef {
                    pane_id: "w8:p1".into(),
                    workspace_id: "w8".into(),
                    shell_pid: Some(100),
                    cwds: vec!["/home/u/projects/acme-api".into()],
                },
                PaneRef {
                    pane_id: "wG:p1".into(),
                    workspace_id: "wG".into(),
                    shell_pid: Some(200),
                    cwds: vec!["/home/u".into(), "/home/u/projects/blog/".into()],
                },
            ],
        );
        t.excluded_roots = vec!["/".into(), "/home/u".into()];
        t
    }

    fn snapshot() -> Snapshot {
        let mut s = Snapshot::default();
        let mut proc = |pid, ppid, command: &str| {
            s.processes.insert(
                pid,
                ProcessInfo {
                    ppid,
                    command: command.into(),
                },
            );
        };
        proc(100, 1, "zsh");
        proc(200, 1, "zsh");
        proc(300, 100, "bin/dev");
        proc(301, 300, "puma");
        proc(400, 1, "overmind"); // re-parented daemon
        proc(
            500,
            1,
            "node /home/u/projects/worktrees/acme-api/feature-x/dist/server.js",
        );
        proc(600, 1, "node /home/u/projects/acme-api-old/x.js");
        proc(700, 200, "python3 -m http.server");
        s.cwds.insert(
            400,
            "/home/u/projects/worktrees/acme-api/feature-x/sub".into(),
        );
        s.cwds.insert(500, "/tmp".into());
        s.cwds.insert(600, "/tmp".into());
        s.cwds.insert(700, "/home/u".into());
        s
    }

    #[test]
    fn pane_ancestry_beats_everything() {
        let hit = attribute(301, &snapshot(), &topology()).unwrap();
        assert_eq!(hit.workspace_id, "w8");
        assert_eq!(
            hit.attribution,
            Attribution::Pane {
                pane_id: "w8:p1".into()
            }
        );
    }

    #[test]
    fn ancestry_wins_even_when_cwd_is_excluded() {
        let hit = attribute(700, &snapshot(), &topology()).unwrap();
        assert_eq!(hit.workspace_id, "wG");
    }

    #[test]
    fn cwd_picks_deepest_root() {
        let hit = attribute(400, &snapshot(), &topology()).unwrap();
        assert_eq!(
            hit.workspace_id, "wN",
            "worktree path is deeper than repo path"
        );
        assert_eq!(hit.attribution, Attribution::Cwd);
    }

    #[test]
    fn command_line_match_requires_path_boundary() {
        let hit = attribute(500, &snapshot(), &topology()).unwrap();
        assert_eq!(
            (hit.workspace_id.as_str(), hit.attribution),
            ("wN", Attribution::Command)
        );
        assert!(
            attribute(600, &snapshot(), &topology()).is_none(),
            "acme-api-old is not acme-api"
        );
    }

    #[test]
    fn home_directory_pane_does_not_own_stray_processes() {
        let mut s = snapshot();
        s.processes.insert(
            800,
            ProcessInfo {
                ppid: 1,
                command: "redis-server".into(),
            },
        );
        s.cwds.insert(800, "/home/u/other".into());
        assert!(attribute(800, &s, &topology()).is_none());
    }

    #[test]
    fn roots_are_deepest_first_and_exclude_home() {
        let topology = topology();
        let roots = topology.roots();
        let paths: Vec<_> = roots
            .iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/home/u/projects/worktrees/acme-api/feature-x",
                "/home/u/projects/acme-api",
                "/home/u/projects/blog",
            ]
        );
    }

    #[test]
    fn boundary_matching() {
        assert!(path_on_boundary("node /a/b/dist/x.js", "/a/b"));
        assert!(path_on_boundary("FOO=/a/b node", "/a/b"));
        assert!(path_on_boundary("node \"/a/b/x\"", "/a/b"));
        assert!(!path_on_boundary("node /a/bc/x.js", "/a/b"));
        assert!(!path_on_boundary("node /z/a/b", "/a/b"));
    }
}
