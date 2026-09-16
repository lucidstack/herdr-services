# herdr-services — specification (v0.1 draft)

A herdr plugin that automatically discovers the dev servers and listening ports
running in each herdr workspace (or git worktree) and lets you act on them from
a popup picker: open in the browser, copy the URL, kill the process. No changes
to herdr core are required; everything uses the plugin v1 surface (manifest
actions, popup panes, startup hooks, the CLI and the socket API).

## 0. Decisions (2026-09-15)

- **URL sniffing is Milestone 3, not a dependency.** `pane.output_matched`
  has no wildcard `pane_id` (herdr `src/api/subscriptions.rs:161-204` probes
  one concrete pane and rejects unknown ids), so it needs one subscription per
  pane plus resubscription on `pane.created`/`pane.closed`. v0.1 ships with
  the listener scan alone and infers `http://localhost:PORT`; the picker and
  glance line work without sniffing. When Milestone 3 lands, do the per-pane
  fan-out rather than waiting for a core wildcard.
- **Detached daemon, herdr-nvim style.** `ensure-daemon` spawns the detector
  from `[[startup]]` and from the picker action (self-healing). No upstream
  `[[daemons]]` proposal for now; revisit only if the detached pattern proves
  flaky in practice.
- **Workspace paths come from panes, not workspaces.** `workspace list` only
  carries a path for worktree workspaces (`worktree.checkout_path`); plain
  workspaces have none. The attribution root set is therefore
  `checkout_path` ∪ every pane's `cwd`/`foreground_cwd`, minus `$HOME` and `/`
  (a shell parked in the home directory must not swallow every listener).
  Deepest match wins, so a worktree under `~/projects/worktrees/repo/x` beats
  a pane in `~/projects/repo`.
- **Pane shell PIDs via `pane process-info --pane <id>`** (`shell_pid`); the
  parent chain comes from one `ps -axo pid=,ppid=,command=` on macOS and
  `/proc/<pid>/stat` on Linux. This also supplies argv, so `ps -o command=`
  per PID is unnecessary. cwd stays a single batched `lsof -a -p a,b,c -d cwd -Fn`.
- **herdr CLI already prints JSON** (`{"id":…,"result":…}`); there is no
  `--json` flag. Every CLI call runs under a timeout (default 4 s) and the
  child is killed on expiry.
- **Daemon liveness against herdr**: socket `ping` (NDJSON, returns
  `{"type":"pong","protocol":22,…}`) once per scan; three consecutive failures
  or a vanished socket file → clean exit, pidfile removed.
- **Daemon control**: Unix socket `$HERDR_PLUGIN_STATE_DIR/daemon.sock`,
  NDJSON `{"method":"rescan"}` / `{"method":"kill","pid":N,"signal":"term"|"kill"}`
  / `{"method":"ping"}`. Pidfile `daemon.pid`; `ensure-daemon` treats a pidfile
  whose PID does not answer on the control socket as stale.
- **Detection filter defaults**: ignore ports < 1024, deny
  `rapportd`, `sharingd`, `ControlCenter`, `com.docker.backend`, and
  herdr's own PID tree. Observed on the dev machine: `rapportd` and
  `ControlCenter` hold many ephemeral wildcard listeners; without the deny list
  they dominate the unattributed set.

Grilling session, 2026-09-15 (all accepted):

