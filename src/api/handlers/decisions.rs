// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `decisions` route handlers: the KIOSK cross-project decision read-model (PD2) + the
//! Lu/Tranché write path. The panel renders the SERVER read-model VERBATIM (no JS
//! re-gate — the fold in `cli::decision` owns state/verdict/visibility), exactly the
//! catalogue's contract. Archiving the `files[]` is PD3; `tranch` only transits state.

use std::fmt::Write as _;
use std::io::Write;
use std::sync::{Arc, Mutex};

use super::super::{TabSnapshot, error_json, respond_bytes, respond_json};

/// `GET /decisions/file?path=<abs>` — serve a decision bundle's CONTENT, SANDBOXED to
/// `<outbox>` and its `_archive/` subtree (#kiosk).
///
/// The KIOSK panel links point here: a raw outbox path 401s at the daemon, so this route
/// lets the PO READ a bundle we SHOW them. Every path is `~`-expanded then CANONICALIZED
/// (collapsing `..` and symlinks) and must live under the canonicalized outbox — anything
/// outside (the source tree, `~/.ssh`, `/etc/…`) is refused 403. Served as text/plain.
/// READ-ONLY.
pub(in crate::api) fn file<S: Write>(stream: &mut S, path_q: Option<&str>) {
    let Some(raw) = path_q.filter(|s| !s.trim().is_empty()) else {
        error_json(stream, 400, "decisions file: ?path= is required");
        return;
    };
    let requested = raw.strip_prefix("~/").map_or_else(
        || std::path::PathBuf::from(raw),
        |rest| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
    );
    // Canonicalize both sides so the sandbox check can't be walked out of (`..`, symlink).
    // A non-existent file / unreadable outbox → 404 (never leak whether a path exists
    // outside the sandbox — the confinement check runs on the canonical form first).
    let (Ok(canon), Ok(base)) = (std::fs::canonicalize(&requested), std::fs::canonicalize(crate::cli::decision::outbox_base()))
    else {
        error_json(stream, 404, "decisions file: not found");
        return;
    };
    if !canon.starts_with(&base) {
        error_json(stream, 403, "decisions file: outside the outbox sandbox");
        return;
    }
    if !canon.is_file() {
        error_json(stream, 404, "decisions file: not a file");
        return;
    }
    let is_md = canon
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"));
    match std::fs::read(&canon) {
        // Item 3 (#kiosk): a .md bundle is RENDERED to sanitized HTML (headings/lists/code/
        // bold) so the viewer shows readable prose, not raw monospace markdown. Everything
        // else stays verbatim text/plain.
        Ok(bytes) if is_md => {
            let name = canon.file_name().and_then(|n| n.to_str()).unwrap_or("document");
            let page = render_markdown_page(name, &String::from_utf8_lossy(&bytes));
            respond_bytes(stream, 200, "text/html; charset=utf-8", page.as_bytes());
        }
        Ok(bytes) => respond_bytes(stream, 200, "text/plain; charset=utf-8", &bytes),
        Err(_) => error_json(stream, 404, "decisions file: unreadable"),
    }
}

/// HTML-escape (XSS): the FIRST transform applied to every span of markdown source, so no
/// `<`/`>`/`&`/`"` from the document reaches the DOM as live markup. Every markdown tag we
/// emit afterwards is our OWN static string, never derived from the input. PURE.
fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Inline markdown on an ALREADY-escaped span: paired `` `code` `` then `**bold**`. The tags
/// are the only HTML introduced (input was escaped first). An unpaired delimiter is left
/// literal. PURE.
fn md_inline(escaped: &str) -> String {
    let coded = wrap_delim(escaped, "`", "<code>", "</code>");
    wrap_delim(&coded, "**", "<strong>", "</strong>")
}

/// Replace PAIRED occurrences of `delim` with `open`…`close`; if the count is odd the whole
/// span is returned unchanged (delimiters stay literal). PURE.
fn wrap_delim(s: &str, delim: &str, open: &str, close: &str) -> String {
    if s.matches(delim).count() < 2 {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut opened = false;
    while let Some(pos) = rest.find(delim) {
        out.push_str(&rest[..pos]);
        out.push_str(if opened { close } else { open });
        opened = !opened;
        rest = &rest[pos + delim.len()..];
    }
    out.push_str(rest);
    if opened {
        return s.to_string(); // odd trailing delimiter — leave the span literal
    }
    out
}

/// An ATX heading line (`#`..`######` + a space) → (level, text). PURE.
fn heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ') {
        Some((hashes, line[hashes + 1..].trim()))
    } else {
        None
    }
}

