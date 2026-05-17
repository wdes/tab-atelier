use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use jni::JavaVM;
use jni::objects::{JObject, JString};
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, SharedString, VecModel, Weak};

use crate::onboard::parse_onboard_url;

slint::include_modules!();

/// Read the URI the activity was launched with, if any (e.g. via the
/// `taremote://onboard?url=...&token=...` deep link).
/// Read the system clipboard's primary text item via JNI. Returns `None` if
/// the clipboard is empty or doesn't contain a CharSequence.
fn read_clipboard(app: &slint::android::AndroidApp) -> Option<String> {
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };

    let context_class = env.find_class("android/content/Context").ok()?;
    let key = env
        .get_static_field(&context_class, "CLIPBOARD_SERVICE", "Ljava/lang/String;")
        .ok()?
        .l()
        .ok()?;
    let manager = env
        .call_method(
            &activity,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[(&key).into()],
        )
        .ok()?
        .l()
        .ok()?;
    let clip = env
        .call_method(
            &manager,
            "getPrimaryClip",
            "()Landroid/content/ClipData;",
            &[],
        )
        .ok()?
        .l()
        .ok()?;
    if clip.is_null() {
        return None;
    }
    let count = env
        .call_method(&clip, "getItemCount", "()I", &[])
        .ok()?
        .i()
        .ok()?;
    if count < 1 {
        return None;
    }
    let item = env
        .call_method(
            &clip,
            "getItemAt",
            "(I)Landroid/content/ClipData$Item;",
            &[0_i32.into()],
        )
        .ok()?
        .l()
        .ok()?;
    let cs = env
        .call_method(&item, "getText", "()Ljava/lang/CharSequence;", &[])
        .ok()?
        .l()
        .ok()?;
    if cs.is_null() {
        return None;
    }
    let s = env
        .call_method(&cs, "toString", "()Ljava/lang/String;", &[])
        .ok()?
        .l()
        .ok()?;
    let jstr: JString = s.into();
    env.get_string(&jstr).ok().map(|j| j.into())
}

/// Query the system battery level (0–100) via JNI.
///
/// Uses the `ACTION_BATTERY_CHANGED` sticky broadcast rather than
/// `BatteryManager.getIntProperty(BATTERY_PROPERTY_CAPACITY)` — the
/// latter returned bogus values on the user's device. Sticky broadcast
/// is the canonical Android battery API and exposes both `level` and
/// `scale` extras so we can normalise to a percentage.
fn read_battery_level(app: &slint::android::AndroidApp) -> Option<i32> {
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };

    let intent_class = env.find_class("android/content/Intent").ok()?;
    let action = env
        .get_static_field(&intent_class, "ACTION_BATTERY_CHANGED", "Ljava/lang/String;")
        .ok()?
        .l()
        .ok()?;
    let filter_class = env.find_class("android/content/IntentFilter").ok()?;
    let filter = env
        .new_object(&filter_class, "(Ljava/lang/String;)V", &[(&action).into()])
        .ok()?;

    // Passing a null receiver makes `registerReceiver` return the
    // last broadcasted sticky Intent without subscribing to future
    // updates — exactly the snapshot we need to read once per poll.
    let null_receiver = JObject::null();
    let battery_intent = env
        .call_method(
            &activity,
            "registerReceiver",
            "(Landroid/content/BroadcastReceiver;Landroid/content/IntentFilter;)Landroid/content/Intent;",
            &[(&null_receiver).into(), (&filter).into()],
        )
        .ok()?
        .l()
        .ok()?;
    if battery_intent.is_null() {
        return None;
    }

    let level_key = env.new_string("level").ok()?;
    let scale_key = env.new_string("scale").ok()?;
    let level = env
        .call_method(
            &battery_intent,
            "getIntExtra",
            "(Ljava/lang/String;I)I",
            &[(&level_key).into(), (-1_i32).into()],
        )
        .ok()?
        .i()
        .ok()?;
    let scale = env
        .call_method(
            &battery_intent,
            "getIntExtra",
            "(Ljava/lang/String;I)I",
            &[(&scale_key).into(), (-1_i32).into()],
        )
        .ok()?
        .i()
        .ok()?;
    if level < 0 || scale <= 0 {
        return None;
    }
    let pct = (level as f64 * 100.0 / scale as f64).round() as i32;
    Some(pct.clamp(0, 100))
}

