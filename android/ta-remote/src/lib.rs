#![cfg(target_os = "android")]

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use slint::{ComponentHandle, Model, SharedString, VecModel, Weak};

slint::include_modules!();

/// Compile-time host config. Will be replaced by a persisted multi-host
/// config with LAN/remote fallback once the settings UI lands.
const HOST_URL: &str = match option_env!("TA_REMOTE_HOST_URL") {
    Some(v) => v,
    None => "http://192.168.1.42:7890",
};
const HOST_TOKEN: &str = match option_env!("TA_REMOTE_HOST_TOKEN") {
    Some(v) => v,
    None => "no-token-set",
};

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

fn fetch_tabs(agent: &ureq::Agent) -> Result<Vec<ApiTab>, ureq::Error> {
    let resp: ApiResponse = agent
        .get(&format!("{HOST_URL}/tabs"))
        .set("Authorization", &format!("Bearer {HOST_TOKEN}"))
        .timeout(Duration::from_secs(2))
        .call()?
        .into_json()?;
    Ok(resp.tabs)
}

fn post_input(agent: &ureq::Agent, idx: i32, bytes: &[u8]) {
    let url = format!("{HOST_URL}/tabs/{idx}/input");
    if let Err(e) = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {HOST_TOKEN}"))
        .set("Content-Type", "application/octet-stream")
        .timeout(Duration::from_secs(2))
        .send_bytes(bytes)
    {
        log::warn!("post_input failed: {e}");
    }
}

fn post_activate(agent: &ureq::Agent, idx: i32) {
    let url = format!("{HOST_URL}/tabs/{idx}/activate");
    if let Err(e) = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {HOST_TOKEN}"))
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

fn push_reachability(ui_weak: &Weak<AppWindow>, ok: bool) {
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let hosts = ui.get_hosts();
            let mut list: Vec<Host> = (0..hosts.row_count())
                .filter_map(|i| hosts.row_data(i))
                .collect();
            if let Some(h) = list.first_mut() {
                h.reachability = SharedString::from(if ok { "lan" } else { "offline" });
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
    slint::android::init(app).unwrap();
    let ui = AppWindow::new().unwrap();
    let ui_weak = ui.as_weak();

    let agent: Arc<ureq::Agent> = Arc::new(
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(5))
            .build(),
    );

    // Background poller
    let poll_weak = ui_weak.clone();
    let poll_agent = agent.clone();
    std::thread::spawn(move || loop {
        match fetch_tabs(&poll_agent) {
            Ok(tabs) => {
                push_tabs(&poll_weak, tabs);
                push_reachability(&poll_weak, true);
            }
            Err(e) => {
                log::warn!("fetch_tabs failed: {e}");
                push_reachability(&poll_weak, false);
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    });

    let activate_agent = agent.clone();
    ui.on_request_activate(move |idx| {
        let agent = activate_agent.clone();
        std::thread::spawn(move || post_activate(&agent, idx));
    });

    let send_agent = agent.clone();
    ui.on_request_send_input(move |idx, text| {
        let agent = send_agent.clone();
        let bytes = text.as_bytes().to_vec();
        std::thread::spawn(move || post_input(&agent, idx, &bytes));
    });

    ui.run().unwrap();
}
