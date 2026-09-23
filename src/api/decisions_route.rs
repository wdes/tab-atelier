// SPDX-License-Identifier: MPL-2.0

//! `decisions` route handlers: the KIOSK cross-project decision read-model (PD2) + the
//! Lu/Tranché write path. The panel renders the SERVER read-model VERBATIM (no JS
//! re-gate — the fold in `cli::decision` owns state/verdict/visibility), exactly the
//! catalogue's contract. Archiving the `files[]` is PD3; `tranch` only transits state.

use std::fmt::Write as _;
use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, error_json, respond_bytes, respond_json, url_decode};

/// `GET /decisions/file?path=<abs>` — serve a decision bundle's CONTENT, SANDBOXED to
/// `<outbox>` and its `_archive/` subtree (#kiosk).
///
/// The KIOSK panel links point here: a raw outbox path 401s at the daemon, so this route
/// lets the PO READ a bundle we SHOW them. A bare `outbox/…`/`_archive/…` ref (the shape
/// decision `--files` carry) is anchored on `outbox_base()`, not the CWD; then every path is
/// `~`-expanded then CANONICALIZED
/// (collapsing `..` and symlinks) and must live under the canonicalized outbox — anything
/// outside (the source tree, `~/.ssh`, `/etc/…`) is refused 403. Served as text/plain.
/// READ-ONLY.
// ---------------------------------------------------------------------------
// Route path seams. The routes read their two on-disk roots through `outbox_root()`
// / `decisions_log()`. In production these are exactly `decision::outbox_base()` /
// `decision::decisions_path()`; under `cfg(test)` a THREAD-LOCAL override (pinned by
// `route_tests`) points them at a tempdir. A thread-local rather than `env::set_var`,
// which is `unsafe` (denied crate-wide) and process-global — it would race parallel
// tests for no benefit.
// ---------------------------------------------------------------------------
#[cfg(not(test))]
fn outbox_root() -> std::path::PathBuf {
    crate::cli::decision::outbox_base()
}
#[cfg(not(test))]
fn decisions_log() -> std::path::PathBuf {
    crate::cli::decision::decisions_path()
}

