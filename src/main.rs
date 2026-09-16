mod advertise;
mod attribute;
mod config;
mod daemon;
mod doctor;
mod herdr;
mod manual;
mod names;
mod picker;
mod process;
mod scan;
mod sidebar;
mod state;

use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::herdr::{herdr_config_path, Herdr, PluginDirs};

const PLUGIN_ID: &str = "lucidstack.herdr-services";

const USAGE: &str = "\
usage: herdr-services <command> [flags]

  daemon [--verbose]        run the detector in the foreground
  ensure-daemon [--verbose] start the detector detached if it is not running
  rescan                    ask the running daemon to scan now
  open-picker               ensure the daemon, then open the picker popup
  configure [--remove|--print|--no-reload]
                            install the managed [ui.sidebar.spaces] block
  add <label> <:PORT|URL> [--workspace <id>]
                            register a service the scanner cannot see
  remove --label <label>   drop a manually registered service
  list [--workspace <id>] [--json]
                            list known services
  doctor                    check environment, herdr, scanner and attribution
  --version";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().skip(1).any(|a| a == name);
    match args.first().map(String::as_str) {
        Some("daemon") => daemon::run(flag("--verbose")),
        Some("ensure-daemon") => ensure_daemon(flag("--verbose")),
        Some("rescan") => rescan(),
        Some("open-picker") => open_picker(flag("--verbose")),
        Some("configure") => configure(flag("--remove"), flag("--print"), !flag("--no-reload")),
        Some("add") => add(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some("list") => list(&args[1..]),
        Some("doctor") => doctor::run(),
        Some("picker") => picker::run(),
        Some("--version") => {
            println!("herdr-services {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => bail!("unknown command `{other}`\n{USAGE}"),
        None => bail!("{USAGE}"),
    }
}

fn session() -> Result<(Herdr, PluginDirs)> {
    let herdr = Herdr::from_env(Duration::from_secs(4))?;
    let dirs = PluginDirs::from_env(&herdr.socket)?;
    Ok((herdr, dirs))
}

fn ensure_daemon(verbose: bool) -> Result<()> {
    let (_, dirs) = session()?;
    let removed = daemon::gc_sessions(&dirs).unwrap_or(0);
    let pid = daemon::ensure_daemon(&dirs, verbose)?;
    println!(
        "herdr-services daemon pid {pid}{}",
        if removed > 0 {
            format!(" (removed {removed} stale session dir(s))")
        } else {
            String::new()
        }
    );
    Ok(())
}

fn rescan() -> Result<()> {
    let (_, dirs) = session()?;
    daemon::ensure_daemon(&dirs, false)?;
    let reply = daemon::control(
        &dirs.control_socket(),
        &daemon::Request::Rescan,
        Duration::from_secs(30),
    )?;
    if !reply.ok {
        bail!("rescan failed: {}", reply.error.unwrap_or_default());
    }
    println!("rescanned");
    Ok(())
}

/// `--name value` anywhere in `args`; the value is consumed so it is never
/// mistaken for a positional argument.
fn value_flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn add(args: &[String]) -> Result<()> {
    let workspace = value_flag(args, "--workspace");
    let positional: Vec<&String> = {
        let mut skip_next = false;
        args.iter()
            .filter(|a| {
                if skip_next {
                    skip_next = false;
                    return false;
                }
                if a.as_str() == "--workspace" {
                    skip_next = true;
                    return false;
                }
                true
            })
            .collect()
    };
    if positional.len() != 2 {
        bail!("usage: herdr-services add <label> <:PORT|URL> [--workspace <id>]");
    }
    let label = positional[0].clone();
    let target = positional[1];
    let (port, url) = manual::parse_target(target)?;
    let workspace_id = match workspace {
        Some(id) => id,
        None => std::env::var("HERDR_WORKSPACE_ID")
            .context("no --workspace given and HERDR_WORKSPACE_ID is not set")?,
    };
    let (_, dirs) = session()?;
    daemon::ensure_daemon(&dirs, false)?;
    let reply = daemon::control(
        &dirs.control_socket(),
        &daemon::Request::AddManual {
            workspace_id,
            label: label.clone(),
            port,
            url,
        },
        Duration::from_secs(10),
    )?;
    if !reply.ok {
        bail!("add failed: {}", reply.error.unwrap_or_default());
    }
    println!("added {label} on port {port}");
    Ok(())
}

fn remove(args: &[String]) -> Result<()> {
    let label = value_flag(args, "--label")
        .ok_or_else(|| anyhow::anyhow!("usage: herdr-services remove --label <label>"))?;
    let (_, dirs) = session()?;
    daemon::ensure_daemon(&dirs, false)?;
    let reply = daemon::control(
        &dirs.control_socket(),
        &daemon::Request::RemoveManual {
            label: label.clone(),
        },
        Duration::from_secs(10),
    )?;
    if !reply.ok {
        bail!("remove failed: {}", reply.error.unwrap_or_default());
    }
    println!("removed {label}");
    Ok(())
}

fn list(args: &[String]) -> Result<()> {
    let workspace = value_flag(args, "--workspace");
    let json = args.iter().any(|a| a == "--json");
    let (_, dirs) = session()?;
    daemon::ensure_daemon(&dirs, false)?;
    let state = state::State::read(&dirs.state_file())?;
    let services: Vec<&state::Service> = match &workspace {
        Some(ws) => state.services_for(ws).collect(),
        None => state.services.iter().collect(),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&services)?);
        return Ok(());
    }
    if services.is_empty() {
        println!("no services");
        return Ok(());
    }
    for s in services {
        let name = s.label.as_deref().unwrap_or(&s.process_name);
        println!(
            "{:<10} {:<6} {:<20} {}",
            s.workspace_id, s.port, name, s.url
        );
    }
    Ok(())
}

fn open_picker(verbose: bool) -> Result<()> {
    let (herdr, dirs) = session()?;
    daemon::ensure_daemon(&dirs, verbose)?;
    herdr.open_plugin_pane(PLUGIN_ID, "picker")
}

fn configure(remove: bool, print: bool, reload: bool) -> Result<()> {
    let config = match session() {
        Ok((_, dirs)) => Config::load(&dirs.config_file())?,
        Err(_) => Config::default(),
    };
    let max_rows = usize::from(config.sidebar.max_rows);
    if print {
        print!("{}", sidebar::managed_block(max_rows));
        return Ok(());
    }
    let path = herdr_config_path()?;
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let next = if remove {
        let next = sidebar::remove_block(&text);
        if next == text {
            println!("no herdr-services block in {}", path.display());
            return Ok(());
        }
        next
    } else {
        match sidebar::install_block(&text, max_rows) {
            Ok(next) => next,
            Err(err) => {
                eprintln!("herdr-services: {err}");
                eprintln!("Add these rows to your existing [ui.sidebar.spaces] table instead:\n");
                eprint!("{}", sidebar::rows_snippet(max_rows));
                std::process::exit(2);
            }
        }
    };
    sidebar::write_config_atomic(&path, &next)?;
    println!(
        "{} herdr-services block in {}",
        if remove { "removed" } else { "installed" },
        path.display()
    );
    if reload {
        let (herdr, _) = session()?;
        herdr
            .reload_config()
            .context("herdr server reload-config")?;
        println!("herdr config reloaded");
    } else {
        println!("run `herdr server reload-config` to apply");
    }
    Ok(())
}
