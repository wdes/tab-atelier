// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::tracking::USER_AGENT;

#[derive(Serialize)]
struct TabInfo {
    index: usize,
    name: String,
    cwd: Option<String>,
    active: bool,
    cpu_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    watts: Option<f64>,
}

#[derive(Serialize)]
struct ApiResponse {
    app: &'static str,
    tabs: Vec<TabInfo>,
}

pub struct TabSnapshot {
    pub tabs: Vec<(String, Option<String>)>,
    pub active: usize,
    pub power: Vec<crate::power::TabPower>,
}

pub fn start_api_server(state: Arc<Mutex<TabSnapshot>>) {
    std::thread::spawn(move || {
        let listener = match TcpListener::bind("127.0.0.1:7890") {
            Ok(l) => l,
            Err(e) => {
                eprintln!("swoop api: failed to bind :7890: {e}");
                return;
            }
        };

        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let state = state.lock().unwrap();

            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }

            let tabs: Vec<TabInfo> = state
                .tabs
                .iter()
                .enumerate()
                .map(|(i, (name, cwd))| TabInfo {
                    index: i,
                    name: name.clone(),
                    cwd: cwd.clone(),
                    active: i == state.active,
                    cpu_percent: state.power.get(i).map(|p| p.cpu_percent).unwrap_or(0.0),
                    watts: state.power.get(i).and_then(|p| p.watts),
                })
                .collect();

            let resp = ApiResponse { app: USER_AGENT, tabs };
            let body = serde_json::to_string_pretty(&resp).unwrap_or_default();

            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
}
