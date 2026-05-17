// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{BufRead, BufReader, Read, Write};
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
    /// Last non-empty line of the cached output buffer — used by remote clients
    /// to preview what's happening without fetching the full output.
    #[serde(skip_serializing_if = "String::is_empty")]
    preview: String,
    #[cfg(feature = "energy")]
    cpu_percent: f64,
    #[cfg(feature = "energy")]
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

#[derive(Clone)]
pub struct SnapshotTab {
    pub name: String,
    pub cwd: Option<String>,
    pub output: String,
}

pub struct TabSnapshot {
    pub tabs: Vec<SnapshotTab>,
    pub active: usize,
    #[cfg(feature = "energy")]
    pub power: Vec<crate::power::TabPower>,
    pub pending_closes: Vec<usize>,
    pub pending_activate: Option<usize>,
    pub pending_input: Vec<(usize, Vec<u8>)>,
    pub pending_new_tabs: usize,
    /// (tab index, new name) pairs queued by `POST /tabs/{idx}/rename`.
    pub pending_renames: Vec<(usize, String)>,
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

fn respond_json<W: Write>(stream: &mut W, status: u16, body: &str) {
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

fn error_json<W: Write>(stream: &mut W, status: u16, msg: &str) {
    let body = serde_json::to_string(&ErrorResponse { error: msg.to_string() }).unwrap_or_default();
    respond_json(stream, status, &body);
}

fn handle_connection<S: Read + Write>(
    stream: &mut S,
    state: &Arc<Mutex<TabSnapshot>>,
    token: &str,
) {
    // Owned BufReader around the stream itself — `try_clone` was only used
    // to dodge the read/write borrow on TcpStream, but it doesn't exist on
    // rustls::Stream. Buffering on `&mut S` works for both, and the read
    // side is dropped before any write below.
    let mut reader = BufReader::new(&mut *stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }

    let mut auth_token = None;
    let mut content_length: usize = 0;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        if let Some(val) = line.strip_prefix("Authorization: Bearer ") {
            auth_token = Some(val.trim().to_string());
        }
        if let Some(val) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
            content_length = val.trim().parse().unwrap_or(0);
        }
    }

