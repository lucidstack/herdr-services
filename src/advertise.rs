//! Advertised URLs from pane output (SPEC §3.1). herdr matches this regex
//! server-side against each subscribed pane's recent output; the plugin
//! reuses the same pattern locally to pull the URL, host and port back out
//! of the matched line. An advertisement is a hint, not a service: it only
//! reaches `state.json` once the listener scan confirms a process on that
//! `(workspace, port)` (wired in `Daemon::observe`).

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

/// `http(s)://` followed by a loopback host or a `.local`/`.test`/`.localhost`
/// hostname and a port. Sent to herdr as the `pane.output_matched` filter and
/// reused locally to extract the match; capturing groups are ignored by
/// herdr's `is_match` check.
pub const URL_PATTERN: &str = r"(https?)://(localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\]|[a-z0-9.-]+\.(?:local|test|localhost)):([0-9]{2,5})(/[^\s)\]>,]*)?";

fn regex() -> &'static Regex {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(URL_PATTERN).expect("URL_PATTERN is a valid regex"));
    &RE
}

/// Pull the first advertised URL and its port out of a matched output line.
pub fn parse_url(line: &str) -> Option<(u16, String)> {
    let caps = regex().captures(line)?;
    let port: u16 = caps.get(3)?.as_str().parse().ok()?;
    Some((port, caps.get(0)?.as_str().to_string()))
}

/// Ranks a URL for the "prefer https and a custom hostname over loopback"
/// rule (SPEC §3.1, Orca's ranking): `3` beats `0`.
fn rank(url: &str) -> u8 {
    let https = url.starts_with("https://");
    let loopback = regex()
        .captures(url)
        .and_then(|c| c.get(2))
        .map(|host| {
            matches!(
                host.as_str(),
                "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]"
            )
        })
        .unwrap_or(true);
    match (https, loopback) {
        (true, false) => 3,
        (true, true) => 2,
        (false, false) => 1,
        (false, true) => 0,
    }
}

#[derive(Debug, Clone)]
struct Entry {
    url: String,
    pane_id: String,
    rank: u8,
}

/// Advertised URLs keyed by `(workspace_id, port)`. A lower-ranked match never
/// displaces a higher-ranked one already on file; entries are dropped when
/// their originating pane closes or (by the caller, via [`forget`](Self::forget))
/// when the port's process identity changes.
#[derive(Debug, Clone, Default)]
pub struct Advertisements(HashMap<(String, u16), Entry>);

impl Advertisements {
    /// Record a match; returns `true` if it changed the stored URL.
    pub fn record(
        &mut self,
        workspace_id: String,
        port: u16,
        url: String,
        pane_id: String,
    ) -> bool {
        let new_rank = rank(&url);
        let key = (workspace_id, port);
        if let Some(existing) = self.0.get(&key) {
            if existing.rank > new_rank || existing.url == url {
                return false;
            }
        }
        self.0.insert(
            key,
            Entry {
                url,
                pane_id,
                rank: new_rank,
            },
        );
        true
    }

    /// The current advertised URL for `(workspace_id, port)`, if any.
    pub fn get(&self, workspace_id: &str, port: u16) -> Option<String> {
        self.0
            .get(&(workspace_id.to_string(), port))
            .map(|e| e.url.clone())
    }

    /// Drop the advertisement for `(workspace_id, port)`: the process behind
    /// it changed identity, so the URL cannot be trusted until re-advertised.
    pub fn forget(&mut self, workspace_id: &str, port: u16) {
        self.0.remove(&(workspace_id.to_string(), port));
    }

    /// Drop advertisements whose pane no longer exists.
    pub fn retain_panes(&mut self, alive: &HashMap<String, String>) {
        self.0.retain(|_, entry| alive.contains_key(&entry.pane_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_extracts_port_and_full_url_from_a_log_line() {
        let line = "Local:   http://localhost:5173/app (ready in 120ms)";
        let (port, url) = parse_url(line).expect("match");
        assert_eq!(port, 5173);
        assert_eq!(url, "http://localhost:5173/app");
    }

    #[test]
    fn parse_url_stops_before_a_wrapping_paren() {
        let line = "Serving HTTP on 127.0.0.1 port 52725 (http://127.0.0.1:52725/) ...";
        let (port, url) = parse_url(line).expect("match");
        assert_eq!(port, 52725);
        assert_eq!(url, "http://127.0.0.1:52725/");
    }

    #[test]
    fn parse_url_ignores_lines_without_a_recognised_host() {
        assert_eq!(parse_url("see https://example.com:5173 for docs"), None);
        assert_eq!(parse_url("just some text"), None);
    }

    #[test]
    fn higher_ranked_url_beats_a_lower_ranked_one_for_the_same_port() {
        let mut ads = Advertisements::default();
        assert!(ads.record(
            "w1".into(),
            3000,
            "http://localhost:3000".into(),
            "p1".into()
        ));
        assert!(ads.record(
            "w1".into(),
            3000,
            "https://app.test:3000".into(),
            "p1".into()
        ));
        assert_eq!(
            ads.get("w1", 3000).as_deref(),
            Some("https://app.test:3000")
        );
        // A lower-ranked match never displaces it.
        assert!(!ads.record(
            "w1".into(),
            3000,
            "http://localhost:3000".into(),
            "p2".into()
        ));
        assert_eq!(
            ads.get("w1", 3000).as_deref(),
            Some("https://app.test:3000")
        );
    }

    #[test]
    fn forget_clears_a_single_port_without_touching_others() {
        let mut ads = Advertisements::default();
        ads.record(
            "w1".into(),
            3000,
            "http://localhost:3000".into(),
            "p1".into(),
        );
        ads.record(
            "w1".into(),
            4000,
            "http://localhost:4000".into(),
            "p1".into(),
        );
        ads.forget("w1", 3000);
        assert_eq!(ads.get("w1", 3000), None);
        assert_eq!(
            ads.get("w1", 4000).as_deref(),
            Some("http://localhost:4000")
        );
    }

    #[test]
    fn retain_panes_drops_advertisements_from_closed_panes() {
        let mut ads = Advertisements::default();
        ads.record(
            "w1".into(),
            3000,
            "http://localhost:3000".into(),
            "p1".into(),
        );
        let mut alive = HashMap::new();
        alive.insert("p2".to_string(), "w1".to_string());
        ads.retain_panes(&alive);
        assert_eq!(ads.get("w1", 3000), None);
    }
}
