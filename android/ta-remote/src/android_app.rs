use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use jni::JavaVM;
use jni::objects::{JObject, JString, JValue};
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, SharedString, VecModel, Weak};

use crate::onboard::{host_detail, parse_onboard_url};

slint::include_modules!();

/// Read the URI the activity was launched with, if any (e.g. via the
/// `taremote://onboard?url=...&token=...` deep link).
/// Read the system clipboard's primary text item via JNI. Returns `None` if
/// the clipboard is empty or doesn't contain a `CharSequence`.
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
        .call_method(&manager, "getPrimaryClip", "()Landroid/content/ClipData;", &[])
        .ok()?
        .l()
        .ok()?;
    if clip.is_null() {
        return None;
    }
    let count = env.call_method(&clip, "getItemCount", "()I", &[]).ok()?.i().ok()?;
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
    env.get_string(&jstr).ok().map(Into::into)
}

/// Load `name` (Java FQCN, dotted: `fr.wdes.tab_atelier.WebViewHost`)
/// via the *activity's* `ClassLoader` rather than `JNIEnv::find_class`.
/// `find_class` uses the system class loader from a Rust-spawned thread
/// and ART aborts the process with `ClassNotFoundException` for any
/// non-system class — including ours in classes.dex.
fn load_app_class<'local>(
    env: &mut jni::JNIEnv<'local>,
    activity: &JObject<'local>,
    name: &str,
) -> Result<jni::objects::JClass<'local>, String> {
    // Activity inherits `getClassLoader()` from Context — call it on
    // the activity INSTANCE, not its Class object. `Class.getClassLoader`
    // exists too but returns the loader of `Class` itself (the boot
    // loader), which doesn't have our app's dex on its path.
    let loader_result = env.call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[]);
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    let loader = loader_result
        .map_err(|e| format!("activity.getClassLoader: {e}"))?
        .l()
        .map_err(|e| format!("getClassLoader result: {e}"))?;
    let name_jstr = env
        .new_string(name)
        .map_err(|e| format!("new_string class name: {e}"))?;
    let load_result = env.call_method(
        &loader,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[JValue::Object(&JObject::from(name_jstr))],
    );
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    let cls_obj = load_result
        .map_err(|e| format!("ClassLoader.loadClass({name}): {e}"))?
        .l()
        .map_err(|e| format!("loadClass result: {e}"))?;
    Ok(cls_obj.into())
}

/// Mount the in-app `WebViewHost` (a static Java helper compiled
/// into classes.dex via cargo-apk2's `java_sources`) at the activity
/// root, pointed at the xterm.js share-viewer URL. Bypasses TLS
/// validation on the WebViewClient — the bearer token in the URL is
/// the authn material, and the share host's TLS cert is typically
/// self-signed.
fn show_webview(app: &slint::android::AndroidApp, url: &str) -> Result<(), String> {
    let vm_ptr = app.vm_as_ptr();
    let activity_ptr = app.activity_as_ptr();
    if vm_ptr.is_null() || activity_ptr.is_null() {
        return Err("null vm/activity pointer".into());
    }
    let vm = unsafe { JavaVM::from_raw(vm_ptr.cast()) }.map_err(|e| e.to_string())?;
    let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
    let activity = unsafe { JObject::from_raw(activity_ptr.cast()) };
    let host_cls = load_app_class(&mut env, &activity, "fr.wdes.tab_atelier.WebViewHost")?;
    let url_jstr = env.new_string(url).map_err(|e| e.to_string())?;
    env.call_static_method(
        &host_cls,
        "show",
        "(Landroid/app/Activity;Ljava/lang/String;)V",
        &[JValue::Object(&activity), JValue::Object(&JObject::from(url_jstr))],
    )
    .map_err(|e| format!("WebViewHost.show: {e}"))?;
    Ok(())
}