    let trimmed = request_line.trim().to_string();
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0].to_string();
    let raw_path = parts[1].to_string();

    let (path, query_token, query_lines) = if let Some((p, q)) = raw_path.split_once('?') {
        let qt = q
            .split('&')
            .find_map(|pair| pair.strip_prefix("token="))
            .map(std::string::ToString::to_string);
        let ql = q
            .split('&')
            .find_map(|pair| pair.strip_prefix("lines="))
            .and_then(|s| s.parse::<usize>().ok());
        (p.to_string(), qt, ql)
    } else {
        (raw_path, None, None)
    };

    // Read the body (if any) before dropping the reader so we can write the
    // response back through `stream` without a borrow conflict.
    let body_bytes: Vec<u8> = if content_length > 0 {
        let mut buf = vec![0u8; content_length];
        if reader.read_exact(&mut buf).is_err() {
            drop(reader);
            error_json(stream, 400, "could not read body");
            return;
        }
        buf
    } else {
        Vec::new()
    };
    drop(reader);

    let provided_token = auth_token.or(query_token);
    if provided_token.as_deref() != Some(token) {
        debug!("API: 401 unauthorized request to {path}");
        error_json(stream, 401, "invalid or missing token");
        return;
    }

    debug!("API: {method} {path}");
    match (method.as_str(), path.as_str()) {
        ("GET", "/" | "/tabs") => {
            let state = state.lock().unwrap();
            let tabs: Vec<TabInfo> = state
                .tabs
                .iter()
                .enumerate()
                .map(|(i, t)| TabInfo {
                    index: i,
                    name: t.name.clone(),
                    cwd: t.cwd.clone(),
                    active: i == state.active,
                    preview: t.output
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .to_string(),
                    #[cfg(feature = "energy")]
                    cpu_percent: state.power.get(i).map_or(0.0, |p| p.cpu_percent),
                    #[cfg(feature = "energy")]
                    watts: state.power.get(i).and_then(|p| p.watts),
                })
                .collect();
            drop(state);
            let resp = ApiResponse { app: USER_AGENT, tabs };
            let body = serde_json::to_string_pretty(&resp).unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/output") => {
            let idx_str = &p["/tabs/".len()..p.len() - "/output".len()];
            if let Ok(idx) = idx_str.parse::<usize>() {
                let state = state.lock().unwrap();
                if let Some(t) = state.tabs.get(idx) {
                    let mut body = t.output.clone();
                    drop(state);
                    if let Some(n) = query_lines
                        && n > 0
                    {
                        let lines: Vec<&str> = body.lines().collect();
                        if lines.len() > n {
                            body = lines[lines.len() - n..].join("\n");
                        }
                    }
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                } else {
                    error_json(stream, 404, "tab index out of range");
                }
            } else {
                error_json(stream, 404, "invalid tab index");
            }
        }
        ("DELETE", p) if p.starts_with("/tabs/") && !p[6..].contains('/') => {
            let idx_str = &p[6..];
            if let Ok(idx) = idx_str.parse::<usize>() {
                let mut state = state.lock().unwrap();
                if idx < state.tabs.len() {
                    info!("API: closing tab {idx}");
                    state.pending_closes.push(idx);
                    drop(state);
                    let body = serde_json::to_string(&serde_json::json!({"closed": idx})).unwrap_or_default();
                    respond_json(stream, 200, &body);
                } else {
                    error_json(stream, 404, "tab index out of range");
                }
            } else {
                error_json(stream, 404, "invalid tab index");
            }
        }
        ("POST", "/tabs") => {
            let mut state = state.lock().unwrap();
            info!("API: queueing new tab creation");
            state.pending_new_tabs += 1;
            drop(state);
            let body = serde_json::to_string(&serde_json::json!({"queued": "new"})).unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/rename") => {
            let idx_str = &p["/tabs/".len()..p.len() - "/rename".len()];
            if let Ok(idx) = idx_str.parse::<usize>() {
                let body = &body_bytes;
                let new_name = serde_json::from_slice::<serde_json::Value>(body).map_or_else(
                    |_| String::from_utf8_lossy(body).trim().to_string(),
                    |v| v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
                );
                if new_name.is_empty() {
                    error_json(stream, 400, "missing or empty name");
                    return;
                }
                let mut state = state.lock().unwrap();
                if idx < state.tabs.len() {
                    info!("API: renaming tab {idx} to {new_name}");
                    state.pending_renames.push((idx, new_name.clone()));
                    drop(state);
                    let body = serde_json::to_string(&serde_json::json!({"renamed": idx, "name": new_name})).unwrap_or_default();
                    respond_json(stream, 200, &body);
                } else {
                    error_json(stream, 404, "tab index out of range");
                }
            } else {
                error_json(stream, 404, "invalid tab index");
            }
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/activate") => {
            let idx_str = &p["/tabs/".len()..p.len() - "/activate".len()];
            if let Ok(idx) = idx_str.parse::<usize>() {
                let mut state = state.lock().unwrap();
                if idx < state.tabs.len() {
                    info!("API: activating tab {idx}");
                    state.pending_activate = Some(idx);
                    drop(state);
                    let body = serde_json::to_string(&serde_json::json!({"activated": idx})).unwrap_or_default();
                    respond_json(stream, 200, &body);
                } else {
                    error_json(stream, 404, "tab index out of range");
                }
            } else {
                error_json(stream, 404, "invalid tab index");
            }
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/input") => {
            let idx_str = &p["/tabs/".len()..p.len() - "/input".len()];
            if let Ok(idx) = idx_str.parse::<usize>() {
                let mut state = state.lock().unwrap();
                if idx < state.tabs.len() {
                    info!("API: sending {} bytes of input to tab {idx}", body_bytes.len());
                    let n = body_bytes.len();
                    state.pending_input.push((idx, body_bytes));
                    drop(state);
                    let resp = serde_json::to_string(&serde_json::json!({"sent": n})).unwrap_or_default();
                    respond_json(stream, 200, &resp);
                } else {
                    error_json(stream, 404, "tab index out of range");
                }
            } else {
                error_json(stream, 404, "invalid tab index");
            }
        }
        (_, "/" | "/tabs") => {
            error_json(stream, 405, "method not allowed");
        }
        (_, p) if p.starts_with("/tabs/") => {
            error_json(stream, 405, "method not allowed");
        }
        _ => {
            error_json(stream, 404, "not found");
        }
    }
}

