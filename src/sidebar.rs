//! Sidebar rows: `svc_N` token formatting, change-tracked reporting, and the
//! managed `[ui.sidebar.spaces]` block written by `configure` (SPEC §2.2).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::herdr::Herdr;
use crate::state::{Liveness, Service, Source};

pub const SOURCE: &str = "herdr-services";
pub const BLOCK_START: &str = "# herdr-services:start";
pub const BLOCK_END: &str = "# herdr-services:end";
const TOKEN_VALUE_MAX: usize = 80;

pub fn glyph(service: &Service) -> &'static str {
    let present = service.pid.is_some() || service.container.is_some();
    match (service.liveness, present) {
        (Liveness::Up, _) => "●",
        (Liveness::Down, true) => "◌",
        _ => "○",
    }
}

/// `{glyph} {name}:{port}`, label over process name, trimmed to herdr's 80-char cap.
pub fn row_text(service: &Service) -> String {
    let name = service
        .label
        .as_deref()
        .filter(|l| !l.trim().is_empty())
        .unwrap_or(service.process_name.as_str());
    let name = if name.is_empty() { "?" } else { name };
    let text = format!("{} {}:{}", glyph(service), name, service.port);
    text.chars().take(TOKEN_VALUE_MAX).collect()
}

/// Rows for one workspace: ascending port, at most `max_rows`; overflow folds
/// into the final row as `… +N more`.
pub fn rows_for(services: &[&Service], max_rows: usize) -> Vec<String> {
    let mut sorted: Vec<&Service> = services.to_vec();
    sorted.sort_by_key(|s| (s.port, s.source != Source::Manual));
    if sorted.len() <= max_rows {
        return sorted.iter().map(|s| row_text(s)).collect();
    }
    let shown = max_rows.saturating_sub(1);
    let mut rows: Vec<String> = sorted[..shown].iter().map(|s| row_text(s)).collect();
    rows.push(format!("… +{} more", sorted.len() - shown));
    rows
}

pub fn token_key(index: usize) -> String {
    format!("svc_{}", index + 1)
}

/// Remembers the last row count per workspace so shrinking lists get explicit
/// clears. Rows are re-sent on every scan: herdr expires tokens after `ttl_ms`
/// (6 × interval), so an unchanged list still needs its TTL refreshed.
#[derive(Debug, Default)]
pub struct Reporter {
    last: HashMap<String, Vec<String>>,
}

impl Reporter {
    /// Report `rows` for `workspace_id`, clearing rows above the new count.
    /// Returns whether the rows differ from the previous report.
    pub fn report(
        &mut self,
        herdr: &Herdr,
        workspace_id: &str,
        rows: Vec<String>,
        ttl_ms: u64,
    ) -> Result<bool> {
        let previous = self.last.get(workspace_id);
        let changed = previous != Some(&rows);
        let previous_len = previous.map_or(0, Vec::len);
        let set: Vec<(String, String)> = rows
            .iter()
            .enumerate()
            .map(|(i, text)| (token_key(i), text.clone()))
            .collect();
        let clear: Vec<String> = (rows.len()..previous_len).map(token_key).collect();
        herdr
            .report_metadata(workspace_id, SOURCE, &set, &clear, ttl_ms)
            .with_context(|| format!("report sidebar rows for {workspace_id}"))?;
        self.last.insert(workspace_id.to_string(), rows);
        Ok(changed)
    }

    /// Workspaces previously reported to but absent from `live`: clear their rows.
    pub fn clear_missing(&mut self, herdr: &Herdr, live: &[&str]) -> Result<()> {
        let gone: Vec<String> = self
            .last
            .keys()
            .filter(|ws| !live.contains(&ws.as_str()))
            .cloned()
            .collect();
        for ws in gone {
            let previous_len = self.last.get(&ws).map_or(0, Vec::len);
            let clear: Vec<String> = (0..previous_len).map(token_key).collect();
            // The workspace may already be closed; that is not an error worth stopping for.
            let _ = herdr.report_metadata(&ws, SOURCE, &[], &clear, 0);
            self.last.remove(&ws);
        }
        Ok(())
    }
}

/// The rows snippet (without table header) for `max_rows` services.
pub fn rows_snippet(max_rows: usize) -> String {
    let mut out = String::from(
        "rows = [\n  [\"state_icon\", \"workspace\"],\n  [\"branch\", \"git_status\"],\n",
    );
    for i in 0..max_rows {
        out.push_str(&format!(
            "  [{{ token = \"${}\", dim = true, rules = [{{ starts_with = \"●\", dim = false, fg = \"#98c379\" }}, {{ starts_with = \"◌\", dim = false, fg = \"#e06c75\" }}] }}],\n",
            token_key(i)
        ));
    }
    out.push_str("]\n");
    out
}

