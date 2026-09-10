// SPDX-License-Identifier: MPL-2.0

//! Inspection mode — capture the JSON actually sent to Anthropic.
//!
//! When a request behaves oddly the one thing nobody can see is the request
//! itself. The client builds it, the proxy reshapes it (routing rewrites the
//! model, `anthropic-beta` is merged, the credential is swapped), and what
//! finally goes on the wire exists for a few milliseconds inside a blocking
//! task. This records it.
//!
//! # Why it is off, and why it turns itself off
//!
//! A captured request is a prompt, and a prompt is whatever the person was
//! working on — source, customer data, an API key they pasted in. That is a
//! different class of thing from the usage counters beside it, so:
//!
//! * **off by default**, armed explicitly for a stated number of minutes;
//! * **it disarms itself** when that runs out. A debug switch left on is how a
//!   week of everyone's prompts ends up in a file nobody remembers, and
//!   "remember to turn it off" is not a control;
//! * **bounded** — [`KEEP`] captures, each truncated to [`MAX_BODY`];
//! * **0600**, beside the accounts, and never served to a user key.
//!
//! # What is removed before anything is written
//!
//! Two credentials pass through this path and neither may be recorded:
//!
//! * the **proxy's** Claude OAuth token, in the outgoing `Authorization`;
//! * the **caller's** user key, in the incoming `x-api-key`.
//!
//! Headers are handled by allowlist — anything not named is dropped rather
//! than kept — and the bodies are additionally swept for token-shaped strings,
//! because a prompt can contain a credential that no header rule would catch
//! (someone pasting their own key in to ask why it is failing is precisely the
//! kind of session that gets inspected).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How many captures to keep. Small on purpose: this is for looking at the
/// last few calls, not for building an archive. With full bodies now stored
/// rather than clipped fragments, this is the number that bounds the file.
const KEEP: usize = 25;

/// Longest body recorded, in bytes.
///
/// This started at 32 KB, on the theory that a truncated request still showed
/// the interesting parts. It does not: a body cut in half is not JSON, so the
/// one thing anybody opens this panel to do — read the object that was sent —
/// was impossible for essentially every real request. A Claude Code call with
/// a working context is a few hundred KB, so the cap is now well clear of
/// real traffic and exists only so that one pathological request cannot fill
/// the disk.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// Longest an inspection window may be armed for.
///
/// A cap rather than a suggestion: the reason this exists is that a switch
/// left on indefinitely accumulates other people's prompts, and an operator
/// typing a large number is exactly how that happens.
pub const MAX_ARM_MINUTES: u64 = 60;

/// Headers worth keeping on a capture.
///
/// An allowlist. The two that must never appear — `authorization` and
/// `x-api-key` — are absent by construction rather than by a rule that could
/// be forgotten when a header is added.
const KEEP_HEADERS: &[&str] = &[
    "content-type",
    "anthropic-version",
    "anthropic-beta",
    "user-agent",
    "x-app",
    "x-claude-code-session-id",
    "accept",
];

/// One recorded call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capture {
    /// RFC3339.
    pub ts: String,
    pub account_id: String,
    pub account_email: String,
    pub method: String,
    /// Path and query as sent upstream.
    pub path: String,
    /// Where it went, after routing.
    pub provider: String,
    /// The model actually requested upstream, which is not necessarily the one
    /// the client asked for — that is half the point of looking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Whether this is the conversation or the auto-mode permission classifier.
    ///
    /// Defaults to `Work`, which is the right reading for the lines already on
    /// disk and for any request the detector does not fire on. The panel uses
    /// it to keep the judge out of the per-account call and token figures,
    /// which answer a question about work.
    #[serde(default)]
    pub kind: crate::classifier::Kind,
    pub request_headers: Vec<(String, String)>,
    /// The JSON sent to Anthropic, scrubbed and truncated.
    pub request_body: String,
    /// True when [`MAX_BODY`] cut it. Stated, so nobody debugs a JSON parse
    /// error caused by our own truncation.
    pub request_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// What upstream said this call cost, read off the response.
    ///
    /// The same numbers the billing path uses — taken from the one parse, not
    /// counted a second time. Without them the panel shows what was SENT and
    /// says nothing about what it cost, which is half of why anybody opens it:
    /// "why was that turn expensive" is answered by these four numbers beside
    /// the request that produced them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<crate::usage::Tokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_excerpt: Option<String>,
}