fn launch_intent_uri(app: &slint::android::AndroidApp) -> Option<String> {
    let vm_ptr = app.vm_as_ptr();
    let activity_ptr = app.activity_as_ptr();
    if vm_ptr.is_null() || activity_ptr.is_null() {
        return None;
    }
    let vm = unsafe { JavaVM::from_raw(vm_ptr.cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let activity = unsafe { JObject::from_raw(activity_ptr.cast()) };
    let intent = env
        .call_method(&activity, "getIntent", "()Landroid/content/Intent;", &[])
        .ok()?
        .l()
        .ok()?;
    let uri = env
        .call_method(&intent, "getData", "()Landroid/net/Uri;", &[])
        .ok()?
        .l()
        .ok()?;
    if uri.is_null() {
        return None;
    }
    let s = env
        .call_method(&uri, "toString", "()Ljava/lang/String;", &[])
        .ok()?
        .l()
        .ok()?;
    let jstr: JString = s.into();
    env.get_string(&jstr).ok().map(|j| j.into())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostConfig {
    name: String,
    url: String,
    #[serde(default)]
    remote_url: String,
    token: String,
}

/// Outcome of an HTTP request that may have been tried against the LAN URL
/// and/or the remote URL of a host.
#[derive(Debug, Clone, Copy)]
enum Reach {
    Lan,
    Remote,
    Offline,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoredConfig {
    hosts: Vec<HostConfig>,
    active: usize,
}

#[derive(Debug, Deserialize)]
struct ApiTab {
    name: String,
    cwd: Option<String>,
    active: bool,
    #[serde(default)]
    cpu_percent: f64,
    #[serde(default)]
    watts: Option<f64>,
    #[serde(default)]
    preview: String,
    #[serde(default)]
    uptime_secs: f64,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    tabs: Vec<ApiTab>,
}

struct AppData {
    hosts: Vec<HostConfig>,
    active: usize,
    config_path: PathBuf,
}

impl AppData {
    fn load(config_path: PathBuf) -> Self {
        let stored = Self::read_stored(&config_path)
            .or_else(|| Self::read_stored(&config_path.with_extension("json.bak")))
            .or_else(|| Self::read_stored(&config_path.with_extension("json.bak.1")))
            .unwrap_or_default();
        Self {
            hosts: stored.hosts,
            active: stored.active,
            config_path,
        }
    }

    fn read_stored(path: &std::path::Path) -> Option<StoredConfig> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Atomically persist host config with a 2-generation rotation
    /// (`hosts.json`, `hosts.json.bak`, `hosts.json.bak.1`) — same approach
    /// as the desktop `save_state`, defensive against partial writes.
    fn save(&self) {
        use std::io::Write;
        let stored = StoredConfig {
            hosts: self.hosts.clone(),
            active: self.active,
        };
        let Ok(text) = serde_json::to_string_pretty(&stored) else { return };
        let Some(parent) = self.config_path.parent() else { return };
        let _ = std::fs::create_dir_all(parent);

        let tmp = self.config_path.with_extension("json.tmp");
        let Ok(mut f) = std::fs::File::create(&tmp) else { return };
        if f.write_all(text.as_bytes()).is_err() || f.sync_all().is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        drop(f);

        if self.config_path.exists() {
            let bak = self.config_path.with_extension("json.bak");
            let bak1 = self.config_path.with_extension("json.bak.1");
            let _ = std::fs::rename(&bak, &bak1);
            let _ = std::fs::rename(&self.config_path, &bak);
        }
        let _ = std::fs::rename(&tmp, &self.config_path);
    }

    fn active_host(&self) -> Option<HostConfig> {
        self.hosts.get(self.active).cloned()
    }
}

fn fetch_output(
    agent: &ureq::Agent,
    host: &HostConfig,
    reach: &Reach,
    idx: i32,
) -> Option<String> {
    let base = base_url(host, reach);
    if base.is_empty() {
        return None;
    }
    let resp = agent
        .get(&format!("{base}/tabs/{idx}/output"))
        .set("Authorization", &format!("Bearer {}", host.token))
        .timeout(Duration::from_millis(1500))
        .call()
        .ok()?;
    resp.into_string().ok()
}

fn push_output(ui_weak: &Weak<AppWindow>, text: String) {
    // Parsing runs on whatever thread called us, but the Slint structs
    // (`ColorLine` carries a `ModelRc` which is `!Send`) must be built
    // on the UI thread.
    let lines = parse_ansi(&text);
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        let model: Vec<ColorLine> = lines
            .into_iter()
            .map(|spans| {
                let span_models: Vec<ColorSpan> = spans
                    .into_iter()
                    .map(|s| ColorSpan {
                        text: SharedString::from(s.text),
                        color: slint::Color::from_rgb_u8(s.rgb[0], s.rgb[1], s.rgb[2]),
                        bold: s.bold,
                    })
                    .collect();
                ColorLine {
                    spans: std::rc::Rc::new(slint::VecModel::from(span_models)).into(),
                }
            })
            .collect();
        ui.set_open_tab_output_lines(VecModel::from_slice(&model));
    });
}

struct ParsedSpan {
    text: String,
    rgb: [u8; 3],
    bold: bool,
}

/// Default foreground colour for the terminal view — keep in sync with
/// the old `#d0d0d0` used by the plain-text Text element.
const DEFAULT_FG: [u8; 3] = [0xd0, 0xd0, 0xd0];

/// Parse `text` (lines separated by '\n') as a sequence of rows where each
/// row is a vector of single-colour runs. Recognises CSI SGR sequences
/// (reset, bold, 8 fg, bright fg, 256 fg, 24-bit fg); other CSI sequences
/// are silently consumed so they don't appear as garbage in the output.
fn parse_ansi(text: &str) -> Vec<Vec<ParsedSpan>> {
    let mut lines = Vec::new();
    let mut cur_color = DEFAULT_FG;
    let mut cur_bold = false;

    for raw_line in text.split('\n') {
        let mut spans: Vec<ParsedSpan> = Vec::new();
        let mut buf = String::new();
        let mut chars = raw_line.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                let mut params = String::new();
                let mut final_byte = 0u8;
                for nc in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&nc) {
                        final_byte = nc as u8;
                        break;
                    }
                    params.push(nc);
                }
                if final_byte == b'm' {
                    if !buf.is_empty() {
                        spans.push(ParsedSpan {
                            text: std::mem::take(&mut buf),
                            rgb: cur_color,
                            bold: cur_bold,
                        });
                    }
                    apply_sgr(&params, &mut cur_color, &mut cur_bold);
                }
            } else {
                buf.push(c);
            }
        }
        if !buf.is_empty() {
            spans.push(ParsedSpan {
                text: buf,
                rgb: cur_color,
                bold: cur_bold,
            });
        }
        // Empty rows still need a placeholder span so the VerticalLayout
        // reserves a row of vertical space — otherwise blank shell lines
        // would collapse the gap above/below them.
        if spans.is_empty() {
            spans.push(ParsedSpan {
                text: " ".to_string(),
                rgb: cur_color,
                bold: cur_bold,
            });
        }
        lines.push(spans);
    }
    lines
}