/// The full managed block.
pub fn managed_block(max_rows: usize) -> String {
    format!(
        "{BLOCK_START}\n[ui.sidebar.spaces]\nrow_gap = 0\n{}{BLOCK_END}\n",
        rows_snippet(max_rows)
    )
}

/// Does `text` define `[ui.sidebar.spaces]` outside our markers?
pub fn foreign_spaces_table(text: &str) -> bool {
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == BLOCK_START {
            inside = true;
        } else if trimmed == BLOCK_END {
            inside = false;
        } else if !inside && trimmed == "[ui.sidebar.spaces]" {
            return true;
        }
    }
    false
}

pub fn has_block(text: &str) -> bool {
    text.lines().any(|l| l.trim() == BLOCK_START)
}

/// Install or refresh the managed block. Errors when a foreign
/// `[ui.sidebar.spaces]` exists; the caller prints the snippet in that case.
pub fn install_block(text: &str, max_rows: usize) -> Result<String> {
    if foreign_spaces_table(text) {
        bail!("config already defines [ui.sidebar.spaces] outside the herdr-services block");
    }
    let block = managed_block(max_rows);
    if has_block(text) {
        return Ok(replace_block(text, &block));
    }
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&block);
    Ok(out)
}

/// Remove the managed block; unchanged text when absent.
pub fn remove_block(text: &str) -> String {
    if !has_block(text) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == BLOCK_START {
            inside = true;
            continue;
        }
        if trimmed == BLOCK_END {
            inside = false;
            continue;
        }
        if !inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    // Collapse the blank line that preceded the block.
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

fn replace_block(text: &str, block: &str) -> String {
    let mut out = String::with_capacity(text.len() + block.len());
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == BLOCK_START {
            inside = true;
            out.push_str(block);
            continue;
        }
        if trimmed == BLOCK_END {
            inside = false;
            continue;
        }
        if !inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

pub fn write_config_atomic(path: &Path, text: &str) -> Result<()> {
    let backup = path.with_extension("toml.herdr-services.bak");
    if path.exists() {
        std::fs::copy(path, &backup).with_context(|| format!("back up to {}", backup.display()))?;
    }
    let tmp = path.with_extension("toml.herdr-services.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribute::Attribution;

    fn svc(port: u16, name: &str, liveness: Liveness, pid: Option<u32>) -> Service {
        Service {
            workspace_id: "w1".into(),
            port,
            host_hint: "*".into(),
            pid,
            container: None,
            process_name: name.into(),
            argv_summary: String::new(),
            cwd: None,
            url: String::new(),
            label: None,
            source: Source::Detected,
            attribution: Attribution::Cwd,
            first_seen_ms: 0,
            last_seen_ms: 0,
            liveness,
            missed_scans: 0,
        }
    }

    #[test]
    fn row_text_uses_label_over_name_and_liveness_glyph() {
        let up = svc(3000, "puma", Liveness::Up, Some(1));
        assert_eq!(row_text(&up), "● puma:3000");
        let down = svc(6379, "redis-server", Liveness::Down, Some(2));
        assert_eq!(row_text(&down), "◌ redis-server:6379");
        let mut manual = svc(6006, "", Liveness::Down, None);
        manual.label = Some("Storybook".into());
        assert_eq!(row_text(&manual), "○ Storybook:6006");
    }

    #[test]
    fn rows_sort_by_port_and_fold_overflow() {
        let a = svc(5173, "node", Liveness::Up, Some(1));
        let b = svc(3000, "puma", Liveness::Up, Some(2));
        let c = svc(6379, "redis", Liveness::Up, Some(3));
        let rows = rows_for(&[&a, &b, &c], 8);
        assert_eq!(rows, vec!["● puma:3000", "● node:5173", "● redis:6379"]);
        let rows = rows_for(&[&a, &b, &c], 2);
        assert_eq!(rows, vec!["● puma:3000", "… +2 more"]);
    }

    #[test]
    fn install_appends_and_refresh_replaces_in_place() {
        let base = "[ui]\ntab_bar = true\n";
        let installed = install_block(base, 2).unwrap();
        assert!(installed.starts_with(base));
        assert!(installed.contains("$svc_2"));
        assert!(!installed.contains("$svc_3"));
        let refreshed = install_block(&installed, 3).unwrap();
        assert!(refreshed.contains("$svc_3"));
        assert_eq!(refreshed.matches(BLOCK_START).count(), 1);
        assert_eq!(remove_block(&refreshed), base);
    }

    #[test]
    fn foreign_spaces_table_is_refused() {
        let text = "[ui.sidebar.spaces]\nrows = [[\"workspace\"]]\n";
        assert!(install_block(text, 8).is_err());
        let ours = install_block("", 8).unwrap();
        assert!(!foreign_spaces_table(&ours));
    }

    #[test]
    fn remove_is_noop_without_block() {
        assert_eq!(remove_block("[ui]\n"), "[ui]\n");
    }
}