/// Dismiss the currently-mounted WebView, if any. Returns true if a
/// WebView was active (caller uses this from the Back-key handler
/// to know whether to consume the press).
fn dismiss_webview(app: &slint::android::AndroidApp) -> bool {
    let vm_ptr = app.vm_as_ptr();
    let activity_ptr = app.activity_as_ptr();
    if vm_ptr.is_null() || activity_ptr.is_null() {
        return false;
    }
    let Ok(vm) = (unsafe { JavaVM::from_raw(vm_ptr.cast()) }) else {
        return false;
    };
    let Ok(mut env) = vm.attach_current_thread() else {
        return false;
    };
    let activity = unsafe { JObject::from_raw(activity_ptr.cast()) };
    let Ok(host_cls) = load_app_class(&mut env, &activity, "fr.wdes.tab_atelier.WebViewHost") else {
        return false;
    };
    env.call_static_method(
        &host_cls,
        "dismiss",
        "(Landroid/app/Activity;)Z",
        &[JValue::Object(&activity)],
    )
    .and_then(|v| v.z())
    .unwrap_or(false)
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
    env.get_string(&jstr).ok().map(Into::into)
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
///
/// `Forbidden` is reached when at least one of the two URLs answered
/// with HTTP 401 / 403 — the host *is* online, the saved bearer token
/// just no longer matches. Distinguishing it from `Offline` lets the
/// UI surface "online but access forbidden" instead of "offline" so
/// the user knows to re-pair the host instead of debugging Wi-Fi.
#[derive(Debug, Clone, Copy)]
enum Reach {
    Lan,
    Remote,
    Forbidden,
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

#[derive(Debug, Deserialize, Default)]
struct ApiHost {
    #[serde(default)]
    battery_percent: Option<u8>,
    #[serde(default)]
    watts: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    #[serde(default)]
    host: ApiHost,
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
        let Ok(text) = serde_json::to_string_pretty(&stored) else {
            return;
        };
        let Some(parent) = self.config_path.parent() else {
            return;
        };
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

/// Output payload + cursor logical position, as returned by the host's
/// `/tabs/N/output` endpoint. The cursor headers are optional; absent
/// → `cursor = (-1, -1)` which the Slint side renders as "no cursor".

/// Outcome of one `GET /tabs` attempt — needed because we want to
/// distinguish "no answer at all" (try the next URL) from "answered
/// with 401/403" (host is reachable but token is stale).
enum FetchOutcome {
    Ok(ApiResponse),
    Forbidden,
    NoResponse,
}

fn try_fetch_tabs(agent: &ureq::Agent, base: &str, token: &str, timeout: Duration) -> FetchOutcome {
    let req = agent
        .get(&format!("{base}/tabs"))
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(timeout);
    match req.call() {
        Ok(resp) => resp
            .into_json::<ApiResponse>()
            .map_or(FetchOutcome::NoResponse, FetchOutcome::Ok),
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => FetchOutcome::Forbidden,
        Err(_) => FetchOutcome::NoResponse,
    }
}

/// Result of a host-wide tabs fetch — bundles the per-tab list with
/// the host-wide stats block so callers don't have to issue a second
/// request just to learn the workstation's battery / power draw.
struct TabsFetch {
    reach: Reach,
    tabs: Option<Vec<ApiTab>>,
    host: ApiHost,
}

fn fetch_tabs(agent: &ureq::Agent, host: &HostConfig) -> TabsFetch {
    let mut saw_forbidden = false;
    if !host.url.is_empty() {
        match try_fetch_tabs(agent, &host.url, &host.token, Duration::from_millis(1500)) {
            FetchOutcome::Ok(r) => {
                return TabsFetch {
                    reach: Reach::Lan,
                    tabs: Some(r.tabs),
                    host: r.host,
                };
            }
            FetchOutcome::Forbidden => saw_forbidden = true,
            FetchOutcome::NoResponse => {}
        }
    }
    if !host.remote_url.is_empty() {
        match try_fetch_tabs(agent, &host.remote_url, &host.token, Duration::from_secs(4)) {
            FetchOutcome::Ok(r) => {
                return TabsFetch {
                    reach: Reach::Remote,
                    tabs: Some(r.tabs),
                    host: r.host,
                };
            }
            FetchOutcome::Forbidden => saw_forbidden = true,
            FetchOutcome::NoResponse => {}
        }
    }
    TabsFetch {
        reach: if saw_forbidden {
            Reach::Forbidden
        } else {
            Reach::Offline
        },
        tabs: None,
        host: ApiHost::default(),
    }
}

fn base_url(host: &HostConfig, reach: Reach) -> String {
    match reach {
        Reach::Remote => host.remote_url.clone(),
        _ => host.url.clone(),
    }
}

fn post_input(agent: &ureq::Agent, host: &HostConfig, reach: Reach, idx: i32, bytes: &[u8]) {
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

fn post_rename_tab(agent: &ureq::Agent, host: &HostConfig, reach: Reach, idx: i32, name: &str) {
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

fn post_new_tab(agent: &ureq::Agent, host: &HostConfig, reach: Reach) {
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

fn delete_tab(agent: &ureq::Agent, host: &HostConfig, reach: Reach, idx: i32) {
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

/// Fold sticky CTRL / ALT into a typed UTF-8 string.
///
/// CTRL maps the *first* ASCII letter (case-insensitive) to its Ctrl
/// equivalent (Ctrl-A → 0x01, Ctrl-? → 0x7f for `?`/`/`), then the
/// remaining bytes are appended verbatim. ALT prepends an ESC byte
/// (the standard meta-encoding) to the *first* code point and leaves
/// the rest untouched. If both are set, ALT wraps CTRL.

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
        let Some(host) = data.lock().unwrap().active_host() else {
            return;
        };
        let result = fetch_tabs(&agent, &host);
        if let Some(t) = result.tabs {
            push_tabs(&weak, t, &seen);
        }
        push_host_stats(&weak, &result.host);
        *reach.lock().unwrap() = result.reach;
    });
}

/// After the user sends input, the 2 s background poll is too slow —
/// the screen sits stale for almost half a second on average. Schedule
/// two follow-up fetches at 120 ms and 380 ms so the terminal output
/// catches up to the keystroke well before the next poll tick.

/// Forward the API's `host` block to the Slint side. The UI shows the
/// workstation's battery and total power draw in the header — these
/// are the user's own machine's stats, not the phone's.
fn push_host_stats(ui_weak: &Weak<AppWindow>, host: &ApiHost) {
    let battery = host.battery_percent.map_or(-1_i32, i32::from);
    let watts = host.watts.unwrap_or(0.0);
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_battery_level(battery);
            ui.set_host_watts(watts as f32);
        }
    });
}

/// Mark the given tab's current preview as "seen", clearing the dot the
/// next time `push_tabs` runs.
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
        Reach::Forbidden => "forbidden",
        Reach::Offline => "offline",
    };
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let model = ui.get_hosts();
            let mut list: Vec<Host> = (0..model.row_count()).filter_map(|i| model.row_data(i)).collect();
            if let Some(h) = list.get_mut(active) {
                h.reachability = SharedString::from(label);
            }
            ui.set_hosts(VecModel::from_slice(&list));
        }
    });
}