fn apply_sgr(params: &str, cur: &mut [u8; 3], bold: &mut bool) {
    if params.is_empty() {
        *cur = DEFAULT_FG;
        *bold = false;
        return;
    }
    let parts: Vec<u32> = params.split(';').filter_map(|s| s.parse().ok()).collect();
    let mut i = 0;
    while i < parts.len() {
        let code = parts[i];
        match code {
            0 => {
                *cur = DEFAULT_FG;
                *bold = false;
            }
            1 => *bold = true,
            22 => *bold = false,
            30..=37 => *cur = ansi16((code - 30) as u8),
            38 => {
                if i + 1 < parts.len() && parts[i + 1] == 5 && i + 2 < parts.len() {
                    *cur = ansi256(parts[i + 2] as u8);
                    i += 2;
                } else if i + 4 < parts.len() && parts[i + 1] == 2 {
                    *cur = [
                        parts[i + 2] as u8,
                        parts[i + 3] as u8,
                        parts[i + 4] as u8,
                    ];
                    i += 4;
                }
            }
            39 => *cur = DEFAULT_FG,
            90..=97 => *cur = ansi16((code - 90 + 8) as u8),
            _ => {}
        }
        i += 1;
    }
}

const ANSI_PALETTE: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00],
    [0xcd, 0x00, 0x00],
    [0x00, 0xcd, 0x00],
    [0xcd, 0xcd, 0x00],
    [0x40, 0x80, 0xff], // blue — brightened from #0000ee for readability on dark bg
    [0xcd, 0x00, 0xcd],
    [0x00, 0xcd, 0xcd],
    [0xe5, 0xe5, 0xe5],
    [0x7f, 0x7f, 0x7f],
    [0xff, 0x40, 0x40],
    [0x40, 0xff, 0x40],
    [0xff, 0xff, 0x40],
    [0x80, 0xa0, 0xff],
    [0xff, 0x40, 0xff],
    [0x40, 0xff, 0xff],
    [0xff, 0xff, 0xff],
];

fn ansi16(idx: u8) -> [u8; 3] {
    ANSI_PALETTE[(idx as usize) % 16]
}