pub fn serve(listener: &TcpListener, state: &Arc<Mutex<TabSnapshot>>, token: &str) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        handle_connection(&mut stream, state, token);
    }
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
        serve(&listener, &state, &token);
    });
}

/// Start a second listener on `:7891` that serves the same API over TLS.
///
/// Uses a self-signed certificate generated on first launch and cached at
/// `{state_base}/tab-atelier/{tls.crt,tls.key}`. The cert is created with
/// the host's local IP and `localhost` as SANs so clients on the LAN can
/// validate via either. Pin-on-first-use clients (the Android remote) can
/// trust the cert directly; browsers will warn until added to their
/// trust store — fine for personal use.
pub fn start_api_server_tls(state: Arc<Mutex<TabSnapshot>>, token: String) {
    use rustls::ServerConfig;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let (cert_der, key_der) = match load_or_generate_cert() {
        Ok(pair) => pair,
        Err(e) => {
            error!("API/TLS: cert provisioning failed: {e}");
            return;
        }
    };

    let cfg = match ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert_der)],
            PrivateKeyDer::try_from(key_der).map_err(std::string::ToString::to_string).unwrap(),
        ) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            error!("API/TLS: rustls config build failed: {e}");
            return;
        }
    };

    std::thread::spawn(move || {
        let listener = match TcpListener::bind("0.0.0.0:7891") {
            Ok(l) => {
                info!("API: TLS listening on 0.0.0.0:7891");
                l
            }
            Err(e) => {
                error!("API: failed to bind :7891: {e}");
                return;
            }
        };
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut conn = match rustls::ServerConnection::new(cfg.clone()) {
                Ok(c) => c,
                Err(e) => {
                    debug!("API/TLS: handshake init failed: {e}");
                    continue;
                }
            };
            let mut tls = rustls::Stream::new(&mut conn, &mut stream);
            handle_connection(&mut tls, &state, &token);
        }
    });
}