#[cfg(test)]
thread_local! {
    static TEST_OUTBOX: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
    static TEST_DECISIONS: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn outbox_root() -> std::path::PathBuf {
    TEST_OUTBOX
        .with(|c| c.borrow().clone())
        .unwrap_or_else(crate::cli::decision::outbox_base)
}
#[cfg(test)]
fn decisions_log() -> std::path::PathBuf {
    TEST_DECISIONS
        .with(|c| c.borrow().clone())
        .unwrap_or_else(crate::cli::decision::decisions_path)
}

pub(in crate::api) fn file<S: Write>(stream: &mut S, path_q: Option<&str>) {
    let Some(raw) = path_q.filter(|s| !s.trim().is_empty()) else {
        error_json(stream, 400, "decisions file: ?path= is required");
        return;
    };
    // Resolve the request to an absolute path BEFORE canonicalizing. Three shapes reach us:
    //  - `~/…`  → HOME-expanded (the `~/Dev/outbox/…` shape the panel builds).
    //  - a BARE `outbox/…` / `_archive/…` → decision `--files` are pushed WITHOUT the ~/Dev
    //    prefix, so a plain `PathBuf::from(raw)` would canonicalize RELATIVE to the daemon
    //    CWD (`/home/mox2` in prod, not `~/Dev`) → the wrong file → a spurious 404. Anchor
    //    these on `outbox_base()` DETERMINISTICALLY instead — CWD-independent by construction
    //    (`_archive/` lives under the outbox, so it keeps its whole segment).
    //  - anything else → taken as-is (an absolute path); the canonicalize + confinement
    //    check below still gates it to the sandbox.
    let base_dir = outbox_root();
    let requested = raw
        .strip_prefix("~/")
        .map(|rest| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest))
        .or_else(|| raw.strip_prefix("outbox/").map(|rest| base_dir.join(rest)))
        .or_else(|| raw.strip_prefix("_archive/").map(|_| base_dir.join(raw)))
        .unwrap_or_else(|| std::path::PathBuf::from(raw));
    // Canonicalize both sides so the sandbox check can't be walked out of (`..`, symlink).
    // A non-existent file / unreadable outbox → 404 (never leak whether a path exists
    // outside the sandbox — the confinement check runs on the canonical form first).
    let (Ok(canon), Ok(base)) = (std::fs::canonicalize(&requested), std::fs::canonicalize(&base_dir)) else {
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
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
        } else if let Some(item) = trimmed
            .trim_start()
            .strip_prefix("- ")
            .or_else(|| trimmed.trim_start().strip_prefix("* "))
        {
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
    use crate::cli::decision::{DecisionEvent, DecisionKind, append_line, archive_decision, parse_decisions};

    #[derive(serde::Deserialize, Default)]
    struct MarkBody {
        verdict: Option<String>,
        by: Option<String>,
    }

    let Some((id_enc, verb)) = p.strip_prefix("/decisions/").and_then(|rest| rest.rsplit_once('/')) else {
        error_json(stream, 404, "bad decisions path");
        return;
    };
    let id = url_decode(id_enc);
    // B1: refuse a hostile id at the trust boundary — charset/length, and no `.`/`..`/slash
    // — so the id can never be `join`ed out of the archive root (400, not a 404: the shape
    // is wrong, not the routing).
    if !crate::cli::decision::valid_decision_id(&id) {
        error_json(stream, 400, "invalid decision id");
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
    let ev = DecisionEvent {
        id: id.clone(),
        kind,
        at: now,
        by: body.by,
        verdict,
        ..Default::default()
    };
    let path = decisions_log();

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
        if let Err(e) = archive_decision(&path, &outbox_root(), &id, now) {
            drop(guard);
            error_json(stream, 500, &format!("decision tranch: archive failed — {e}"));
            return;
        }
        DecisionKind::Archived
    } else {
        kind
    };
    let landed = std::fs::read_to_string(&path).is_ok_and(|body| {
        parse_decisions(&body)
            .iter()
            .rev()
            .find(|e| e.id == id)
            .is_some_and(|e| e.kind == expected)
    });
    drop(guard);
    if landed {
        // Built through serde (never string-formatted): `verb` is the dynamic KEY and `id`
        // comes from the URL, so both get escaped by the serializer.
        let body = serde_json::to_string(&serde_json::json!({ (verb): id })).unwrap_or_default();
        respond_json(stream, 200, &body);
    } else {
        error_json(stream, 500, "decision: read-back failed");
    }
}

/// `GET /reports` — list the report documents living directly under `outbox_base()`
/// (top-level `*.md`/`*.markdown`, newest first). READ-ONLY. Each item's `path` is the bare
/// `outbox/<name>` form the `/decisions/file` viewer resolves against `outbox_base()`
/// (CWD-independent, same sandbox as the decisions). `_archive/` and dotfiles are skipped; a
/// missing outbox reads empty. The remote-share link (volet 3) is a CLIENT concern layered on
/// `path` later — NOT built here (clean seam).
pub(in crate::api) fn reports<S: Write>(stream: &mut S) {
    let base = outbox_root();
    let mut items: Vec<(u64, String)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&base) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue; // skip dotfiles (and never `_archive`, a dir, since we require a file below)
            }
            let is_md = std::path::Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"));
            if !is_md || !ent.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let mtime = ent
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            items.push((mtime, name));
        }
    }
    // Newest first; stable by name for equal mtimes (deterministic ordering for the check).
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let reports: Vec<_> = items
        .into_iter()
        .map(|(mtime, name)| serde_json::json!({ "name": name, "path": format!("outbox/{name}"), "mtime": mtime }))
        .collect();
    let body = serde_json::to_string(&serde_json::json!({ "reports": reports })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /intent` (volet-2 grille d'intention) — persist an intention grid as a NEW
/// `intent-<ts>.md` under `outbox_base()`. The filename is SERVER-GENERATED from the daemon
/// clock (`ts` = unix millis, digits only) — NEVER from the client, so there is no path
/// traversal and no overwrite of an existing bundle. The body is `{content:"…markdown…"}`
/// (the client folds the Given/When/Then fields into markdown); it is written VERBATIM as
/// text — the `/decisions/file` viewer renders it XSS-safe (escape-first) on read, so no
/// markup from the payload is ever executed. Narrow write scope (master-only, same outbox
/// sandbox as the decisions). Returns 200 `{path, name}` — the bare `outbox/…` the viewer resolves.
pub(in crate::api) fn intent<S: Write>(stream: &mut S, body_bytes: &[u8]) {
    #[derive(serde::Deserialize, Default)]
    struct IntentBody {
        content: Option<String>,
    }
    let body: IntentBody = serde_json::from_slice(body_bytes).unwrap_or_default();
    let content = body.content.unwrap_or_default();
    if content.trim().is_empty() {
        error_json(stream, 400, "intent: non-empty content is required");
        return;
    }
    let base = outbox_root();
    if std::fs::create_dir_all(&base).is_err() {
        error_json(stream, 500, "intent: outbox unavailable");
        return;
    }
    // Digits-only, server-clock name — traversal-impossible by construction.
    let name = format!("intent-{}.md", crate::unix_millis());
    match std::fs::write(base.join(&name), content.as_bytes()) {
        Ok(()) => {
            // Same rule as `mutate`: serialized, not `format!`-interpolated (`name` lands twice).
            let body = serde_json::to_string(&serde_json::json!({ "path": format!("outbox/{name}"), "name": name }))
                .unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        Err(_) => error_json(stream, 500, "intent: write failed"),
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
        assert!(
            html.contains("<ul>") && html.contains("<li>a</li>") && html.contains("<li>b</li>"),
            "list rendered: {html}"
        );
        assert!(
            html.contains("<pre class=\"md-code\"><code>x=1</code></pre>"),
            "fenced code rendered: {html}"
        );
        // NOT raw markdown left in the output.
        assert!(
            !html.contains("# Titre") && !html.contains("**gras**"),
            "no raw markdown leaked: {html}"
        );
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
        assert!(
            html.contains("<h2>") && html.contains("&lt;script&gt;"),
            "heading wraps escaped text: {html}"
        );
    }

    #[test]
    fn page_wrapper_is_self_contained_html() {
        let page = render_markdown_page("mon-fichier.md", "# Hi");
        assert!(page.starts_with("<!doctype html>"), "full HTML document");
        assert!(page.contains("noindex"), "a decision bundle is not for crawlers");
        assert!(
            page.contains("<title>mon-fichier.md</title>"),
            "title present + escaped"
        );
        assert!(page.contains("<h1>Hi</h1>"), "body is the rendered markdown");
    }
}

/// Route-level tests (B3): the KIOSK decision routes driven through their real entry
/// points with a `Vec<u8>` sink, so a refusal or a shape change is caught at the
/// BOUNDARY, not just in the pure helpers. The two on-disk roots are pinned to a
/// tempdir via the thread-local seams above (hermetic, no env, no shared state, no
/// sleeps — safe under the default parallel test runner).
#[cfg(test)]
mod route_tests {
    use super::{file, intent, mutate, reports};
    use std::sync::{Arc, Mutex};

    fn status_code(response: &str) -> u16 {
        response
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    }
    fn body(response: &str) -> &str {
        response.split("\r\n\r\n").nth(1).unwrap_or("")
    }

    /// Run `f` with both route roots pinned under `dir` (an `outbox/` subdir is made,
    /// as the routes expect it to exist), then unpin. The pins are thread-local, so
    /// parallel tests never see them.
    fn with_roots<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        std::fs::create_dir_all(dir.join("outbox")).unwrap();
        super::TEST_OUTBOX.with(|c| *c.borrow_mut() = Some(dir.join("outbox")));
        super::TEST_DECISIONS.with(|c| *c.borrow_mut() = Some(dir.join("decisions.jsonl")));
        let out = f();
        super::TEST_OUTBOX.with(|c| *c.borrow_mut() = None);
        super::TEST_DECISIONS.with(|c| *c.borrow_mut() = None);
        out
    }

    /// B1 (ITEM-1): a hostile id is refused 400 at the ROUTE boundary, BEFORE the
    /// `{"<verb>": id}` reply and before anything is joined onto the archive root.
    #[test]
    fn mutate_refuses_a_hostile_decision_id_before_the_join() {
        let dir = tempfile::tempdir().unwrap();
        with_roots(dir.path(), || {
            let state = Arc::new(Mutex::new(super::super::test_snapshot(vec![])));
            // Traversal, encoded traversal, separator, and empty — a wrong SHAPE is a
            // 400, never a 404, and never a join.
            for p in [
                "/decisions/../x/read",
                "/decisions/..%2Fx/read",
                "/decisions/a%2Fb/tranch",
                "/decisions//read",
            ] {
                let mut out = Vec::new();
                mutate(&mut out, &state, p, br#"{"verdict":"ok"}"#);
                let resp = String::from_utf8(out).unwrap();
                assert_eq!(status_code(&resp), 400, "{p} must be refused: {resp}");
                assert!(resp.contains("invalid decision id"), "{p}: {resp}");
            }
            // The refusal precedes any write: the event log was never created.
            assert!(
                !dir.path().join("decisions.jsonl").exists(),
                "no event for a hostile id"
            );
        });
    }

    /// Happy path: a well-formed id is recorded, and the reply is the SERIALIZED
    /// `{"read": id}` (not a hand-formatted string that a quote in `id` could break).
    #[test]
    fn mutate_recorded_read_answers_200_with_the_id() {
        let dir = tempfile::tempdir().unwrap();
        with_roots(dir.path(), || {
            let state = Arc::new(Mutex::new(super::super::test_snapshot(vec![])));
            let mut out = Vec::new();
            mutate(&mut out, &state, "/decisions/dec.2_x-3/read", b"");
            let resp = String::from_utf8(out).unwrap();
            assert_eq!(status_code(&resp), 200, "{resp}");
            assert_eq!(body(&resp), r#"{"read":"dec.2_x-3"}"#, "serialized reply: {resp}");
            let log = std::fs::read_to_string(dir.path().join("decisions.jsonl")).unwrap();
            assert!(log.contains(r#""id":"dec.2_x-3""#), "event appended: {log}");
            assert!(log.contains(r#""kind":"read""#), "kind recorded: {log}");
        });
    }

    /// `file`: the outbox sandbox holds at the route (an absolute path outside, and an
    /// `outbox/../` that canonicalizes back out, are both refused 403 — the content
    /// never leaks); a path INSIDE is served, with a `.md` RENDERED to HTML.
    #[test]
    fn file_confines_the_outbox_and_serves_what_is_inside_it() {
        let dir = tempfile::tempdir().unwrap();
        with_roots(dir.path(), || {
            let outbox = dir.path().join("outbox");
            std::fs::write(outbox.join("ok.md"), "# hi\n").unwrap();
            let secret = dir.path().join("secret.txt");
            std::fs::write(&secret, "top-secret").unwrap();

            // Inside: 200, rendered (not raw markdown).
            let mut out = Vec::new();
            file(&mut out, Some("outbox/ok.md"));
            let resp = String::from_utf8(out).unwrap();
            assert_eq!(status_code(&resp), 200, "{resp}");
            assert!(resp.contains("<h1>hi</h1>"), "rendered html: {resp}");

            // Outside (absolute) and `outbox/../` (collapsed back out): both 403, no leak.
            for p in [secret.to_str().unwrap(), "outbox/../secret.txt"] {
                let mut out = Vec::new();
                file(&mut out, Some(p));
                let resp = String::from_utf8(out).unwrap();
                assert_eq!(status_code(&resp), 403, "{p} must stay in the sandbox: {resp}");
                assert!(!resp.contains("top-secret"), "content must never leak: {resp}");
            }

            // A missing bundle is a plain 404.
            let mut out = Vec::new();
            file(&mut out, Some("outbox/nope.md"));
            assert_eq!(status_code(&String::from_utf8(out).unwrap()), 404);

            // No `?path=` at all is a 400.
            let mut out = Vec::new();
            file(&mut out, None);
            assert_eq!(status_code(&String::from_utf8(out).unwrap()), 400);
        });
    }

    /// `reports` lists only top-level `*.md`/`*.markdown` bundles (skipping dirs,
    /// dotfiles, other extensions) as the bare `outbox/<name>` the viewer resolves;
    /// `intent` writes a server-named bundle on non-empty content and refuses 400 on empty.
    #[test]
    fn reports_lists_markdown_bundles_and_intent_writes_a_named_one() {
        let dir = tempfile::tempdir().unwrap();
        with_roots(dir.path(), || {
            let outbox = dir.path().join("outbox");
            std::fs::write(outbox.join("a.md"), "A").unwrap();
            std::fs::write(outbox.join("b.markdown"), "B").unwrap();
            std::fs::write(outbox.join("skip.txt"), "x").unwrap();
            std::fs::write(outbox.join(".hidden.md"), "h").unwrap();
            std::fs::create_dir_all(outbox.join("_archive")).unwrap();

            let mut out = Vec::new();
            reports(&mut out);
            let resp = String::from_utf8(out).unwrap();
            assert_eq!(status_code(&resp), 200, "{resp}");
            let listed = body(&resp);
            assert!(listed.contains(r#""outbox/a.md""#), "{listed}");
            assert!(listed.contains(r#""outbox/b.markdown""#), "{listed}");
            assert!(!listed.contains("skip.txt"), "non-markdown skipped: {listed}");
            assert!(!listed.contains("_archive"), "the archive dir is skipped: {listed}");
            assert!(!listed.contains(".hidden"), "dotfiles are skipped: {listed}");

            // Non-empty content → 200 + a server-clock `intent-<ts>.md` name.
            let mut out = Vec::new();
            intent(&mut out, br##"{"content":"# Grille\n\nGiven x"}"##);
            let resp = String::from_utf8(out).unwrap();
            assert_eq!(status_code(&resp), 200, "{resp}");
            let w = body(&resp);
            assert!(w.contains(r#""path":"outbox/intent-"#), "{w}");
            assert!(w.contains(r#""name":"intent-"#), "{w}");

            // Empty (or whitespace-only) content is refused 400 and writes nothing.
            let mut out = Vec::new();
            intent(&mut out, br#"{"content":"   "}"#);
            assert_eq!(status_code(&String::from_utf8(out).unwrap()), 400);

            let intents = std::fs::read_dir(&outbox)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("intent-"))
                .count();
            assert_eq!(intents, 1, "only the valid intent lands on disk");
        });
    }
}
