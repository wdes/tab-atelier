//! Parsing of the `taremote://onboard?url=...&token=...` deep link.

/// Parsed contents of a `taremote://onboard?…` deep link. `url` + `token` are
/// required; `cf_access_client_id` / `cf_access_client_secret` are the optional
/// Cloudflare Access service-token pair (query params `cf_id` / `cf_secret`),
/// empty when the link doesn't carry one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Onboard {
    pub url: String,
    pub token: String,
    pub cf_access_client_id: String,
    pub cf_access_client_secret: String,
}

#[must_use]
pub fn parse_onboard_url(url: &str) -> Option<Onboard> {
    let q = url.strip_prefix("taremote://onboard?")?;
    let mut host_url = None;
    let mut token = None;
    let mut cf_id = String::new();
    let mut cf_secret = String::new();
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=')?;
        match k {
            "url" => host_url = Some(percent_decode(v)),
            "token" => token = Some(percent_decode(v)),
            "cf_id" => cf_id = percent_decode(v),
            "cf_secret" => cf_secret = percent_decode(v),
            _ => {}
        }
    }
    Some(Onboard {
        url: host_url?,
        token: token?,
        cf_access_client_id: cf_id,
        cf_access_client_secret: cf_secret,
    })
}

#[must_use]
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = ((bytes[i + 1] as char).to_digit(16), (bytes[i + 2] as char).to_digit(16))
        {
            out.push(((hi << 4) | lo) as u8);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Upgrade a headless base URL to its TLS form: `http://` → `https://`, and the
/// default API port `7890` → the TLS listener port `7891`. Other schemes/ports
/// pass through untouched. Kept here (host-compiled) so it's unit-testable —
/// `android_app` is `cfg(target_os = "android")` and never builds on the host.
#[must_use]
pub fn to_tls_url(http_url: &str) -> String {
    let mut s = http_url.to_string();
    if let Some(rest) = s.strip_prefix("http://") {
        s = format!("https://{rest}");
    }
    if let Some(idx) = s.rfind(":7890") {
        s = format!("{}:7891{}", &s[..idx], &s[idx + 5..]);
    }
    s
}

/// A host URL stripped of its scheme, for compact display (`https://h:7891` →
/// `h:7891`).
#[must_use]
pub fn host_detail(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_plain() {
        assert_eq!(percent_decode("hello"), "hello");
    }

    #[test]
    fn percent_decode_escapes() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("%2F%3A%3F"), "/:?");
    }

    #[test]
    fn percent_decode_plus_is_space() {
        assert_eq!(percent_decode("hello+world"), "hello world");
    }

    #[test]
    fn percent_decode_invalid_hex_passes_through() {
        // %ZZ is not valid hex; keep the literal bytes
        assert_eq!(percent_decode("a%ZZb"), "a%ZZb");
    }

    #[test]
    fn parse_minimal() {
        let o = parse_onboard_url("taremote://onboard?url=http://1.2.3.4:7890&token=abc").unwrap();
        assert_eq!(o.url, "http://1.2.3.4:7890");
        assert_eq!(o.token, "abc");
        // No CF service token in the link → empty pair.
        assert_eq!(o.cf_access_client_id, "");
        assert_eq!(o.cf_access_client_secret, "");
    }

    #[test]
    fn parse_url_encoded() {
        let o = parse_onboard_url("taremote://onboard?url=http%3A%2F%2F1.2.3.4%3A7890&token=deadbeef0123").unwrap();
        assert_eq!(o.url, "http://1.2.3.4:7890");
        assert_eq!(o.token, "deadbeef0123");
    }

    #[test]
    fn parse_extra_params_ignored() {
        let o = parse_onboard_url("taremote://onboard?foo=bar&url=http://x:7890&extra=baz&token=tok").unwrap();
        assert_eq!(o.url, "http://x:7890");
        assert_eq!(o.token, "tok");
    }

    #[test]
    fn parse_cf_service_token() {
        // A CF Access service token rides along as cf_id / cf_secret and is
        // percent-decoded like the rest. `.access` suffix on the id is typical.
        let o = parse_onboard_url(
            "taremote://onboard?url=https%3A%2F%2Fh%3A7891&token=tok&cf_id=abc.access&cf_secret=s%3Ae%3Ac",
        )
        .unwrap();
        assert_eq!(o.url, "https://h:7891");
        assert_eq!(o.token, "tok");
        assert_eq!(o.cf_access_client_id, "abc.access");
        assert_eq!(o.cf_access_client_secret, "s:e:c");
    }

    #[test]
    fn parse_missing_token_returns_none() {
        assert!(parse_onboard_url("taremote://onboard?url=http://x:7890").is_none());
    }

    #[test]
    fn parse_wrong_scheme_returns_none() {
        assert!(parse_onboard_url("http://example.com/?url=x&token=y").is_none());
    }

    #[test]
    fn parse_wrong_host_returns_none() {
        assert!(parse_onboard_url("taremote://other?url=x&token=y").is_none());
    }

    #[test]
    fn to_tls_url_upgrades_scheme_and_port() {
        // http → https AND the default API port 7890 → the TLS port 7891.
        assert_eq!(to_tls_url("http://192.168.1.5:7890"), "https://192.168.1.5:7891");
    }

    #[test]
    fn to_tls_url_already_https_only_bumps_port() {
        assert_eq!(to_tls_url("https://host:7890"), "https://host:7891");
    }

    #[test]
    fn to_tls_url_keeps_trailing_path_and_leaves_other_ports() {
        assert_eq!(to_tls_url("http://host:7890/tabs"), "https://host:7891/tabs");
        // A non-default port is untouched (only the scheme upgrades).
        assert_eq!(to_tls_url("http://host:8443"), "https://host:8443");
    }

    #[test]
    fn host_detail_strips_scheme() {
        assert_eq!(host_detail("https://t-atelier.example:7891"), "t-atelier.example:7891");
        assert_eq!(host_detail("http://192.168.1.5:7890"), "192.168.1.5:7890");
        // Already scheme-less → unchanged.
        assert_eq!(host_detail("bare-host:7890"), "bare-host:7890");
    }
}