fn load_or_generate_cert() -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let dir = crate::platform::state_base_dir().join(tab_atelier::APP_DIR);
    std::fs::create_dir_all(&dir)?;
    let crt_path = dir.join("tls.crt");
    let key_path = dir.join("tls.key");

    if crt_path.exists() && key_path.exists() {
        let crt_pem = std::fs::read(&crt_path)?;
        let key_pem = std::fs::read(&key_path)?;
        let cert_der = rustls_pemfile::certs(&mut crt_pem.as_slice())
            .next()
            .and_then(Result::ok)
            .ok_or_else(|| std::io::Error::other("no cert in tls.crt"))?
            .to_vec();
        let key_der = rustls_pemfile::private_key(&mut key_pem.as_slice())?
            .ok_or_else(|| std::io::Error::other("no key in tls.key"))?
            .secret_der()
            .to_vec();
        return Ok((cert_der, key_der));
    }

    info!("API/TLS: generating self-signed certificate at {}", dir.display());
    let mut params = rcgen::CertificateParams::new(vec![
        "localhost".to_string(),
        local_ip(),
    ])
    .map_err(|e| std::io::Error::other(e.to_string()))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "tab-atelier");
    let key_pair = rcgen::KeyPair::generate()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let crt_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();
    std::fs::write(&crt_path, &crt_pem)?;
    std::fs::write(&key_path, &key_pem)?;
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();
    Ok((cert_der, key_der))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    fn test_state() -> Arc<Mutex<TabSnapshot>> {
        Arc::new(Mutex::new(TabSnapshot {
            tabs: vec![
                SnapshotTab {
                    name: "shell".into(),
                    cwd: Some("/home/user".into()),
                    output: "$ ls\nfoo bar baz".into(),
                },
                SnapshotTab {
                    name: "build".into(),
                    cwd: None,
                    output: String::new(),
                },
            ],
            active: 0,
            #[cfg(feature = "energy")]
            power: vec![],
            pending_closes: vec![],
            pending_activate: None,
            pending_input: vec![],
            pending_new_tabs: 0,
            pending_renames: vec![],
        }))
    }

    fn spawn_server() -> (u16, Arc<Mutex<TabSnapshot>>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = test_state();
        let token = "test-secret-token".to_string();
        let s = state.clone();
        let t = token.clone();
        std::thread::spawn(move || serve(&listener, &s, &t));
        (port, state, token)
    }

    fn request(port: u16, req: &str) -> String {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        stream.write_all(req.as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).unwrap();
        buf
    }

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

    #[test]
    fn generate_token_length() {
        let t = generate_token();
        assert_eq!(t.len(), 32);
    }

    #[test]
    fn generate_token_is_hex() {
        let t = generate_token();
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
    }

    #[test]
    fn local_ip_not_empty() {
        let ip = local_ip();
        assert!(!ip.is_empty());
    }

    #[test]
    fn local_ip_valid_format() {
        let ip = local_ip();
        assert!(ip.contains('.'), "should be IPv4: {ip}");
        let parts: Vec<&str> = ip.split('.').collect();
        assert_eq!(parts.len(), 4);
        for p in parts {
            assert!(p.parse::<u32>().unwrap() <= 255);
        }
    }

    #[test]
    fn get_tabs_with_bearer_token() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("GET /tabs HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        let b = body(&resp);
        let json: serde_json::Value = serde_json::from_str(b).unwrap();
        assert_eq!(json["tabs"][0]["name"], "shell");
        assert_eq!(json["tabs"][0]["cwd"], "/home/user");
        assert_eq!(json["tabs"][0]["active"], true);
        // Last non-empty line of the cached output is exposed as preview.
        assert_eq!(json["tabs"][0]["preview"], "foo bar baz");
        assert_eq!(json["tabs"][1]["name"], "build");
        assert_eq!(json["tabs"][1]["active"], false);
        // Empty output → preview field omitted entirely.
        assert!(json["tabs"][1].get("preview").is_none());
    }

    #[test]
    fn get_root_with_query_token() {
        let (port, _, token) = spawn_server();
        let resp = request(port, &format!("GET /?token={token} HTTP/1.1\r\n\r\n"));
        assert_eq!(status_code(&resp), 200);
        let json: serde_json::Value = serde_json::from_str(body(&resp)).unwrap();
        assert!(json["app"].as_str().unwrap().contains("tab-atelier"));
    }

    #[test]
    fn unauthorized_without_token() {
        let (port, _, _) = spawn_server();
        let resp = request(port, "GET /tabs HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 401);
        let json: serde_json::Value = serde_json::from_str(body(&resp)).unwrap();
        assert!(json["error"].as_str().unwrap().contains("invalid"));
    }

    #[test]
    fn unauthorized_wrong_token() {
        let (port, _, _) = spawn_server();
        let resp = request(port, "GET /tabs HTTP/1.1\r\nAuthorization: Bearer wrong\r\n\r\n");
        assert_eq!(status_code(&resp), 401);
    }

    #[test]
    fn delete_tab_success() {
        let (port, state, token) = spawn_server();
        let resp = request(
            port,
            &format!("DELETE /tabs/1 HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        let json: serde_json::Value = serde_json::from_str(body(&resp)).unwrap();
        assert_eq!(json["closed"], 1);
        assert_eq!(state.lock().unwrap().pending_closes, vec![1]);
    }

    #[test]
    fn delete_tab_out_of_range() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("DELETE /tabs/99 HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
        assert!(body(&resp).contains("out of range"));
    }

    #[test]
    fn delete_tab_invalid_index() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("DELETE /tabs/abc HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
        assert!(body(&resp).contains("invalid tab index"));
    }

    #[test]
    fn method_not_allowed_on_tabs() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("PATCH /tabs HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 405);
    }

    #[test]
    fn post_tabs_queues_new_tab() {
        let (port, state, token) = spawn_server();
        let resp = request(
            port,
            &format!("POST /tabs HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        assert_eq!(state.lock().unwrap().pending_new_tabs, 1);
    }

    #[test]
    fn post_tabs_requires_auth() {
        let (port, _, _) = spawn_server();
        let resp = request(port, "POST /tabs HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 401);
    }

    #[test]
    fn rename_tab_success_json_body() {
        let (port, state, token) = spawn_server();
        let body = r#"{"name":"renamed"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/0/rename HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let pending = state.lock().unwrap().pending_renames.clone();
        assert_eq!(pending, vec![(0_usize, "renamed".into())]);
    }

    #[test]
    fn rename_tab_empty_name_400() {
        let (port, _, token) = spawn_server();
        let body = r#"{"name":""}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/0/rename HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            ),
        );
        assert_eq!(status_code(&resp), 400);
    }

    #[test]
    fn rename_tab_out_of_range() {
        let (port, _, token) = spawn_server();
        let body = r#"{"name":"x"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/99/rename HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            ),
        );
        assert_eq!(status_code(&resp), 404);
    }

    #[test]
    fn method_not_allowed_on_tab_index() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("PATCH /tabs/0 HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 405);
    }

    #[test]
    fn not_found_unknown_path() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("GET /unknown HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
        assert!(body(&resp).contains("not found"));
    }

    #[test]
    fn query_token_with_extra_params() {
        let (port, _, token) = spawn_server();
        let resp = request(port, &format!("GET /tabs?foo=bar&token={token}&baz=1 HTTP/1.1\r\n\r\n"));
        assert_eq!(status_code(&resp), 200);
    }

    #[test]
    fn activate_tab_success() {
        let (port, state, token) = spawn_server();
        let resp = request(
            port,
            &format!("POST /tabs/1/activate HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        let json: serde_json::Value = serde_json::from_str(body(&resp)).unwrap();
        assert_eq!(json["activated"], 1);
        assert_eq!(state.lock().unwrap().pending_activate, Some(1));
    }

    #[test]
    fn activate_tab_out_of_range() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("POST /tabs/99/activate HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
        assert!(body(&resp).contains("out of range"));
    }

    #[test]
    fn activate_tab_invalid_index() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("POST /tabs/abc/activate HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
    }

    #[test]
    fn activate_requires_auth() {
        let (port, _, _) = spawn_server();
        let resp = request(port, "POST /tabs/0/activate HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 401);
    }

    #[test]
    fn send_input_success() {
        let (port, state, token) = spawn_server();
        let payload = "ls -la\n";
        let resp = request(
            port,
            &format!(
                "POST /tabs/0/input HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{}",
                payload.len(),
                payload
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let json: serde_json::Value = serde_json::from_str(body(&resp)).unwrap();
        assert_eq!(json["sent"], payload.len());
        let pending = state.lock().unwrap().pending_input.clone();
        assert_eq!(pending, vec![(0_usize, payload.as_bytes().to_vec())]);
    }

    #[test]
    fn send_input_empty_body() {
        let (port, state, token) = spawn_server();
        let resp = request(
            port,
            &format!(
                "POST /tabs/0/input HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let json: serde_json::Value = serde_json::from_str(body(&resp)).unwrap();
        assert_eq!(json["sent"], 0);
        let pending = state.lock().unwrap().pending_input.clone();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].1.is_empty());
    }

    #[test]
    fn send_input_out_of_range() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!(
                "POST /tabs/99/input HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 1\r\n\r\nx"
            ),
        );
        assert_eq!(status_code(&resp), 404);
    }

    #[test]
    fn get_tab_output_success() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("GET /tabs/0/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        let b = body(&resp);
        assert_eq!(b, "$ ls\nfoo bar baz");
    }

    #[test]
    fn get_tab_output_empty() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("GET /tabs/1/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        assert_eq!(body(&resp), "");
    }

    #[test]
    fn get_tab_output_out_of_range() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("GET /tabs/99/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
    }

    #[test]
    fn get_tab_output_requires_auth() {
        let (port, _, _) = spawn_server();
        let resp = request(port, "GET /tabs/0/output HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 401);
    }

    #[test]
    fn get_tab_output_lines_param_tails() {
        let (port, state, token) = spawn_server();
        state.lock().unwrap().tabs[0].output =
            (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let resp = request(
            port,
            &format!("GET /tabs/0/output?lines=3&token={token} HTTP/1.1\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        assert_eq!(body(&resp), "line 8\nline 9\nline 10");
    }

    #[test]
    fn get_tab_output_lines_param_larger_than_buffer_returns_all() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("GET /tabs/0/output?lines=99&token={token} HTTP/1.1\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        assert_eq!(body(&resp), "$ ls\nfoo bar baz");
    }

    #[test]
    fn send_input_binary_bytes() {
        // ctrl-c (0x03) + newline (0x0a)
        let (port, state, token) = spawn_server();
        let payload: &[u8] = &[0x03, 0x0a];
        let header = format!(
            "POST /tabs/1/input HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        );
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        stream.write_all(header.as_bytes()).unwrap();
        stream.write_all(payload).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).unwrap();
        assert_eq!(status_code(&buf), 200);
        let pending = state.lock().unwrap().pending_input.clone();
        assert_eq!(pending, vec![(1_usize, vec![0x03_u8, 0x0a])]);
    }
}