/// Replace credential-shaped runs with a marker.
///
/// Belt to the header allowlist's braces. Anthropic tokens are
/// `sk-ant-…` and this proxy's keys are `tap_…`; both are long unbroken runs
/// of URL-safe characters, so they are recognisable without knowing the value.
/// A prompt that contains one — someone pasting a key in to ask why it does
/// not work — must not turn a debugging aid into a second place the key lives.
#[must_use]
pub fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("sk-ant-").or_else(|| rest.find("tap_")) {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let end = tail
            .char_indices()
            .position(|(_, c)| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(tail.len());
        out.push_str("<redacted-credential>");
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Keep both ends of an over-long body.
fn clamp(body: &str) -> (String, bool) {
    if body.len() <= MAX_BODY {
        return (body.to_owned(), false);
    }
    // Split on a char boundary, or the slice panics on multi-byte input.
    let head_end = (0..=MAX_BODY / 2)
        .rev()
        .find(|i| body.is_char_boundary(*i))
        .unwrap_or(0);
    let tail_start = (body.len() - MAX_BODY / 2..body.len())
        .find(|i| body.is_char_boundary(*i))
        .unwrap_or(body.len());
    (
        format!(
            "{}\n\n… {} bytes omitted by tab-atelier-proxy …\n\n{}",
            &body[..head_end],
            body.len() - head_end - (body.len() - tail_start),
            &body[tail_start..]
        ),
        true,
    )
}

/// A request on its way upstream, as [`capture`] needs to see it.
///
/// A struct rather than eight parameters: they are all strings, and eight
/// positional strings is a call nobody can read and a swap nobody notices.
pub struct Outgoing<'a> {
    pub ts: String,
    pub account_id: &'a str,
    pub account_email: &'a str,
    pub method: &'a str,
    /// Path and query, as sent.
    pub path: &'a str,
    pub provider: &'a str,
    /// The conversation, or the auto-mode classifier. Detected from the body
    /// by the caller, which has already parsed it to route the request.
    pub kind: crate::classifier::Kind,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

/// Build a capture from the request as it is about to leave.
#[must_use]
pub fn capture(o: &Outgoing<'_>) -> Capture {
    let Outgoing {
        ts,
        account_id,
        account_email,
        method,
        path,
        provider,
        kind,
        headers,
        body,
    } = o;
    let raw = String::from_utf8_lossy(body);
    let scrubbed = scrub(&raw);
    let model = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("model").and_then(serde_json::Value::as_str).map(str::to_owned));
    let (request_body, request_truncated) = clamp(&scrubbed);
    Capture {
        ts: ts.clone(),
        account_id: (*account_id).to_owned(),
        account_email: (*account_email).to_owned(),
        method: (*method).to_owned(),
        path: (*path).to_owned(),
        provider: (*provider).to_owned(),
        model,
        kind: *kind,
        request_headers: headers
            .iter()
            .filter(|(k, _)| KEEP_HEADERS.contains(&k.to_ascii_lowercase().as_str()))
            .map(|(k, v)| (k.clone(), scrub(v)))
            .collect(),
        request_body,
        request_truncated,
        status: None,
        tokens: None,
        response_excerpt: None,
    }
}

/// The armed window and the ring of captures.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    /// Unix seconds until which capturing is on. Zero is off.
    armed_until: u64,
    recent: Vec<Capture>,
}

impl Store {
    /// Load previous captures, if any. Never arms itself on load: an armed
    /// window does not survive a restart, because the operator who armed it is
    /// not necessarily the one who restarted the service.
    #[must_use]
    pub fn load(dir: impl Into<PathBuf>) -> Self {
        let path = dir.into().join("inspect.jsonl");
        let recent = std::fs::read_to_string(&path).map_or_else(
            |_| Vec::new(),
            |raw| {
                raw.lines()
                    .filter_map(|l| serde_json::from_str::<Capture>(l).ok())
                    .rev()
                    .take(KEEP)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            },
        );
        Self {
            path,
            armed_until: 0,
            recent,
        }
    }