/// Minimal, DEPENDENCY-FREE markdown → HTML for the `.md` viewer. XSS-safe by construction:
/// escape-first, then only our own tags. Supports ATX headings, `-`/`*` bullet lists, fenced
/// ```` ``` ```` code (verbatim, escaped), blank-line paragraphs, inline `**bold**` /
/// `` `code` ``.
/// ponytail 🟡: no tables / nested lists / links — enough to read a decision bundle; swap in
/// pulldown-cmark if richer docs are ever needed.
fn md_to_html(src: &str) -> String {
    let mut out = String::new();
    let mut para = String::new();
    let mut code = String::new();
    let mut in_code = false;
    let mut in_list = false;
    let flush = |out: &mut String, para: &mut String, in_list: &mut bool| {
        if !para.is_empty() {
            out.push_str("<p>");
            out.push_str(para);
            out.push_str("</p>\n");
            para.clear();
        }
        if *in_list {
            out.push_str("</ul>\n");
            *in_list = false;
        }
    };
    for line in src.lines() {
        if line.trim_start().starts_with("```") {
            if in_code {
                out.push_str("<pre class=\"md-code\"><code>");
                out.push_str(&esc_html(&code));
                out.push_str("</code></pre>\n");
                code.clear();
                in_code = false;
            } else {
                flush(&mut out, &mut para, &mut in_list);
                in_code = true;
            }
            continue;
        }
        if in_code {
            if !code.is_empty() {
                code.push('\n');
            }
            code.push_str(line);
            continue;
        }
        let trimmed = line.trim_end();
        if let Some((level, text)) = heading(trimmed.trim_start()) {
            flush(&mut out, &mut para, &mut in_list);
            let inner = md_inline(&esc_html(text));
            let _ = writeln!(out, "<h{level}>{inner}</h{level}>");
        } else if let Some(item) = trimmed.trim_start().strip_prefix("- ").or_else(|| trimmed.trim_start().strip_prefix("* ")) {
            if !para.is_empty() {
                out.push_str("<p>");
                out.push_str(&para);
                out.push_str("</p>\n");
                para.clear();
            }
            if !in_list {
                out.push_str("<ul>\n");
                in_list = true;
            }
            let _ = writeln!(out, "<li>{}</li>", md_inline(&esc_html(item)));
        } else if trimmed.is_empty() {
            flush(&mut out, &mut para, &mut in_list);
        } else {
            if in_list {
                out.push_str("</ul>\n");
                in_list = false;
            }
            if !para.is_empty() {
                para.push_str("<br>");
            }
            para.push_str(&md_inline(&esc_html(trimmed)));
        }
    }
    if in_code {
        out.push_str("<pre class=\"md-code\"><code>");
        out.push_str(&esc_html(&code));
        out.push_str("</code></pre>\n");
    }
    flush(&mut out, &mut para, &mut in_list);
    out
}

/// Readability CSS for the .md viewer — dark, matching the dashboard palette.
const MD_VIEWER_CSS: &str = "body{background:#1e1e1e;color:#cdd;font:16px/1.6 system-ui,sans-serif;margin:0}\
.md-doc{max-width:52rem;margin:2rem auto;padding:0 1.2rem}\
.md-doc h1,.md-doc h2,.md-doc h3{color:#fff;line-height:1.25}\
.md-doc code{background:#2a2a2a;border-radius:4px;padding:.1em .3em;font-family:ui-monospace,monospace;font-size:.9em}\
.md-doc pre.md-code{background:#2a2a2a;border:1px solid #444;border-radius:6px;padding:.7rem;overflow:auto}\
.md-doc pre.md-code code{background:none;padding:0}\
.md-doc a{color:#6cf}";

/// Wrap rendered markdown in a minimal, self-contained HTML document (noindex — a decision
/// bundle is not for crawlers). `title` is escaped; the body is already sanitized by
/// [`md_to_html`]. PURE.
fn render_markdown_page(title: &str, md: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"fr\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<meta name=\"robots\" content=\"noindex, nofollow\">\
<title>{}</title><style>{MD_VIEWER_CSS}</style></head>\
<body><main class=\"md-doc\">{}</main></body></html>",
        esc_html(title),
        md_to_html(md)
    )
}

