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
    preview: String,
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
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_open_tab_output(SharedString::from(text));
        }
    });
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

fn push_tabs(ui_weak: &Weak<AppWindow>, tabs: Vec<ApiTab>) {
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        let rows: Vec<TabRow> = tabs
            .into_iter()
            .map(|t| TabRow {
                name: SharedString::from(t.name),
                cwd: SharedString::from(t.cwd.unwrap_or_default()),
                active: t.active,
                cpu: SharedString::from(format!("{:.1}%", t.cpu_percent)),
                preview: SharedString::from(t.preview),
            })
            .collect();
        ui.set_tabs(VecModel::from_slice(&rows));
    });
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
    let open_tab: Arc<AtomicI32> = Arc::new(AtomicI32::new(-1));

    // Background poller
    let poll_weak = ui_weak.clone();
    let poll_agent = agent.clone();
    let poll_data = data.clone();
    let poll_reach = last_reach.clone();
    let poll_open = open_tab.clone();
    std::thread::spawn(move || loop {
        let (host, active_idx) = {
            let d = poll_data.lock().unwrap();
            (d.active_host(), d.active)
        };
        if let Some(host) = host {
            let (reach, tabs) = fetch_tabs(&poll_agent, &host);
            if let Some(t) = tabs {
                push_tabs(&poll_weak, t);
            }
            log::debug!("poll {}: {reach:?}", host.name);
            *poll_reach.lock().unwrap() = reach;
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

    let close_agent = agent.clone();
    let close_data = data.clone();
    let close_reach = last_reach.clone();
    ui.on_request_close_tab(move |idx| {
        let Some(host) = close_data.lock().unwrap().active_host() else { return };
        let reach = *close_reach.lock().unwrap();
        let agent = close_agent.clone();
        std::thread::spawn(move || delete_tab(&agent, &host, &reach, idx));
    });

    let open_tab_for_cb = open_tab.clone();
    let open_weak = ui_weak.clone();
    let open_data = data.clone();
    let open_reach = last_reach.clone();
    let open_agent = agent.clone();
    ui.on_open_tab_changed(move |idx| {
        open_tab_for_cb.store(idx, Ordering::Relaxed);
        if idx < 0 {
            push_output(&open_weak, String::new());
            return;
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

    ui.run().unwrap();
}
