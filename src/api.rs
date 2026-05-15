// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use log::{debug, error, info};

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

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

pub struct TabSnapshot {
    pub tabs: Vec<(String, Option<String>)>,
    pub active: usize,
    pub power: Vec<crate::power::TabPower>,
    pub pending_closes: Vec<usize>,
}

pub fn generate_token() -> String {
    use std::fmt::Write;
    let mut buf = [0u8; 16];
    crate::platform::random_bytes(&mut buf);
    let mut s = String::with_capacity(buf.len() * 2);
    for b in buf {
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn local_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map_or_else(|_| "127.0.0.1".into(), |a| a.ip().to_string())
}

fn respond_json(stream: &mut std::net::TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
}

fn error_json(stream: &mut std::net::TcpStream, status: u16, msg: &str) {
    let body = serde_json::to_string(&ErrorResponse { error: msg.to_string() }).unwrap_or_default();
    respond_json(stream, status, &body);
}

pub fn start_api_server(state: Arc<Mutex<TabSnapshot>>, token: String) {
    std::thread::spawn(move || {
        let listener = match TcpListener::bind("0.0.0.0:7890") {
            Ok(l) => {
                info!("API: listening on 0.0.0.0:7890");
                l
            }
            Err(e) => {
                error!("API: failed to bind :7890: {e}");
                return;
            }
        };

        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };

            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }

            let mut auth_token = None;
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                    break;
                }
                if let Some(val) = line.strip_prefix("Authorization: Bearer ") {
                    auth_token = Some(val.trim().to_string());
                }
            }

            let trimmed = request_line.trim().to_string();
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            let method = parts[0].to_string();
            let raw_path = parts[1].to_string();

            let (path, query_token) = if let Some((p, q)) = raw_path.split_once('?') {
                let qt = q
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("token="))
                    .map(std::string::ToString::to_string);
                (p.to_string(), qt)
            } else {
                (raw_path.clone(), None)
            };

            let provided_token = auth_token.or(query_token);
            if provided_token.as_deref() != Some(&token) {
                debug!("API: 401 unauthorized request to {path}");
                error_json(&mut stream, 401, "invalid or missing token");
                continue;
            }

            debug!("API: {method} {path}");
            match (method.as_str(), path.as_str()) {
                ("GET", "/" | "/tabs") => {
                    let state = state.lock().unwrap();
                    let tabs: Vec<TabInfo> = state
                        .tabs
                        .iter()
                        .enumerate()
                        .map(|(i, (name, cwd))| TabInfo {
                            index: i,
                            name: name.clone(),
                            cwd: cwd.clone(),
                            active: i == state.active,
                            cpu_percent: state.power.get(i).map_or(0.0, |p| p.cpu_percent),
                            watts: state.power.get(i).and_then(|p| p.watts),
                        })
                        .collect();
                    drop(state);
                    let resp = ApiResponse { app: USER_AGENT, tabs };
                    let body = serde_json::to_string_pretty(&resp).unwrap_or_default();
                    respond_json(&mut stream, 200, &body);
                }
                ("DELETE", p) if p.starts_with("/tabs/") => {
                    let idx_str = &p[6..];
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        let mut state = state.lock().unwrap();
                        if idx < state.tabs.len() {
                            info!("API: closing tab {idx}");
                            state.pending_closes.push(idx);
                            drop(state);
                            let body = serde_json::to_string(&serde_json::json!({"closed": idx})).unwrap_or_default();
                            respond_json(&mut stream, 200, &body);
                        } else {
                            error_json(&mut stream, 404, "tab index out of range");
                        }
                    } else {
                        error_json(&mut stream, 404, "invalid tab index");
                    }
                }
                (_, "/" | "/tabs") => {
                    error_json(&mut stream, 405, "method not allowed");
                }
                (_, p) if p.starts_with("/tabs/") => {
                    error_json(&mut stream, 405, "method not allowed");
                }
                _ => {
                    error_json(&mut stream, 404, "not found");
                }
            }
        }
    });
}
