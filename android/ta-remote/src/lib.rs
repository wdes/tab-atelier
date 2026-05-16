#![cfg(target_os = "android")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, SharedString, VecModel, Weak};

slint::include_modules!();

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostConfig {
    name: String,
    url: String,
    token: String,
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
        let stored: StoredConfig = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            hosts: stored.hosts,
            active: stored.active,
            config_path,
        }
    }

    fn save(&self) {
        let stored = StoredConfig {
            hosts: self.hosts.clone(),
            active: self.active,
        };
        if let Ok(text) = serde_json::to_string_pretty(&stored)
            && let Some(parent) = self.config_path.parent()
        {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::fs::write(&self.config_path, text);
        }
    }

    fn active_host(&self) -> Option<HostConfig> {
        self.hosts.get(self.active).cloned()
    }
}

fn fetch_tabs(agent: &ureq::Agent, host: &HostConfig) -> Result<Vec<ApiTab>, ureq::Error> {
    let resp: ApiResponse = agent
        .get(&format!("{}/tabs", host.url))
        .set("Authorization", &format!("Bearer {}", host.token))
        .timeout(Duration::from_secs(2))
        .call()?
        .into_json()?;
    Ok(resp.tabs)
}

fn post_input(agent: &ureq::Agent, host: &HostConfig, idx: i32, bytes: &[u8]) {
    let url = format!("{}/tabs/{idx}/input", host.url);
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

fn post_activate(agent: &ureq::Agent, host: &HostConfig, idx: i32) {
    let url = format!("{}/tabs/{idx}/activate", host.url);
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
            reachability: SharedString::from("offline"),
            detail: SharedString::from(host_detail(&h.url)),
            url: SharedString::from(h.url.as_str()),
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

fn push_reachability(ui_weak: &Weak<AppWindow>, active: usize, ok: bool) {
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let model = ui.get_hosts();
            let mut list: Vec<Host> = (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .collect();
            if let Some(h) = list.get_mut(active) {
                let new_state = if ok {
                    if h.url.starts_with("https://") { "remote" } else { "lan" }
                } else {
                    "offline"
                };
                h.reachability = SharedString::from(new_state);
            }
            ui.set_hosts(VecModel::from_slice(&list));
        }
    });
}

#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
    );

    let data_dir = app
        .internal_data_path()
        .unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    let config_path = data_dir.join("hosts.json");
    log::info!("ta-remote config path: {}", config_path.display());

    slint::android::init(app).unwrap();
    let ui = AppWindow::new().unwrap();
    let ui_weak = ui.as_weak();

    let data = Arc::new(Mutex::new(AppData::load(config_path)));
    push_hosts(&ui_weak, &data.lock().unwrap());

    let agent: Arc<ureq::Agent> = Arc::new(
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(5))
            .build(),
    );

    // Background poller
    let poll_weak = ui_weak.clone();
    let poll_agent = agent.clone();
    let poll_data = data.clone();
    std::thread::spawn(move || loop {
        let snapshot = poll_data.lock().unwrap().active_host();
        if let Some(host) = snapshot {
            let active_idx = poll_data.lock().unwrap().active;
            match fetch_tabs(&poll_agent, &host) {
                Ok(tabs) => {
                    push_tabs(&poll_weak, tabs);
                    push_reachability(&poll_weak, active_idx, true);
                }
                Err(e) => {
                    log::warn!("fetch_tabs({}) failed: {e}", host.url);
                    push_reachability(&poll_weak, active_idx, false);
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    });

    let act_agent = agent.clone();
    let act_data = data.clone();
    ui.on_request_activate(move |idx| {
        let Some(host) = act_data.lock().unwrap().active_host() else { return };
        let agent = act_agent.clone();
        std::thread::spawn(move || post_activate(&agent, &host, idx));
    });

    let send_agent = agent.clone();
    let send_data = data.clone();
    ui.on_request_send_input(move |idx, text| {
        let Some(host) = send_data.lock().unwrap().active_host() else { return };
        let agent = send_agent.clone();
        let bytes = text.as_bytes().to_vec();
        std::thread::spawn(move || post_input(&agent, &host, idx, &bytes));
    });

    let add_data = data.clone();
    let add_weak = ui_weak.clone();
    ui.on_request_add_host(move |name, url, token| {
        let name = name.to_string();
        let url = url.trim_end_matches('/').to_string();
        let token = token.to_string();
        if url.is_empty() || token.is_empty() {
            return;
        }
        let mut data = add_data.lock().unwrap();
        let new_idx = data.hosts.len();
        data.hosts.push(HostConfig {
            name: if name.is_empty() { host_detail(&url) } else { name },
            url,
            token,
        });
        data.active = new_idx;
        data.save();
        push_hosts(&add_weak, &data);
    });

    ui.run().unwrap();
}
