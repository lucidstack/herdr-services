//! Friendlier process names than the kernel's `comm`.
//!
//! `lsof`/`/proc` report the executable (`ruby`, `node`, `Python`); the command
//! line usually names the actual server (`puma`, `vite`, `http.server`).

use std::path::Path;

const INTERPRETERS: &[&str] = &[
    "ruby", "python", "python3", "node", "bun", "deno", "java", "perl", "php", "sh", "bash", "zsh",
    "dotnet", "npx", "bundle",
];

/// Best display name for a listener from its `comm` and full command line.
pub fn display_name(comm: &str, command: &str) -> String {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let Some(&first) = tokens.first() else {
        return comm.to_string();
    };
    // Executable paths may contain spaces (`…/Plex Media Server`); if the
    // kernel's name appears as a path component of the command, it is argv0.
    let argv0 = if first.contains('/') && !comm.is_empty() && command.contains(&format!("/{comm}"))
    {
        comm.to_string()
    } else {
        basename(first)
    };
    if !is_interpreter(&argv0) {
        // Servers that rewrite their process title (`puma 8.0.2 (tcp://…)`) or
        // plain binaries (`redis-server`, `overmind`).
        return if argv0.is_empty() {
            comm.to_string()
        } else {
            argv0
        };
    }
    // Interpreter: the script/module/subcommand after it names the server.
    let mut iter = tokens.iter().skip(1).peekable();
    while let Some(tok) = iter.next() {
        if *tok == "-m" || *tok == "--module" {
            if let Some(module) = iter.next() {
                return module.to_string();
            }
            break;
        }
        if *tok == "exec" || *tok == "run" {
            continue;
        }
        if tok.starts_with('-') {
            continue;
        }
        let name = basename(tok);
        let name = strip_ext(&name);
        if !name.is_empty() && !is_interpreter(name) {
            return name.to_string();
        }
    }
    comm.to_string()
}

fn is_interpreter(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let trimmed = lower.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    INTERPRETERS.contains(&lower.as_str()) || INTERPRETERS.contains(&trimmed)
}

fn basename(s: &str) -> String {
    Path::new(s)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn strip_ext(s: &str) -> &str {
    match s.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && matches!(
                    ext,
                    "js" | "mjs" | "cjs" | "ts" | "rb" | "py" | "jar" | "php"
                ) =>
        {
            stem
        }
        _ => s,
    }
}

#[cfg(test)]
mod tests {
    use super::display_name;

    #[test]
    fn executable_paths_with_spaces_keep_the_kernel_name() {
        assert_eq!(
            display_name(
                "Plex Media Server",
                "/Applications/Plex Media Server.app/Contents/MacOS/Plex Media Server"
            ),
            "Plex Media Server"
        );
        assert_eq!(
            display_name(
                "Discord Helper (Renderer)",
                "/Applications/Discord.app/Contents/Frameworks/Discord Helper (Renderer).app/Contents/MacOS/Discord Helper (Renderer) --type=renderer"
            ),
            "Discord Helper (Renderer)"
        );
        // A plain path still yields its basename.
        assert_eq!(
            display_name("redis-server", "/opt/homebrew/bin/redis-server"),
            "redis-server"
        );
    }

    #[test]
    fn process_title_rewrites_win() {
        assert_eq!(
            display_name("ruby", "puma 8.0.2 (tcp://0.0.0.0:3000) [app]"),
            "puma"
        );
        assert_eq!(
            display_name(
                "redis-server",
                "/opt/homebrew/bin/redis-server 127.0.0.1:6379"
            ),
            "redis-server"
        );
    }

    #[test]
    fn interpreters_yield_their_script_or_module() {
        assert_eq!(
            display_name("Python", "python3 -m http.server 8765"),
            "http.server"
        );
        assert_eq!(
            display_name("node", "node /repo/node_modules/.bin/vite --port 5173"),
            "vite"
        );
        assert_eq!(
            display_name("node", "/usr/local/bin/node dist/server.js"),
            "server"
        );
        assert_eq!(
            display_name("ruby", "/usr/bin/ruby bin/rails server"),
            "rails"
        );
        assert_eq!(display_name("ruby", "bundle exec sidekiq"), "sidekiq");
        assert_eq!(display_name("java", "java -jar target/app.jar"), "app");
    }

    #[test]
    fn falls_back_to_comm() {
        assert_eq!(display_name("mystery", ""), "mystery");
        assert_eq!(display_name("node", "node"), "node");
        assert_eq!(display_name("python3", "python3 -u"), "python3");
    }
}