fn ansi256(idx: u8) -> [u8; 3] {
    if idx < 16 {
        ansi16(idx)
    } else if idx < 232 {
        let i = idx - 16;
        let r = i / 36;
        let g = (i % 36) / 6;
        let b = i % 6;
        let to_val = |v: u8| if v == 0 { 0u8 } else { 55 + v * 40 };
        [to_val(r), to_val(g), to_val(b)]
    } else {
        let v = 8u8.saturating_add((idx - 232).saturating_mul(10));
        [v, v, v]
    }
}

fn fetch_tabs(
    agent: &ureq::Agent,
    host: &HostConfig,
) -> (Reach, Option<Vec<ApiTab>>) {
    if !host.url.is_empty()
        && let Ok(resp) = agent
            .get(&format!("{}/tabs", host.url))
            .set("Authorization", &format!("Bearer {}", host.token))
            .timeout(Duration::from_millis(1500))
            .call()
        && let Ok(r) = resp.into_json::<ApiResponse>()
    {
        return (Reach::Lan, Some(r.tabs));
    }
    if !host.remote_url.is_empty()
        && let Ok(resp) = agent
            .get(&format!("{}/tabs", host.remote_url))
            .set("Authorization", &format!("Bearer {}", host.token))
            .timeout(Duration::from_secs(4))
            .call()
        && let Ok(r) = resp.into_json::<ApiResponse>()
    {
        return (Reach::Remote, Some(r.tabs));
    }
    (Reach::Offline, None)
}

fn base_url(host: &HostConfig, reach: &Reach) -> String {
    match reach {
        Reach::Remote => host.remote_url.clone(),
        _ => host.url.clone(),
    }
}

fn post_input(agent: &ureq::Agent, host: &HostConfig, reach: &Reach, idx: i32, bytes: &[u8]) {
    let base = base_url(host, reach);
    if base.is_empty() {
        return;
    }
    let url = format!("{base}/tabs/{idx}/input");
    if let Err(e) = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", host.token))
        .set("Content-Type", "application/octet-stream")
        .timeout(Duration::from_secs(2))
        .send_bytes(bytes)
    {
        log::warn!("post_input failed: {e}");
    }
}

fn post_rename_tab(agent: &ureq::Agent, host: &HostConfig, reach: &Reach, idx: i32, name: &str) {
    let base = base_url(host, reach);
    if base.is_empty() {
        return;
    }
    let body = serde_json::json!({ "name": name }).to_string();
    let url = format!("{base}/tabs/{idx}/rename");
    if let Err(e) = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", host.token))
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(2))
        .send_string(&body)
    {
        log::warn!("post_rename_tab failed: {e}");
    }
}

fn post_new_tab(agent: &ureq::Agent, host: &HostConfig, reach: &Reach) {
    let base = base_url(host, reach);
    if base.is_empty() {
        return;
    }
    let url = format!("{base}/tabs");
    if let Err(e) = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", host.token))
        .timeout(Duration::from_secs(2))
        .send_string("")
    {
        log::warn!("post_new_tab failed: {e}");
    }
}

fn delete_tab(agent: &ureq::Agent, host: &HostConfig, reach: &Reach, idx: i32) {
    let base = base_url(host, reach);
    if base.is_empty() {
        return;
    }
    let url = format!("{base}/tabs/{idx}");
    if let Err(e) = agent
        .delete(&url)
        .set("Authorization", &format!("Bearer {}", host.token))
        .timeout(Duration::from_secs(2))
        .call()
    {
        log::warn!("delete_tab failed: {e}");
    }
}

fn post_activate(agent: &ureq::Agent, host: &HostConfig, reach: &Reach, idx: i32) {
    let base = base_url(host, reach);
    if base.is_empty() {
        return;
    }
    let url = format!("{base}/tabs/{idx}/activate");
    if let Err(e) = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", host.token))
        .timeout(Duration::from_secs(2))
        .send_string("")
    {
        log::warn!("post_activate failed: {e}");
    }
}