/// `GET /decisions[?includeArchived]` — the folded cross-project decision read-model
/// (PD1 fold, camelCase). READ-ONLY. `?includeArchived` surfaces archived decisions
/// (`state:archived`); the default hides them. A missing log reads empty. Returns 200
/// `{decisions:[…]}`.
pub(in crate::api) fn list<S: Write>(stream: &mut S, include_archived: bool) {
    let decisions = crate::cli::decision::read_decisions(include_archived);
    let body = serde_json::to_string(&serde_json::json!({ "decisions": decisions })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /decisions/{id}/{read|tranch}` (PD2) — the KIOSK state mutations. Each APPENDS
/// an event (event-sourced; the fold derives the new state), UNDER THE DAEMON LOCK
/// (single-writer — the same lock the catalogue holds, so all cold-source writers
/// serialise) + a read-back gate (append → re-read → confirm our event is the latest
/// for this id → 200). `tranch` requires a non-empty `{verdict}` (a ruling without a
/// verdict is meaningless — mirrors the CLI's `--verdict` requirement). Optional
/// `{by}`. Archiving the `files[]` is PD3.
pub(in crate::api) fn mutate<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    use crate::cli::decision::{
        DecisionEvent, DecisionKind, append_line, archive_decision, decisions_path, outbox_base, parse_decisions,
    };

    #[derive(serde::Deserialize, Default)]
    struct MarkBody {
        verdict: Option<String>,
        by: Option<String>,
    }

    let Some((id_enc, verb)) = p.strip_prefix("/decisions/").and_then(|rest| rest.rsplit_once('/')) else {
        error_json(stream, 404, "bad decisions path");
        return;
    };
    let id = String::from_utf8_lossy(&crate::api_ws::percent_decode(id_enc)).into_owned();
    if id.trim().is_empty() {
        error_json(stream, 400, "empty decision id");
        return;
    }
    let kind = match verb {
        "read" => DecisionKind::Read,
        "tranch" => DecisionKind::Tranched,
        _ => {
            error_json(stream, 404, "unknown decision verb");
            return;
        }
    };

    let body: MarkBody = serde_json::from_slice(body_bytes).unwrap_or_default();
    let verdict = body.verdict.filter(|v| !v.trim().is_empty());
    if kind == DecisionKind::Tranched && verdict.is_none() {
        error_json(stream, 400, "decision tranch: a non-empty verdict is required");
        return;
    }
    let now = crate::unix_millis() / 1000;
    let ev = DecisionEvent { id: id.clone(), kind, at: now, by: body.by, verdict, ..Default::default() };
    let path = decisions_path();

    // Under the daemon lock: append the state event, then (on tranch) ARCHIVE — the
    // ruling triggers filing the bundle under _archive/AAAA-MM/ + appending the `archived`
    // event (PD3), so the decision leaves the active list (reversible via a re-open). The
    // read-back gate confirms the FINAL event landed for this id — `archived` after a
    // tranch, else our own event — a true read-back independent of the folded state.
    let guard = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if append_line(&path, &ev).is_err() {
        drop(guard);
        error_json(stream, 500, "decision: append failed");
        return;
    }
    let expected = if kind == DecisionKind::Tranched {
        // The state transition is recorded even if no file moved; a hard move I/O error
        // 500s (the `tranched` event stands — the ruling isn't lost).
        if let Err(e) = archive_decision(&path, &outbox_base(), &id, now) {
            drop(guard);
            error_json(stream, 500, &format!("decision tranch: archive failed — {e}"));
            return;
        }
        DecisionKind::Archived
    } else {
        kind
    };
    let landed = std::fs::read_to_string(&path)
        .is_ok_and(|body| parse_decisions(&body).iter().rev().find(|e| e.id == id).is_some_and(|e| e.kind == expected));
    drop(guard);
    if landed {
        respond_json(stream, 200, &format!(r#"{{"{verb}":"{id}"}}"#));
    } else {
        error_json(stream, 500, "decision: read-back failed");
    }
}

#[cfg(test)]
mod md_viewer_tests {
    use super::{md_to_html, render_markdown_page};

    // Item 3 (#kiosk): a .md is RENDERED (headings/lists/code/bold), not shown raw — AND the
    // render is XSS-safe (escape-first): any HTML in the source is neutralized to text.
    #[test]
    fn renders_markdown_to_html_not_raw() {
        let html = md_to_html("# Titre\n\nUn **gras** et du `code`.\n\n- a\n- b\n\n```\nx=1\n```");
        assert!(html.contains("<h1>Titre</h1>"), "heading rendered: {html}");
        assert!(html.contains("<strong>gras</strong>"), "bold rendered: {html}");
        assert!(html.contains("<code>code</code>"), "inline code rendered: {html}");
        assert!(html.contains("<ul>") && html.contains("<li>a</li>") && html.contains("<li>b</li>"), "list rendered: {html}");
        assert!(html.contains("<pre class=\"md-code\"><code>x=1</code></pre>"), "fenced code rendered: {html}");
        // NOT raw markdown left in the output.
        assert!(!html.contains("# Titre") && !html.contains("**gras**"), "no raw markdown leaked: {html}");
    }

    #[test]
    fn xss_safe_escape_first() {
        // A doc trying to inject a live script / img — every angle bracket must be escaped.
        let html = md_to_html("## <script>alert(1)</script>\n\nhi <img src=x onerror=alert(2)> there");
        assert!(!html.contains("<script>"), "raw <script> must never survive: {html}");
        assert!(!html.contains("<img "), "raw <img> must never survive: {html}");
        assert!(html.contains("&lt;script&gt;"), "script tag escaped to text: {html}");
        assert!(html.contains("&lt;img"), "img tag escaped to text: {html}");
        // The heading structure is still applied around the escaped text.
        assert!(html.contains("<h2>") && html.contains("&lt;script&gt;"), "heading wraps escaped text: {html}");
    }

    #[test]
    fn page_wrapper_is_self_contained_html() {
        let page = render_markdown_page("mon-fichier.md", "# Hi");
        assert!(page.starts_with("<!doctype html>"), "full HTML document");
        assert!(page.contains("noindex"), "a decision bundle is not for crawlers");
        assert!(page.contains("<title>mon-fichier.md</title>"), "title present + escaped");
        assert!(page.contains("<h1>Hi</h1>"), "body is the rendered markdown");
    }
}
