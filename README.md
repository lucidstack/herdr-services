# herdr-services

A [herdr](https://herdr.dev) plugin that discovers the dev servers and
listening ports running in each workspace and shows them in the sidebar, one
row per service:

```
 acme-api
 ● puma:3000
 ● node:5173
 ◌ redis-server:6379
```

`●` accepted a TCP connect, `◌` listener present but connect refused, `○` not
probed. Ports are attributed to workspaces by pane ancestry first (the process
descends from a pane's shell), then by working directory, then by a workspace
path on the command line. Nothing outside your workspaces is ever shown in the
sidebar.

Status: Milestones 1–2 (detector, sidebar rows, Docker compose ports, popup
picker, `doctor`). Next: advertised URLs from pane output, manual entries.
See `SPEC.md`.

## Install

```sh
herdr plugin install lucidstack/herdr-services
```

Then run the **Services: install sidebar rows** action (or
`herdr plugin action invoke configure --plugin lucidstack.herdr-services`). It
writes a managed `[ui.sidebar.spaces]` block into herdr's `config.toml` and
reloads the server. If you already own that table — by hand or via another
plugin such as herdr-radar — the action refuses and prints the rows to paste
instead; `bin/herdr-services configure --print` prints them any time.

Bind the picker to a key (not `prefix+s`: that is herdr's `settings` default
and herdr silently disables a custom binding that clashes with it — check
`herdr-client.log` for `config diagnostic` lines):

```toml
[[keys.command]]
key = "prefix+§"
type = "plugin_action"
command = "lucidstack.herdr-services.pick"
description = "services"
```

In the popup: `⏎` open in browser, `y` copy URL, `x` stop (`X` SIGKILL),
`r` rescan, `a` all workspaces, `/` filter, `q` close.

The detector starts with herdr (`[[startup]]`) and exits when herdr does.

## Development

```sh
just dev          # debug build, symlinked into bin/
just link         # herdr plugin link "$PWD"
just logs         # herdr plugin log list --plugin lucidstack.herdr-services
bin/herdr-services doctor
```

`doctor` prints the environment, herdr reachability, scanner timing and every
listener with its attribution — run it inside a herdr pane.

Configuration lives in `$HERDR_PLUGIN_CONFIG_DIR/config.toml`
(`herdr plugin config-dir lucidstack.herdr-services`); every key and its
default is listed in `SPEC.md` §4.3.

## Platforms

macOS (`lsof` + `ps`) and Linux (`/proc`); Docker compose stacks on either via
`docker ps` labels. Windows is not supported in v0.1.