/// Fold sticky CTRL / ALT into a typed UTF-8 string.
///
/// CTRL maps the *first* ASCII letter (case-insensitive) to its Ctrl
/// equivalent (Ctrl-A → 0x01, Ctrl-? → 0x7f for `?`/`/`), then the
/// remaining bytes are appended verbatim. ALT prepends an ESC byte
/// (the standard meta-encoding) to the *first* code point and leaves
/// the rest untouched. If both are set, ALT wraps CTRL.
fn apply_modifiers(text: &str, ctrl: bool, alt: bool) -> Vec<u8> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut iter = text.chars();
    let Some(first) = iter.next() else { return Vec::new() };
    let rest: String = iter.collect();

    let first_bytes: Vec<u8> = if ctrl {
        match first {
            'a'..='z' => vec![(first as u8) - b'a' + 1],
            'A'..='Z' => vec![(first as u8) - b'A' + 1],
            // Ctrl-? / Ctrl-/ → DEL (0x7f), Ctrl-Space → NUL,
            // anything else falls through as-is so the user still sees
            // their key arrive at the terminal.
            '?' | '/' => vec![0x7f],
            ' ' => vec![0x00],
            other => other.to_string().into_bytes(),
        }
    } else {
        first.to_string().into_bytes()
    };

    let mut out = Vec::with_capacity(first_bytes.len() + rest.len() + 1);
    if alt {
        out.push(0x1b);
    }
    out.extend_from_slice(&first_bytes);
    out.extend_from_slice(rest.as_bytes());
    out
}

/// Compact "Xh Ym" / "Xm Ys" / "Xs" uptime string — matches the visual
/// vocabulary the desktop uses in its tab headers so the phone counter
/// reads identically.
fn format_uptime(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    if total >= 3600 {
        format!("{}h {:02}m", total / 3600, (total % 3600) / 60)
    } else if total >= 60 {
        format!("{}m {:02}s", total / 60, total % 60)
    } else {
        format!("{total}s")
    }
}

/// Per-tab CRC32 of the last preview we showed the user. A row gets a
/// "new output" dot whenever the current preview hashes differently from
/// the stored value, *unless* this is the first poll for that tab (in
/// which case we just record the baseline silently).
type SeenPreviews = Arc<Mutex<HashMap<String, u32>>>;

/// Small inline CRC32 (IEEE) — same polynomial as the desktop side's
/// helper. Used to fingerprint tab previews for the "new output" dot.
fn crc32(data: &[u8]) -> u32 {
    const POLY: u32 = 0xEDB8_8320;
    let mut crc: u32 = !0;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (POLY & mask);
        }
    }
    !crc
}

fn push_tabs(ui_weak: &Weak<AppWindow>, tabs: Vec<ApiTab>, seen: &SeenPreviews) {
    let rows: Vec<TabRow> = {
        let mut seen_guard = seen.lock().unwrap();
        tabs.into_iter()
            .map(|t| {
                let preview_hash = crc32(t.preview.as_bytes());
                let has_new = match seen_guard.get(&t.name) {
                    None => false, // first sighting, seed silently
                    Some(&prev) => prev != preview_hash,
                };
                seen_guard.entry(t.name.clone()).or_insert(preview_hash);
                TabRow {
                    name: SharedString::from(t.name.clone()),
                    cwd: SharedString::from(t.cwd.unwrap_or_default()),
                    active: t.active,
                    cpu: SharedString::from(match t.watts {
                        // Mirror wattaouille's two-decimal `0.43 W` style;
                        // CPU % shown without watts when RAPL is unavailable.
                        Some(w) if w >= 0.005 => format!("{:.1}% · {:.2}W", t.cpu_percent, w),
                        Some(w) if w > 0.0 => format!("{:.1}% · {:.0}mW", t.cpu_percent, w * 1000.0),
                        _ => format!("{:.1}%", t.cpu_percent),
                    }),
                    preview: SharedString::from(t.preview),
                    uptime: SharedString::from(format_uptime(t.uptime_secs)),
                    has_new,
                }
            })
            .collect()
    };
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_tabs(VecModel::from_slice(&rows));
        }
    });
}

/// After a mutating API call (new tab / close / rename / activate),
/// trigger a one-shot /tabs refresh ~250 ms later so the user sees the
/// change reflected without waiting for the next 2 s poll tick.
fn refresh_soon(
    ui_weak: &Weak<AppWindow>,
    agent: &Arc<ureq::Agent>,
    data: &Arc<Mutex<AppData>>,
    reach: &Arc<Mutex<Reach>>,
    seen: &SeenPreviews,
) {
    let weak = ui_weak.clone();
    let agent = agent.clone();
    let data = data.clone();
    let reach = reach.clone();
    let seen = seen.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        let Some(host) = data.lock().unwrap().active_host() else { return };
        let (new_reach, tabs) = fetch_tabs(&agent, &host);
        if let Some(t) = tabs {
            push_tabs(&weak, t, &seen);
        }
        *reach.lock().unwrap() = new_reach;
    });
}