- **G1** Managed sidebar block (`configure`) ships in Milestone 1; picker moves to Milestone 2.
- **G2** `configure` refuses when a foreign `[ui.sidebar.spaces]` exists (radar included) and prints the snippet; `configure --print` emits it standalone.
- **G3** Row format `{glyph} {name}:{port}`; label replaces name. Cap `[sidebar] max_rows` (default 8).
- **G4** No one-line `services` summary token.
- **G5** Sidebar rows ascending by port; picker newest-first.
- **G6** Glyphs `●` up / `◌` down / `○` unprobed; colours via `starts_with` rules.
- **G7** Rows are re-reported every scan (the TTL must be refreshed — a change-only report let unchanged rows expire after 6 intervals, found in live testing); rows above the current count are cleared explicitly with `--clear-token`. TTL remains the dead-daemon net.
- **G8** `configure` reloads the server itself (`herdr server reload-config`); `--no-reload` opt-out.
- **G9** Kill is daemon-mediated over `daemon.sock`; request carries `(workspace, port, pid, signal)` and the daemon re-checks the triple against its last scan. Picker hides kill when the daemon is unreachable.
- **G10** Plugin state dir is per plugin, not per session (`src/plugin_paths.rs:21-24`); everything lives under `state/<hash(HERDR_SOCKET_PATH)>/`. `ensure-daemon` removes session dirs whose socket is gone.
- **G11** Rescan on timer plus debounced (2 s) on `pane.created`/`pane.exited`/`workspace.closed` over a persistent `events.subscribe` connection; that connection dropping is the herdr-gone signal (replaces per-loop `ping`).
- **G12** (revised after live test) `[scan] hide_ephemeral` (ports ≥ 49152) applies only to cwd/command-line attributions; a process descending from a pane shell is shown on any port — Rails worktrees here bind `tcp://0.0.0.0:0` and landed on 53512/55701. Pane-attributed agent tooling is denied by name instead: default `ignore_processes` adds `omp`, `claude`, `Google Chrome for Testing`. `[scan] ignore_commands` regex list, default empty.
- **G13** `[[build]]` only compiles; `config.toml` is touched solely by the explicit `configure` action.
- **G14** (found on first live link) Unix socket paths are capped at ~104 bytes and the per-session state dir already exceeds that. The control socket lives in `$XDG_RUNTIME_DIR/herdr-services/<key>.sock`, else `<temp>/herdr-services-<uid>/<key>.sock` (dir 0700); files stay in the state dir.
- **G15** (live test) Docker compose stacks are in scope for v0.1 via `docker ps` + compose labels (§3.2b); the user's worktree ran four containers on 32768–32772 that nothing else could see.
- **G16** (Milestone 3, live test) `events.subscribe` streams take over their connection (`stream_subscriptions` in herdr's server), so the per-pane `pane.output_matched` fan-out is a second, independent event connection, resubscribed with the current pane list whenever it changes (piggybacked on the existing scan cycle, not a new topology-event path). Advertised URLs live in an in-memory `(workspace, port) → url` registry, ranked by scheme + hostname (https/custom beats http/loopback); a service adopts one only once the listener scan confirms a process on that port, and the registry entry is dropped — not just the service's `Source` — the instant that port's PID changes, so a stale URL from before a restart can never resurface. The regex's optional path capture excludes `)`, `]`, `>`, `,`: log lines commonly wrap the URL (`Serving HTTP on 127.0.0.1 port 52725 (http://127.0.0.1:52725/) ...`) and a greedy path swallowed the closing paren in the first live test.
- **G17** (Milestone 4, live test) `add`/`remove` go through the control socket like `kill`/`rescan` — they mutate `Daemon`'s in-memory `State` directly and trigger a scan, rather than writing `state.json` from the short-lived CLI process, which would race the daemon's own next write. Re-adding an existing label moves it; a manual label attached to an already-detected `(workspace, port)` stays on that service (source unchanged) so it still disappears with the process, while a label with no backing listener creates a standalone `Source::Manual` row that persists until `remove` deletes it outright. `list` reads `state.json` directly (like the picker) rather than round-tripping through the daemon.

Status: Milestones 1–4 complete and verified live (2026-09-16); GitHub topic
`herdr-plugin` set and a release workflow builds/publishes binaries on tag
push. Milestone 5 (Windows, remote workspaces, `[[daemons]]` upstream
proposal) is next, unscoped. This document is the design to build from. It
is written in British English; keep it that way.

## 1. Why a plugin

- herdr's plugin doc is explicit: "Herdr owns the host surface … the plugin
  owns its implementation." Port discovery is heuristic, OS-specific and
  churn-prone (port whitelists, container filters, `lsof` quirks). It should
  live outside core.
- The plugin v1 surface already provides everything except an *ambient*
  sidebar tree: server-side regex on pane output (`pane.output_matched`),
  per-pane process/cwd info, a modal popup terminal for the picker, workspace
  context for every invocation, and full CLI/socket access.
- A first-class core implementation was prototyped and shelved on the fork
  branch `lucidstack/herdr@feature/services` (registry, sidebar chip,
  liveness probe, protocol bump). See `docs/research/herdr-core-services-prd.md`.
  Do not depend on it: the plugin must work against stock herdr ≥ 0.9.0.

## 2. User experience

### 2.1 Picker (primary surface)

A user-bound key (e.g. `prefix+§`) opens a popup listing the services of the **current
workspace** (the workspace of the focused pane), newest-listener first:

```
 services · acme-api                                        5 listening
 ─────────────────────────────────────────────────────────────────────────
 ● puma         :3000   http://localhost:3000                           2m
 ● vite    :4000   http://localhost:4000                           2m
 ● postgres     :5433   http://localhost:5433   ⧉ acme-api-postgres-1  1h
 ─────────────────────────────────────────────────────────────────────────
 ⏎ open   y copy url   x kill   X kill -9   r rescan   a all   / filter   q close
```

Keys (inside the popup herdr forwards every key, including Escape, so these are
plugin-defined; no herdr prefix is involved). Letters are commands, so
filtering is a mode entered with `/` (as in `less`/vim) rather than bare typing:

| Key | Action |
|---|---|
| `↑/↓`, `j/k`, `ctrl-n/p` | move |
| `/` … `⏎` | filter by port, name, label, url, workspace id; `⏎` keeps the filter, `esc` clears it (a second `esc` closes) |
| `⏎` | open URL in the system browser (`open` / `xdg-open`) |
| `y` | copy URL: `pbcopy`/`wl-copy`/`xclip`/`xsel` when present, else OSC 52 through the popup terminal |
| `x` | stop the service (SIGTERM, or `docker stop` for containers); `y/n` confirmation on first use per session (`[picker] confirm_kill`) |
| `X` | SIGKILL / `docker kill` |
| `r` | rescan now (via the daemon) |
| `a` | toggle: this workspace ⇄ all workspaces, grouped by workspace label with unattributed listeners under **other** (open/copy only, never kill) |
| `esc`, `q`, `ctrl-c` | close |
| `l` logs | deferred to v0.2 |

Rows show: liveness glyph (`●` green = accepted TCP connect, `◌` red = listener
present but connect refused, `○` grey = not probed), display name (argv-derived:
`puma` not `ruby`, compose service for containers), port, URL, optional label,
`⧉ container-name` for containers, age since first seen. The popup polls
`state.json` every 250 ms so rows update while it is open.

### 2.2 Sidebar rows (secondary, ambient)

Revised 2026-09-15 after reading `hhdebb/herdr-radar`. The plugin surface is
still `workspace report-metadata` tokens rendered by `[ui.sidebar.spaces]`
rows, but it is richer than a single glance line:

- A workspace may carry up to 32 tokens; one report may set up to 16; token
  keys ≤ 32 chars `[A-Za-z0-9_-]`, values ≤ 80 chars, control characters
  stripped, no newlines (`src/app/api_helpers.rs:202-274`).
- `[ui.sidebar.spaces] rows` accepts up to 16 rows × 16 tokens
  (`src/config/sidebar.rs:11-12`). Each cell may be styled
  `{ token = "$x", fg, bold, dim, rules = [...] }` with up to 16 value rules
  (`equals`, `starts_with`, `contains`, `gt`, `lt`, `ignore_case`, `hide`,
  first match wins). A cell whose token is absent or hidden is dropped and an
  **empty row collapses** (`src/ui/sidebar/tokens.rs:342`), so N fixed rows
  behave like a variable-length list.
- Plugins cannot draw a tree, attach click actions to rows, or reorder spaces.
  (`agent.view.set` exists for the *agents* panel only.)
- Radar installs its `[ui.sidebar.*]` tables as a marker-delimited managed
  block in the user's `config.toml` from a `configure` action, refuses when a
  foreign copy of those tables exists, and calls `herdr server reload-config`
  (`lib/managed-config.js`, `bin/configure.js`). We copy that.

**Design (grilled 2026-09-15, decisions G1–G13 in §0).** After every scan the
daemon reports, per workspace whose rows changed, in one call:

```bash
$HERDR_BIN_PATH workspace report-metadata <ws> --source herdr-services \
  --token svc_1="● puma:3000" --token svc_2="● node:5173" --token svc_3="◌ redis-server:6379" \
  --clear-token svc_4 --clear-token svc_5 \
  --ttl-ms 90000
```

- Row format `{glyph} {name}:{port}`; a manual label replaces the process
  name (`● Storybook:6006`). No URL or age in the sidebar; the picker has those.
- Rows are ordered by **ascending port** (stable under restarts); the picker
  stays newest-first.
- Cap `[sidebar] max_rows` (default 8, 1–16); overflow collapses into the last
  row as `… +N more`. Previously reported rows above the current count are
  cleared explicitly; the TTL (6 × interval) only covers a dead daemon.
- Glyphs: `●` up, `◌` listener present but connect refused, `○` not probed /
  manual without listener. Single-width Unicode, no icon font. Colour comes
  from `starts_with` rules in the sidebar block, not from the token value.
- There is no one-line summary token; users who lay out their own sidebar
  reference `$svc_1..$svc_N` directly.

`herdr-services configure` (manifest action, Milestone 1) writes the managed
block between marker comments and runs `herdr server reload-config`
(`--no-reload` to skip). `configure --remove` drops the block. `configure
--print` emits only the rows snippet. If the user — or another plugin such as
radar — already owns `[ui.sidebar.spaces]`, `configure` **refuses** and prints
the snippet to paste; it never edits a table it did not write. `[[build]]`
only compiles; nothing touches `config.toml` without the explicit action.

```toml
# herdr-services:start
[ui.sidebar.spaces]
row_gap = 0
rows = [
  ["state_icon", "workspace"],
  ["branch", "git_status"],
  [{ token = "$svc_1", dim = true, rules = [{ starts_with = "●", dim = false, fg = "#98c379" }, { starts_with = "◌", dim = false, fg = "#e06c75" }] }],
  # … svc_2 … svc_8, same cell
]
# herdr-services:end
```

### 2.3 Manual registration

For servers the heuristics cannot see (remote tunnels, docker-compose on a
different host, a URL you simply want pinned):

```bash
herdr-services add "Rails server" :3000        # attributes to $HERDR_WORKSPACE_ID
herdr-services add "Storybook" http://localhost:6006 --workspace w3
herdr-services remove --label "Storybook"
herdr-services list [--workspace <id>] [--json]
```

Manual entries are upserted by label, persist across restarts, and are merged
with detected listeners by `(workspace, port)`; a manual label wins over an
inferred one.

## 3. Detection

Three layers, modelled on Orca (`stablyai/orca`, `src/main/ports/*`) but with
herdr's advantage that every pane's PID and cwd are known.

### 3.1 URL advertisements from pane output (cheap, real-time)

Subscribe once, server-side, to every pane:

```json
{"id":"urls","method":"events.subscribe","params":{"subscriptions":[
  {"type":"pane.output_matched","pane_id":"*", "source":"recent",
   "match":{"regex":"https?://(localhost|127\\.0\\.0\\.1|0\\.0\\.0\\.0|\\[::1\\]|[a-z0-9.-]+\\.(local|test|localhost)):[0-9]{2,5}(/[^\\s]*)?"},
   "strip_ansi":true}]}}
```

(Confirm whether `pane_id` accepts a wildcard; if not, subscribe per pane and
resubscribe on `pane.created`/`pane.closed`.)

Each match gives `pane_id` + `matched_line`. Resolve the pane's workspace via
`pane get` (cache it). Record `{workspace, port, url, pane_id, first_seen}`
as an *advertised URL*. Prefer https over http and a custom hostname over
loopback when several URLs name the same port (Orca's ranking).

An advertised URL alone is a hint, not a service: a printed URL might be a
docs link. It becomes a service only when layer 3.2 sees a listener on that
port, or immediately if a connect probe succeeds. This mirrors Orca's rule that
the port scan is the primary false-positive filter.

### 3.2 Listener scan (authoritative, periodic)

Every 15 s (configurable; 30 s in Orca) enumerate TCP listeners with PID:

- macOS: `lsof -nP -iTCP -sTCP:LISTEN -F pcn` (parse `p`, `c`, `n` records);
  cwd via `lsof -a -p <pids> -d cwd -Fn`; argv via `ps -o command= -p <pid>`.
  Fall back to `proc_pidinfo`/`proc_pidfdinfo` via `libproc` later if `lsof`
  proves too slow (it can take >1 s on busy machines; Orca skips metadata when
  spawn exceeds 2 s and backs off exponentially on timeouts).
- Linux: parse `/proc/net/tcp` and `/proc/net/tcp6` (state `0A`), map socket
  inodes to PIDs through `/proc/<pid>/fd/*` → `socket:[inode]`; cwd via
  `readlink /proc/<pid>/cwd`; argv via `/proc/<pid>/cmdline`.
- Windows: `netstat -ano -p tcp` (LISTENING rows) + `Get-CimInstance
  Win32_Process` for command lines; v0.2.

Run the scan in one worker thread; never more than one in flight; per-command
timeout 4 s; exponential backoff on repeated timeouts (60 s → 5 min).

### 3.2b Container ports (`docker ps`)

Added in Milestone 1 after live testing (G15). Docker Desktop publishes
container ports through `com.docker.backend`, which has no cwd or ancestry link
to any workspace, so the listener scan alone cannot attribute them. Instead,
every scan also runs `docker ps --no-trunc --format '{{json .}}'` (same 4 s
timeout; a missing or stopped docker yields nothing and is logged once) and
parses each row's `Ports` (`0.0.0.0:32771->5432/tcp`; IPv4/IPv6 bindings of one
host port collapse) and compose labels:

- `com.docker.compose.project.working_dir` → attribution by the ordinary
  deepest-root rule (`Attribution::Container`);
- `com.docker.compose.service` → row name (`● postgres:32771`), falling back to
  the container name for plain `docker run` containers.

Container rows carry `container = <name>` and no PID; kill runs `docker stop`
(`docker kill` for SIGKILL) after checking the name still matches. A host
process and a container on the same `(workspace, port)` collapse to the host
process. Published ports are user intent, so the ephemeral-port filter does not
apply to them; the process-name deny list does (by service name).

### 3.3 Attribution (which workspace owns a listener)

In order, first hit wins:

1. **Pane ancestry (exact).** For each herdr pane, `pane process-info` /
   `pane get` yields the shell PID. Walk the listener's parent chain
   (`ps -o ppid=` / `/proc/<pid>/stat`) until it reaches a pane shell PID →
   that pane's workspace. This is the herdr-specific advantage over Orca.
2. **cwd under workspace path.** The listener's cwd equals or is inside a
   workspace's `new_workspace_cwd`/worktree checkout path (deepest match).
   Catches daemons re-parented to launchd/systemd (`overmind`, `docker
   compose` proxies, detached `bin/dev`).
3. **Workspace path on the command line**, on a word boundary (Orca's
   `includesPathBoundary`). Catches `node /repo/dist/server.js` started from
   elsewhere.
4. Otherwise the listener is *unattributed*: shown only in the "all
   workspaces" view under "other", never in the glance line.

Filters: ignore herdr's own sockets, ignore listeners whose process is in the
deny list (default: `rapportd`, `ControlCenter`, `sharingd`, `Docker` backend
`com.docker.backend`, IDE helpers), ignore ports < 1024 unless advertised.
Users can extend allow/deny lists in config.

### 3.4 Merge and liveness

State per `(workspace, port)`:

```
Service {
  workspace_id, port, host_hint,
  pid, process_name, argv_summary, cwd,
  url: Option<Url>,          // advertised (3.1) or manual, else inferred http://localhost:PORT
  label: Option<String>,     // manual only
  source: Detected | Advertised | Manual,
  attribution: Pane(pane_id) | Cwd | Command | Manual,
  first_seen, last_seen, liveness: Unknown | Up | Down
}
```

- A detected service disappears after it is absent from two consecutive scans
  (grace for restarts). Manual services never disappear but show `○` when no
  listener is present.
- Liveness: after each scan, `TcpStream::connect_timeout(500 ms)` to each
  attributed port. Cheap because the set is small and already filtered.
- The glance token is re-reported after every scan (TTL = 6 × interval).

### 3.5 What is deliberately out of scope for v0.1

Remote/SSH workspaces (herdr `--machine` targets; would need the detector to
run on the remote), UDP, Unix sockets, Windows. Docker was on this list until
live testing (G15): a worktree's whole compose stack was invisible.

## 4. Architecture

Single Rust binary `herdr-services` (like `herdr-nvim`: crossterm TUI, serde,
toml; prebuilt release binaries fetched by the build hook, cargo fallback).

Subcommands:

| Command | Role |
|---|---|
| `herdr-services daemon` | long-running detector: event subscription (3.1) + scan loop (3.2–3.4) + glance token reporting; writes `state.json` atomically under `HERDR_PLUGIN_STATE_DIR`; pidfile; exits when the herdr socket goes away. |
| `herdr-services picker` | popup TUI (2.1); reads `state.json`, watches it for changes, sends `rescan`/`kill` requests to the daemon over a local socket or by signal + file. |
| `herdr-services ensure-daemon` | startup hook: spawn `daemon` detached if no live pidfile. Also invoked by the picker action so the daemon self-heals. |
| `herdr-services add/remove/list` | manual registration (2.3). |
| `herdr-services doctor` | prints herdr socket, plugin env, `lsof` availability and timing, scan sample, attribution for each listener. |

Plugin v1 has no supervised daemon type; `ensure-daemon` from `[[startup]]`
follows herdr-nvim's daemon pattern. Propose a `[[daemons]]` manifest entry to
herdr upstream separately; do not block on it.

### 4.1 Manifest (draft)

```toml
id = "lucidstack.herdr-services"
name = "Services"
version = "0.1.0"
min_herdr_version = "0.9.0"
description = "See and act on the dev servers and ports running in each workspace"
platforms = ["macos", "linux"]

[[build]]
command = ["bash", "herdr/install.sh"]

[[startup]]
command = ["bin/herdr-services", "ensure-daemon"]

[[actions]]
id = "pick"
title = "Services: open picker"
contexts = ["workspace", "pane"]
command = ["bin/herdr-services", "open-picker"]   # ensure-daemon, then `plugin pane open --entrypoint picker`

[[actions]]
id = "rescan"
title = "Services: rescan now"
contexts = ["workspace"]
command = ["bin/herdr-services", "rescan"]

[[actions]]
id = "configure"
title = "Services: install sidebar rows"
contexts = ["workspace", "pane"]
command = ["bin/herdr-services", "configure"]

[[panes]]
id = "picker"
title = "services"
placement = "popup"
width = "80%"
height = 16
command = ["bin/herdr-services", "picker"]
```

User keybinding:

```toml
[[keys.command]]
key = "prefix+§"              # not prefix+s: herdr's `settings` default wins and disables the binding
type = "plugin_action"
command = "lucidstack.herdr-services.pick"
description = "services"
```

### 4.2 herdr APIs used

- `events.subscribe` — `pane.output_matched` (URL sniffing), `pane.created`,
  `pane.closed`, `workspace.created`, `workspace.closed`, `worktree.*`.
- `workspace list`, `pane list`, `pane get`, `pane process-info` — workspace
  paths, pane shell PIDs, cwd.
- `workspace report-metadata --token services=… --ttl-ms` — glance line.
- `plugin pane open --entrypoint picker` — picker popup.
- `pane split` / `pane run` — optional "logs" action.
- Env: `HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`, `HERDR_WORKSPACE_ID`,
  `HERDR_PANE_ID`, `HERDR_PLUGIN_STATE_DIR`, `HERDR_PLUGIN_CONFIG_DIR`,
  `HERDR_PLUGIN_CONTEXT_JSON`.

### 4.3 Config (`$HERDR_PLUGIN_CONFIG_DIR/config.toml`)

```toml
[scan]
interval_seconds = 15
command_timeout_ms = 4000
min_port = 1024
ignore_processes = ["rapportd", "sharingd", "ControlCenter", "com.docker.backend",
                    "omp", "claude", "Google Chrome for Testing"]
allow_processes = []          # if non-empty, only these
hide_ephemeral = true         # ports >= 49152, except processes started from a pane
ignore_commands = []          # regexes matched against the command line

[docker]
enabled = true                # include ports published by running containers

[urls]
default_scheme = "http"
https_ports = [443, 8443]
prefer_hostname = true        # custom hostname over localhost when both advertised

[sidebar]
enabled = true
max_rows = 8                  # 1–16 svc_N tokens per workspace

[picker]
confirm_kill = true
```

## 5. Testing

- Unit: parsers for `lsof -F`, `/proc/net/tcp`, `ps`; attribution ordering
  (pane ancestry beats cwd beats command line; deepest path wins); merge rules
  (manual label wins; two-scan disappearance grace; advertised URL invalidated
  when PID changes); URL ranking; glance token formatting; config parsing.
- Integration (needs a herdr binary): start a disposable named herdr session,
  create a workspace, run `python3 -m http.server 0` in a pane, assert the
  daemon attributes the port to that workspace via pane ancestry, that the
  advertised URL printed by `http.server` is captured, and that the picker
  renders the row; kill via the picker and assert the row disappears after two
  scans. Reuse herdr's `herdr-throwaway-repro` recipe (`--session`, cleared
  `HERDR_*` env).