    /// Turn capture on for `minutes`, capped at [`MAX_ARM_MINUTES`].
    /// Returns the deadline.
    pub fn arm(&mut self, now: u64, minutes: u64) -> u64 {
        self.armed_until = now + minutes.clamp(1, MAX_ARM_MINUTES) * 60;
        self.armed_until
    }

    pub const fn disarm(&mut self) {
        self.armed_until = 0;
    }

    /// Whether a call happening now should be recorded.
    #[must_use]
    pub const fn armed(&self, now: u64) -> bool {
        self.armed_until > now
    }

    #[must_use]
    pub const fn armed_until(&self) -> u64 {
        self.armed_until
    }

    #[must_use]
    pub fn recent(&self) -> &[Capture] {
        &self.recent
    }

    /// Forget everything captured so far, on disk as well as in memory.
    ///
    /// # Errors
    /// The file exists and could not be removed.
    pub fn clear(&mut self) -> Result<(), String> {
        self.recent.clear();
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("remove {}: {e}", self.path.display())),
        }
    }

    /// Record one call, trimming to [`KEEP`].
    pub fn push(&mut self, c: Capture) {
        self.recent.push(c);
        let overflow = self.recent.len().saturating_sub(KEEP);
        if overflow > 0 {
            self.recent.drain(..overflow);
        }
        // Rewritten whole rather than appended: the ring has to be trimmed on
        // disk too, or a long window leaves every prompt of the session in a
        // file the bound implied was small.
        let _ = self.persist();
    }

    fn persist(&self) -> Result<(), String> {
        use std::io::Write as _;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // Prompts. Not world-readable, from creation.
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(|e| format!("open {}: {e}", tmp.display()))?;
        for c in &self.recent {
            let line = serde_json::to_string(c).map_err(|e| e.to_string())?;
            writeln!(f, "{line}").map_err(|e| e.to_string())?;
        }
        f.sync_all().map_err(|e| e.to_string())?;
        drop(f);
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ta-inspect-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }

    #[test]
    fn neither_credential_on_this_path_can_reach_the_file() {
        // The two that pass through: the proxy's Claude token and the caller's
        // user key. A capture that recorded either would turn a debugging aid
        // into a second place a live credential lives.
        let body = r#"{"model":"claude-opus-5","system":"my key is sk-ant-oat01-AAAABBBBCCCC and the proxy one is tap_deadbeefcafe0123"}"#;
        let out = scrub(body);
        assert!(!out.contains("sk-ant-oat01-AAAABBBBCCCC"), "{out}");
        assert!(!out.contains("tap_deadbeefcafe0123"), "{out}");
        assert_eq!(out.matches("<redacted-credential>").count(), 2, "{out}");
        // Everything around them survives — a scrub that ate the request would
        // defeat the purpose.
        assert!(out.contains(r#""model":"claude-opus-5""#), "{out}");
        assert!(out.contains("my key is"), "{out}");
    }

    #[test]
    fn the_authorization_header_is_absent_by_construction() {
        // Headers are an allowlist, so a credential is not dropped by a rule
        // that someone could forget to update — it is simply never named.
        let headers = vec![
            ("Authorization".to_owned(), "Bearer sk-ant-oat01-LIVE".to_owned()),
            ("x-api-key".to_owned(), "tap_callerkey".to_owned()),
            ("CF-Access-Client-Secret".to_owned(), "cf-secret".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
            ("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned()),
        ];
        let c = capture(&Outgoing {
            ts: "2026-09-10T12:00:00Z".to_owned(),
            account_id: "id",
            account_email: "ada@example.org",
            method: "POST",
            path: "/v1/messages",
            provider: "anthropic",
            kind: crate::classifier::Kind::Work,
            headers: &headers,
            body: br#"{"model":"claude-haiku-4-5-20251001"}"#,
        });
        let names: Vec<_> = c.request_headers.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
        assert!(!names.contains(&"authorization".to_owned()), "{names:?}");
        assert!(!names.contains(&"x-api-key".to_owned()), "{names:?}");
        assert!(!names.contains(&"cf-access-client-secret".to_owned()), "{names:?}");
        assert!(names.contains(&"content-type".to_owned()));
        let serialised = serde_json::to_string(&c).expect("serialise");
        assert!(!serialised.contains("sk-ant-oat01-LIVE"), "{serialised}");
        assert!(!serialised.contains("tap_callerkey"), "{serialised}");
        assert!(!serialised.contains("cf-secret"), "{serialised}");
        // The model upstream was asked for is pulled out for the list view.
        assert_eq!(c.model.as_deref(), Some("claude-haiku-4-5-20251001"));
    }

    #[test]
    fn an_armed_window_expires_on_its_own_and_cannot_be_armed_forever() {
        let mut s = Store::load(tmp("arm"));
        assert!(!s.armed(1000), "off by default — prompts are not collected unasked");

        let deadline = s.arm(1000, 15);
        assert_eq!(deadline, 1000 + 15 * 60);
        assert!(s.armed(1000));
        assert!(s.armed(1000 + 14 * 60));
        // The whole point: nobody has to remember to switch it off.
        assert!(!s.armed(1000 + 15 * 60), "the window closes by itself");

        // A large number is clamped rather than honoured.
        let deadline = s.arm(0, 10_000);
        assert_eq!(
            deadline,
            MAX_ARM_MINUTES * 60,
            "an unbounded window is the failure mode"
        );
    }

    #[test]
    fn captures_are_bounded_and_land_unreadable_to_anyone_else() {
        let dir = tmp("ring");
        let mut s = Store::load(&dir);
        let one = |n: usize| {
            capture(&Outgoing {
                ts: format!("2026-09-10T12:00:{n:02}Z"),
                account_id: "id",
                account_email: "ada@example.org",
                method: "POST",
                path: "/v1/messages",
                provider: "anthropic",
                kind: crate::classifier::Kind::Work,
                headers: &[],
                body: br#"{"model":"m"}"#,
            })
        };
        for n in 0..KEEP + 10 {
            s.push(one(n));
        }
        assert_eq!(s.recent().len(), KEEP, "the ring must not grow without bound");
        // Trimmed on disk too, not just in memory.
        let lines = std::fs::read_to_string(s.path()).expect("read").lines().count();
        assert_eq!(lines, KEEP, "the file must be bounded as well");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(s.path()).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "captured prompts must not be world-readable");
        }

        // Reload keeps the captures but NOT the armed window: whoever restarts
        // the service has not consented to keep collecting.
        s.arm(1000, 30);
        let back = Store::load(&dir);
        assert_eq!(back.recent().len(), KEEP);
        assert!(!back.armed(1000), "an armed window must not survive a restart");

        let mut back = back;
        back.clear().expect("clear");
        assert!(back.recent().is_empty());
        assert!(!back.path().exists(), "clearing removes the file, not just the memory");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_enormous_body_keeps_both_ends_and_says_so() {
        // A real request is megabytes and the interesting parts are the model
        // and system blocks at the start and the newest turn at the end.
        // The cap is megabytes now, so the fixture has to be bigger than a
        // real request to exercise the guard at all. That is the point of
        // raising it: nothing ordinary meets it.
        let big = format!(
            r#"{{"model":"claude-opus-5","messages":[{}]}}"#,
            "x".repeat(MAX_BODY + 1024)
        );
        let c = capture(&Outgoing {
            ts: "t".to_owned(),
            account_id: "id",
            account_email: "e",
            method: "POST",
            path: "/v1/messages",
            provider: "anthropic",
            kind: crate::classifier::Kind::Work,
            headers: &[],
            body: big.as_bytes(),
        });
        assert!(c.request_truncated, "truncation must be declared, not silent");
        assert!(c.request_body.len() < big.len());
        assert!(
            c.request_body.contains(r#""model":"claude-opus-5""#),
            "the head survives"
        );
        assert!(
            c.request_body.ends_with("]}"),
            "the tail survives: {:?}",
            &c.request_body[c.request_body.len() - 20..]
        );
        assert!(c.request_body.contains("omitted by tab-atelier-proxy"));
    }

    #[test]
    fn truncation_does_not_split_a_multibyte_character() {
        // Slicing a String on a byte index panics mid-character, and a prompt
        // is exactly the place non-ASCII shows up.
        // é is two bytes in UTF-8, so this is 2×MAX_BODY bytes.
        let big = "é".repeat(MAX_BODY);
        let (out, cut) = clamp(&big);
        assert!(cut);
        assert!(out.contains('é'));
    }
}