/// Mark the given tab's current preview as "seen", clearing the dot the
/// next time push_tabs runs.
fn mark_seen(seen: &SeenPreviews, name: &str, preview: &str) {
    let hash = crc32(preview.as_bytes());
    seen.lock().unwrap().insert(name.to_string(), hash);
}

fn push_hosts(ui_weak: &Weak<AppWindow>, data: &AppData) {
    let rows: Vec<Host> = data
        .hosts
        .iter()
        .map(|h| Host {
            name: SharedString::from(h.name.as_str()),
            // Start as "connecting" — the poller flips this to lan/remote/offline
            // after the first attempt completes, so newly-added hosts don't
            // look broken for the first 2 seconds.
            reachability: SharedString::from("connecting"),
            detail: SharedString::from(host_detail(&h.url)),
            url: SharedString::from(h.url.as_str()),
            remote_url: SharedString::from(h.remote_url.as_str()),
            token: SharedString::from(h.token.as_str()),
        })
        .collect();
    let active = data.active as i32;
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_hosts(VecModel::from_slice(&rows));
            ui.set_active_host(active);
        }
    });
}

fn host_detail(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string()
}

fn show_toast(ui_weak: &Weak<AppWindow>, msg: String) {
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_toast_text(SharedString::from(msg));
            ui.set_toast_visible(true);
        }
    });
}

fn push_reachability(ui_weak: &Weak<AppWindow>, active: usize, reach: Reach) {
    let label = match reach {
        Reach::Lan => "lan",
        Reach::Remote => "remote",
        Reach::Offline => "offline",
    };
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let model = ui.get_hosts();
            let mut list: Vec<Host> = (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .collect();
            if let Some(h) = list.get_mut(active) {
                h.reachability = SharedString::from(label);
            }
            ui.set_hosts(VecModel::from_slice(&list));
        }
    });
}