- `doctor` doubles as a manual smoke tool.

## 6. Milestones

1. **Skeleton**: manifest, build hook, `doctor`, `daemon` with scan loop
   (macOS `lsof` + Linux `/proc`), event-triggered rescan, attribution by
   pane ancestry + cwd, `state.json`, `svc_N` row tokens, `configure` action
   for the managed sidebar block. Verify with `doctor` inside a real session.
2. **Picker**: popup TUI, open/copy/kill/rescan, all-workspaces view. ✔ 2026-09-16
3. **Advertised URLs**: `pane.output_matched` subscription, merge, ranking. ✔ 2026-09-16
4. **Manual registration** and persistence, docs, release binaries and
   marketplace topic. ✔ 2026-09-16
5. Later: Windows, remote workspaces, `[[daemons]]` upstream proposal.

## 7. References

- herdr plugin docs: `docs/next/website/src/content/docs/plugins.mdx`,
  `socket-api.mdx`, `cli-reference.mdx` in the herdr repo.
- Reference plugin with popup picker + detached daemon: `ChmaraX/herdr-nvim`
  (`herdr-plugin.toml`, `src/picker.rs`, `src/daemon.rs`, `herdr/install.sh`).
- Orca port detection: `stablyai/orca` `src/main/ports/` —
  `local-workspace-platform-port-scanner.ts`, `local-workspace-port-attribution.ts`,
  `advertised-url-watcher.ts`, `advertised-url-parsing.ts`.
- Shelved core prototype: `lucidstack/herdr@feature/services`;
  design notes in `docs/research/herdr-core-services-prd.md`.
