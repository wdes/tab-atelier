//! Parsing of the `taremote://onboard?url=...&token=...` deep link.

pub fn parse_onboard_url(url: &str) -> Option<(String, String)> {
    let q = url.strip_prefix("taremote://onboard?")?;
    let mut host_url = None;
    let mut token = None;
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=')?;
        match k {
            "url" => host_url = Some(percent_decode(v)),
            "token" => token = Some(percent_decode(v)),
            _ => {}
        }
    }
    Some((host_url?, token?))
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
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
        let (u, t) = parse_onboard_url("taremote://onboard?url=http://1.2.3.4:7890&token=abc").unwrap();
        assert_eq!(u, "http://1.2.3.4:7890");
        assert_eq!(t, "abc");
    }

    #[test]
    fn parse_url_encoded() {
        let (u, t) = parse_onboard_url(
            "taremote://onboard?url=http%3A%2F%2F1.2.3.4%3A7890&token=deadbeef0123",
        )
        .unwrap();
        assert_eq!(u, "http://1.2.3.4:7890");
        assert_eq!(t, "deadbeef0123");
    }

    #[test]
    fn parse_extra_params_ignored() {
        let (u, t) = parse_onboard_url(
            "taremote://onboard?foo=bar&url=http://x:7890&extra=baz&token=tok",
        )
        .unwrap();
        assert_eq!(u, "http://x:7890");
        assert_eq!(t, "tok");
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
}