#[unsafe(no_mangle)]
pub fn android_main(app: slint::android::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
    );

    let data_dir = app
        .internal_data_path()
        .unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    let config_path = data_dir.join("hosts.json");
    log::info!("ta-remote config path: {}", config_path.display());

    // Keep an AndroidApp clone for JNI callbacks (e.g. scan-QR) before
    // slint::android::init takes ownership of the original.
    let app_for_callbacks = app.clone();
    let app_for_battery = app.clone();

    let launch_onboard = launch_intent_uri(&app).and_then(|u| parse_onboard_url(&u));

    slint::android::init(app).unwrap();
    let ui = AppWindow::new().unwrap();
    let ui_weak = ui.as_weak();

    let data = Arc::new(Mutex::new(AppData::load(config_path)));
    push_hosts(&ui_weak, &data.lock().unwrap());

    // Pre-fill the host editor from a launch deep link, if any.
    if let Some((host_url, token)) = launch_onboard {
        log::info!("launched with onboard deep link for {host_url}");
        ui.set_editor_url(SharedString::from(host_url));
        ui.set_editor_token(SharedString::from(token));
        ui.set_editor_open(true);
    }

    let agent: Arc<ureq::Agent> = Arc::new(
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(5))
            .build(),
    );

    let last_reach: Arc<Mutex<Reach>> = Arc::new(Mutex::new(Reach::Offline));
    let seen_previews: SeenPreviews = Arc::new(Mutex::new(HashMap::new()));
    let open_tab: Arc<AtomicI32> = Arc::new(AtomicI32::new(-1));

    // Background poller
    let poll_weak = ui_weak.clone();
    let poll_agent = agent.clone();
    let poll_data = data.clone();
    let poll_reach = last_reach.clone();
    let poll_open = open_tab.clone();
    let poll_seen = seen_previews.clone();
    std::thread::spawn(move || loop {
        let (host, active_idx) = {
            let d = poll_data.lock().unwrap();
            (d.active_host(), d.active)
        };
        if let Some(host) = host {
            let (reach, tabs) = fetch_tabs(&poll_agent, &host);
            if let Some(t) = tabs {
                push_tabs(&poll_weak, t, &poll_seen);
            }
            log::debug!("poll {}: {reach:?}", host.name);
            // Show a toast when reachability transitions between online and
            // offline so a connection drop doesn't go unnoticed.
            {
                let mut last = poll_reach.lock().unwrap();
                let was_online = matches!(*last, Reach::Lan | Reach::Remote);
                let is_online = matches!(reach, Reach::Lan | Reach::Remote);
                if was_online && !is_online {
                    show_toast(&poll_weak, format!("Disconnected from {}", host.name));
                } else if !was_online && is_online {
                    let via = if matches!(reach, Reach::Lan) { "LAN" } else { "remote" };
                    show_toast(&poll_weak, format!("Connected to {} via {via}", host.name));
                }
                *last = reach;
            }
            push_reachability(&poll_weak, active_idx, reach);

            let idx = poll_open.load(Ordering::Relaxed);
            if idx >= 0
                && let Some(text) = fetch_output(&poll_agent, &host, &reach, idx)
            {
                push_output(&poll_weak, text);
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    });

    let act_agent = agent.clone();
    let act_data = data.clone();
    let act_reach = last_reach.clone();
    ui.on_request_activate(move |idx| {
        let Some(host) = act_data.lock().unwrap().active_host() else { return };
        let reach = *act_reach.lock().unwrap();
        let agent = act_agent.clone();
        std::thread::spawn(move || post_activate(&agent, &host, &reach, idx));
    });

    let send_agent = agent.clone();
    let send_data = data.clone();
    let send_reach = last_reach.clone();
    ui.on_request_send_input(move |idx, text| {
        let Some(host) = send_data.lock().unwrap().active_host() else { return };
        let reach = *send_reach.lock().unwrap();
        let agent = send_agent.clone();
        let bytes = text.as_bytes().to_vec();
        std::thread::spawn(move || post_input(&agent, &host, &reach, idx, &bytes));
    });

    let typed_agent = agent.clone();
    let typed_data = data.clone();
    let typed_reach = last_reach.clone();
    ui.on_request_typed_text(move |idx, text, ctrl, alt| {
        let Some(host) = typed_data.lock().unwrap().active_host() else { return };
        let reach = *typed_reach.lock().unwrap();
        let agent = typed_agent.clone();
        let bytes = apply_modifiers(text.as_str(), ctrl, alt);
        std::thread::spawn(move || post_input(&agent, &host, &reach, idx, &bytes));
    });

    let close_agent = agent.clone();
    let close_data = data.clone();
    let close_reach = last_reach.clone();
    let close_weak = ui_weak.clone();
    let close_seen = seen_previews.clone();
    ui.on_request_close_tab(move |idx| {
        let Some(host) = close_data.lock().unwrap().active_host() else { return };
        let reach = *close_reach.lock().unwrap();
        let agent = close_agent.clone();
        std::thread::spawn(move || delete_tab(&agent, &host, &reach, idx));
        refresh_soon(&close_weak, &close_agent, &close_data, &close_reach, &close_seen);
    });

    let new_agent = agent.clone();
    let new_data = data.clone();
    let new_reach = last_reach.clone();
    let new_weak = ui_weak.clone();
    let new_seen = seen_previews.clone();
    ui.on_request_new_tab(move || {
        let Some(host) = new_data.lock().unwrap().active_host() else { return };
        let reach = *new_reach.lock().unwrap();
        let agent = new_agent.clone();
        std::thread::spawn(move || post_new_tab(&agent, &host, &reach));
        refresh_soon(&new_weak, &new_agent, &new_data, &new_reach, &new_seen);
    });

    let rename_agent = agent.clone();
    let rename_data = data.clone();
    let rename_reach = last_reach.clone();
    let rename_weak = ui_weak.clone();
    let rename_seen = seen_previews.clone();
    ui.on_request_rename_tab(move |idx, name| {
        let Some(host) = rename_data.lock().unwrap().active_host() else { return };
        let reach = *rename_reach.lock().unwrap();
        let agent = rename_agent.clone();
        let name = name.to_string();
        std::thread::spawn(move || post_rename_tab(&agent, &host, &reach, idx, &name));
        refresh_soon(&rename_weak, &rename_agent, &rename_data, &rename_reach, &rename_seen);
    });

    let open_tab_for_cb = open_tab.clone();
    let open_weak = ui_weak.clone();
    let open_data = data.clone();
    let open_reach = last_reach.clone();
    let open_agent = agent.clone();
    let open_seen = seen_previews.clone();
    ui.on_open_tab_changed(move |idx| {
        open_tab_for_cb.store(idx, Ordering::Relaxed);
        if idx < 0 {
            push_output(&open_weak, String::new());
            return;
        }
        // Mark this tab's current preview as seen so the green "new
        // output" dot clears on the next poll's tabs refresh.
        if let Some(ui) = open_weak.upgrade()
            && let Some(row) = ui.get_tabs().row_data(idx as usize)
        {
            mark_seen(&open_seen, &row.name, &row.preview);
        }
        // Fire an immediate fetch so the view isn't blank for up to 2s.
        let host = open_data.lock().unwrap().active_host();
        let reach = *open_reach.lock().unwrap();
        if let Some(host) = host {
            let agent = open_agent.clone();
            let weak = open_weak.clone();
            std::thread::spawn(move || {
                if let Some(text) = fetch_output(&agent, &host, &reach, idx) {
                    push_output(&weak, text);
                }
            });
        }
    });

    let set_data = data.clone();
    ui.on_request_set_active_host(move |idx| {
        let mut data = set_data.lock().unwrap();
        if (idx as usize) < data.hosts.len() {
            data.active = idx as usize;
            data.save();
        }
    });

    let rm_data = data.clone();
    let rm_weak = ui_weak.clone();
    ui.on_request_remove_host(move |idx| {
        let mut data = rm_data.lock().unwrap();
        let idx = idx as usize;
        if idx < data.hosts.len() {
            data.hosts.remove(idx);
            if data.active >= data.hosts.len() && !data.hosts.is_empty() {
                data.active = data.hosts.len() - 1;
            } else if data.hosts.is_empty() {
                data.active = 0;
            }
            data.save();
            push_hosts(&rm_weak, &data);
        }
    });

    let add_data = data.clone();
    let add_weak = ui_weak.clone();
    ui.on_request_add_host(move |name, url, remote_url, token| {
        let name = name.to_string();
        let url = url.trim_end_matches('/').to_string();
        let remote_url = remote_url.trim_end_matches('/').to_string();
        let token = token.to_string();
        if url.is_empty() || token.is_empty() {
            return;
        }
        let mut data = add_data.lock().unwrap();
        let new_idx = data.hosts.len();
        data.hosts.push(HostConfig {
            name: if name.is_empty() { host_detail(&url) } else { name },
            url,
            remote_url,
            token,
        });
        data.active = new_idx;
        data.save();
        push_hosts(&add_weak, &data);
    });

    let upd_data = data.clone();
    let upd_weak = ui_weak.clone();
    ui.on_request_update_host(move |idx, name, url, remote_url, token| {
        let idx = idx as usize;
        let url = url.trim_end_matches('/').to_string();
        let remote_url = remote_url.trim_end_matches('/').to_string();
        let token = token.to_string();
        if url.is_empty() || token.is_empty() {
            return;
        }
        let name = {
            let name = name.to_string();
            if name.is_empty() { host_detail(&url) } else { name }
        };
        let mut data = upd_data.lock().unwrap();
        if let Some(h) = data.hosts.get_mut(idx) {
            h.name = name;
            h.url = url;
            h.remote_url = remote_url;
            h.token = token;
            data.save();
            push_hosts(&upd_weak, &data);
        }
    });

    let scan_weak = ui_weak.clone();
    ui.on_request_scan_qr(move || {
        // The Android intents for "open camera" only invoke the photo
        // viewfinder on many devices — they don't run the QR detector. So
        // instead of guessing which camera launcher might scan QRs, we
        // read the clipboard: the user copies the `taremote://onboard…`
        // URL from the desktop QR dialog, hits this button, and the
        // editor opens pre-filled. Falls back gracefully if the clipboard
        // doesn't contain a recognisable onboard URL.
        let pasted = read_clipboard(&app_for_callbacks).unwrap_or_default();
        let parsed = pasted
            .lines()
            .find(|l| l.trim_start().starts_with("taremote://onboard?"))
            .and_then(|l| crate::onboard::parse_onboard_url(l.trim()));
        let weak = scan_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_editor_edit_index(-1);
            ui.set_editor_name(SharedString::new());
            ui.set_editor_remote_url(SharedString::new());
            match parsed {
                Some((url, token)) => {
                    ui.set_editor_url(SharedString::from(url));
                    ui.set_editor_token(SharedString::from(token));
                    ui.set_editor_error(SharedString::new());
                }
                None => {
                    ui.set_editor_url(SharedString::new());
                    ui.set_editor_token(SharedString::new());
                    ui.set_editor_error(SharedString::from(
                        "Clipboard didn't contain a taremote:// URL. Copy the URL under the QR code on the desktop and try again.",
                    ));
                }
            }
            ui.set_editor_open(true);
        });
    });

    // Battery poller — refreshes every 30 s. Slint side reads
    // `battery-level` (0–100, or -1 when unavailable) and turns the top
    // bar red/blinking when the phone is below 20 % / 10 %.
    {
        let weak = ui_weak.clone();
        std::thread::spawn(move || {
            loop {
                let level = read_battery_level(&app_for_battery).unwrap_or(-1);
                let w = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        ui.set_battery_level(level);
                    }
                });
                std::thread::sleep(Duration::from_secs(30));
            }
        });
    }

    ui.run().unwrap();
}
