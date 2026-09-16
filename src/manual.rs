//! Manual service registration (SPEC §2.3): `:PORT` or a full URL, parsed
//! client-side so the daemon always receives a concrete port and url.

use anyhow::{anyhow, Context, Result};

/// Parse `:3000` (port only, url left for the daemon to infer) or a full
/// `scheme://host:port[/path]` URL (port taken from it, url kept verbatim).
pub fn parse_target(target: &str) -> Result<(u16, Option<String>)> {
    if let Some(rest) = target.strip_prefix(':') {
        let port: u16 = rest
            .parse()
            .with_context(|| format!("`{target}` is not `:PORT`"))?;
        return Ok((port, None));
    }
    let after_scheme = target
        .split_once("://")
        .map(|(_, rest)| rest)
        .ok_or_else(|| anyhow!("`{target}` is not `:PORT` or a `scheme://host:port` URL"))?;
    let host_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    let port_str = host_port
        .rsplit_once(':')
        .map(|(_, port)| port)
        .ok_or_else(|| anyhow!("`{target}` has no port"))?;
    let port: u16 = port_str
        .parse()
        .with_context(|| format!("`{target}` has a non-numeric port"))?;
    Ok((port, Some(target.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_port_leaves_the_url_for_the_daemon_to_infer() {
        assert_eq!(parse_target(":3000").unwrap(), (3000, None));
    }

    #[test]
    fn a_full_url_keeps_itself_and_yields_its_port() {
        assert_eq!(
            parse_target("http://localhost:6006").unwrap(),
            (6006, Some("http://localhost:6006".to_string()))
        );
        assert_eq!(
            parse_target("https://app.example.com:8443/dashboard").unwrap(),
            (
                8443,
                Some("https://app.example.com:8443/dashboard".to_string())
            )
        );
    }

    #[test]
    fn rejects_garbage_and_missing_ports() {
        assert!(parse_target("not a target").is_err());
        assert!(parse_target("http://localhost").is_err());
        assert!(parse_target(":not-a-number").is_err());
    }
}