// `android_main` is the entry point android-activity dispatches to; it
// must keep its plain symbol name. `slint::android::AndroidApp` isn't
// `#[repr(C)]` so we can't honour clippy's "no_mangle should be extern"
// recommendation either — allow it explicitly.
#[allow(clippy::no_mangle_with_rust_abi)]
#[unsafe(no_mangle)]
pub fn android_main(app: slint::android::AndroidApp) {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Debug));

    let data_dir = app
        .internal_data_path()
        .unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    let config_path = data_dir.join("hosts.json");
    log::info!("ta-remote config path: {}", config_path.display());

    // Keep an AndroidApp clone for JNI callbacks (e.g. scan-QR) before
    // slint::android::init takes ownership of the original.
    let app_for_callbacks = app.clone();

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

    let agent: Arc<ureq::Agent> = Arc::new(ureq::AgentBuilder::new().timeout(Duration::from_secs(5)).build());

    let last_reach: Arc<Mutex<Reach>> = Arc::new(Mutex::new(Reach::Offline));
    let seen_previews: SeenPreviews = Arc::new(Mutex::new(HashMap::new()));
    // Background poller — fetches the tab list + host stats. The old
    // per-tab /output poll (driven by `open_tab` when the user opened
    // a tab in the in-app TerminalView) is gone; the tap-to-browser
    // flow hands rendering to the web viewer, which runs its own WS.
    let poll_weak = ui_weak.clone();
    let poll_agent = agent.clone();
    let poll_data = data.clone();
    let poll_reach = last_reach.clone();
    let poll_seen = seen_previews.clone();
    std::thread::spawn(move || {
        loop {
            let (host, active_idx) = {
                let d = poll_data.lock().unwrap();
                (d.active_host(), d.active)
            };
            if let Some(host) = host {
                let result = fetch_tabs(&poll_agent, &host);
                let reach = result.reach;
                if let Some(t) = result.tabs {
                    push_tabs(&poll_weak, t, &poll_seen);
                }
                push_host_stats(&poll_weak, &result.host);
                log::debug!("poll {}: {reach:?}", host.name);
                // Show a toast when reachability transitions between online and
                // offline so a connection drop doesn't go unnoticed.
                // Forbidden gets its own message — the host answered, the
                // saved token just doesn't match, so "Disconnected" would
                // be misleading.
                {
                    let mut last = poll_reach.lock().unwrap();
                    let was_online = matches!(*last, Reach::Lan | Reach::Remote);
                    let is_online = matches!(reach, Reach::Lan | Reach::Remote);
                    let was_forbidden = matches!(*last, Reach::Forbidden);
                    let is_forbidden = matches!(reach, Reach::Forbidden);
                    if (was_online || was_forbidden) && !is_online && !is_forbidden {
                        show_toast(&poll_weak, format!("Disconnected from {}", host.name));
                    } else if !was_forbidden && is_forbidden {
                        show_toast(
                            &poll_weak,
                            format!("{} rejected the saved token — re-pair to reconnect", host.name),
                        );
                    } else if !was_online && is_online {
                        let via = if matches!(reach, Reach::Lan) { "LAN" } else { "remote" };
                        show_toast(&poll_weak, format!("Connected to {} via {via}", host.name));
                    }
                    *last = reach;
                }
                push_reachability(&poll_weak, active_idx, reach);
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });

    // request-send-input is still fired by the action-sheet's
    // "restart catbus-agent" entry (it types `clear && exec
    // catbus-agent\n` into the chosen tab). The TerminalView keyboard
    // and tap-to-activate pieces are gone with the in-app renderer,
    // so the activate / typed-text handlers don't exist anymore.
    let send_agent = agent.clone();
    let send_data = data.clone();
    let send_reach = last_reach.clone();
    ui.on_request_send_input(move |idx, text| {
        let Some(host) = send_data.lock().unwrap().active_host() else {
            return;
        };
        let reach = *send_reach.lock().unwrap();
        let agent = send_agent.clone();
        let bytes = text.as_bytes().to_vec();
        std::thread::spawn(move || post_input(&agent, &host, reach, idx, &bytes));
    });

    let close_agent = agent.clone();
    let close_data = data.clone();
    let close_reach = last_reach.clone();
    let close_weak = ui_weak.clone();
    let close_seen = seen_previews.clone();
    ui.on_request_close_tab(move |idx| {
        let Some(host) = close_data.lock().unwrap().active_host() else {
            return;
        };
        let reach = *close_reach.lock().unwrap();
        let agent = close_agent.clone();
        std::thread::spawn(move || delete_tab(&agent, &host, reach, idx));
        refresh_soon(&close_weak, &close_agent, &close_data, &close_reach, &close_seen);
    });

    let new_agent = agent.clone();
    let new_data = data.clone();
    let new_reach = last_reach.clone();
    let new_weak = ui_weak.clone();
    let new_seen = seen_previews.clone();
    ui.on_request_new_tab(move || {
        let Some(host) = new_data.lock().unwrap().active_host() else {
            return;
        };
        let reach = *new_reach.lock().unwrap();
        let agent = new_agent.clone();
        std::thread::spawn(move || post_new_tab(&agent, &host, reach));
        refresh_soon(&new_weak, &new_agent, &new_data, &new_reach, &new_seen);
    });

    let rename_agent = agent.clone();
    let rename_data = data.clone();
    let rename_reach = last_reach.clone();
    let rename_weak = ui_weak.clone();
    let rename_seen = seen_previews.clone();
    ui.on_request_rename_tab(move |idx, name| {
        let Some(host) = rename_data.lock().unwrap().active_host() else {
            return;
        };
        let reach = *rename_reach.lock().unwrap();
        let agent = rename_agent.clone();
        let name = name.to_string();
        std::thread::spawn(move || post_rename_tab(&agent, &host, reach, idx, &name));
        refresh_soon(&rename_weak, &rename_agent, &rename_data, &rename_reach, &rename_seen);
    });

    // Tab tap → share-viewer in system browser. Build the URL from
    // the active host + idx + token, fire Intent.ACTION_VIEW via
    // JNI. Failures log + drop the tap — there's no in-app fallback
    // anymore (the Slint TerminalView is gone).
    // Back-key handler bridge: Slint's FocusScope asks Rust whether
    // there's a mounted WebView to dismiss. If yes, Slint consumes
    // the Back press; if no, the press falls through to the in-app
    // dialog handlers below.
    let dismiss_app = app_for_callbacks.clone();
    ui.on_request_dismiss_webview(move || dismiss_webview(&dismiss_app));

    let browser_data = data.clone();
    let browser_app = app_for_callbacks.clone();
    ui.on_open_tab_in_browser(move |idx| {
        if idx < 0 {
            return;
        }
        let host = browser_data.lock().unwrap().active_host();
        let Some(host) = host else {
            log::warn!("open-tab-in-browser fired with no active host");
            return;
        };
        // Prefer the LAN URL (faster, no NAT traversal); fall back to
        // remote-url if LAN is empty. base_url() chooses based on
        // last-poll reach but here we're DISPATCHING, so any reachable
        // URL is fine — the browser will retry against the user's
        // network state.
        let base = if host.url.is_empty() {
            &host.remote_url
        } else {
            &host.url
        };
        if base.is_empty() {
            log::warn!("open-tab-in-browser: host has no URL");
            return;
        }
        // Strip trailing slash so we don't emit `//tabs/…`.
        let base = base.trim_end_matches('/');
        // Token is opaque (hex today), but `ureq::url::form_urlencoded`
        // is overkill for a single value. Validate that there's no
        // bare `&` or `#` that would split the query.
        if host.token.contains('&') || host.token.contains('#') || host.token.contains(' ') {
            log::warn!("open-tab-in-browser: token contains unsafe chars, refusing");
            return;
        }
        // Use the HTTP base, NOT TLS. We tried HTTPS first but Chromium
        // WebView refuses self-signed WSS handshakes (net_error -202,
        // CERT_AUTHORITY_INVALID) and there's no WebViewClient hook to
        // override the WS path the way onReceivedSslError covers the
        // HTTPS page load. Until the device trusts the headless's cert
        // (Settings → Security → Install certificate) we use plain HTTP.
        let url = format!("{base}/tabs/{idx}/view?token={}", host.token);
        match show_webview(&browser_app, &url) {
            Ok(()) => log::info!("mounted WebView for tab {idx} at {url}"),
            Err(e) => log::warn!("show_webview failed: {e}"),
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

    let upd_data = data;
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

    // Add-Workstation entry points all funnel through this callback so
    // pasting the desktop's `taremote://onboard…` link from the system
    // clipboard auto-fills the editor. When the clipboard has nothing
    // usable, fall back to a blank editor without an error banner —
    // the user explicitly chose "add", so a scolding "clipboard didn't
    // contain X" message would be wrong.
    let add_app = app_for_callbacks.clone();
    let add_weak = ui_weak.clone();
    ui.on_request_add_host_prefill(move || {
        let pasted = read_clipboard(&add_app).unwrap_or_default();
        let parsed = pasted
            .lines()
            .find(|l| l.trim_start().starts_with("taremote://onboard?"))
            .and_then(|l| crate::onboard::parse_onboard_url(l.trim()));
        let weak = add_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_editor_edit_index(-1);
            ui.set_editor_name(SharedString::new());
            ui.set_editor_remote_url(SharedString::new());
            ui.set_editor_error(SharedString::new());
            if let Some((url, token)) = parsed {
                ui.set_editor_url(SharedString::from(url));
                ui.set_editor_token(SharedString::from(token));
            } else {
                ui.set_editor_url(SharedString::new());
                ui.set_editor_token(SharedString::new());
            }
            ui.set_editor_open(true);
        });
    });

    // Last use of `ui_weak`; the local battery poller that used to
    // hold a separate clone is gone.
    let scan_weak = ui_weak;
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
            if let Some((url, token)) = parsed {
                ui.set_editor_url(SharedString::from(url));
                ui.set_editor_token(SharedString::from(token));
                ui.set_editor_error(SharedString::new());
            } else {
                ui.set_editor_url(SharedString::new());
                ui.set_editor_token(SharedString::new());
                ui.set_editor_error(SharedString::from(
                    "Clipboard didn't contain a taremote:// URL. Copy the URL under the QR code on the desktop and try again.",
                ));
            }
            ui.set_editor_open(true);
        });
    });

    // The workstation's battery + power draw arrive through the
    // existing `/tabs` poll (host stats block) — no separate poller
    // needed.

    ui.run().unwrap();
}
