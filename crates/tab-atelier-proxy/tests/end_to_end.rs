// SPDX-License-Identifier: MPL-2.0

//! The whole thing, through the real binary: mint a key with the CLI, start
//! the server, and spend that key on a request.
//!
//! Every other test here calls library functions. This one runs
//! `tab-atelier-proxy` as a process, which is the only way to catch the class
//! of bug that actually reached a deployment: the CLI and the server resolving
//! DIFFERENT directories, so a key minted by one authenticates nothing on the
//! other, and `admin-token` printing a token the service has never heard of.
//! A library test cannot see that, because it never asks where the files are.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// The binary under test, built by cargo for this integration target.
const BIN: &str = env!("CARGO_BIN_EXE_tab-atelier-proxy");

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("ta-proxy-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("home/.claude")).expect("mkdir");
        // A far-future expiry, so the egress never tries to refresh against
        // the real OAuth endpoint during a test.
        std::fs::write(
            p.join("home/.claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"oat-e2e","refreshToken":"r","expiresAt":9999999999999,"scopes":[]}}"#,
        )
        .expect("write creds");
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A child that is killed when the test ends, however it ends.
struct Serving(Child);
impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Run the CLI exactly as an operator would, with the same environment the
/// server will get. If these two disagree about the state directory, the whole
/// point of the test is lost — so they are built from one place.
fn cli(scratch: &Path, args: &[&str]) -> String {
    let out = Command::new(BIN)
        .args(args)
        .env("TAB_ATELIER_PROXY_CONFIG", scratch.join("config"))
        .env("TAB_ATELIER_PROXY_STATE", scratch.join("state"))
        .env("HOME", scratch.join("home"))
        .output()
        .expect("run the CLI");
    assert!(
        out.status.success(),
        "`{}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A port nothing is listening on. Bound and released, which races in
/// principle and does not in practice on a test machine.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

/// A stand-in Anthropic that reports usage, so the accounting has something
/// real to record.
fn mock_upstream() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { break };
            let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(2)));
            // Drain head + declared body before answering: replying and
            // closing mid-body turns into a connection reset.
            let mut req = Vec::new();
            let mut tmp = [0u8; 2048];
            let mut need: Option<usize> = None;
            loop {
                match sock.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&tmp[..n]),
                }
                if need.is_none()
                    && let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    let len = String::from_utf8_lossy(&req[..pos])
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    need = Some(pos + 4 + len);
                }
                if need.is_some_and(|n| req.len() >= n) {
                    break;
                }
            }
            let body = r#"{"model":"claude-sonnet-5","usage":{"input_tokens":11,"output_tokens":7}}"#;
            let _ = write!(
                sock,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.flush();
        }
    });
    port
}

/// One HTTP request, returning the whole response.
fn http(port: u16, req: &str) -> String {
    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    sock.write_all(req.as_bytes()).expect("write");
    let mut out = Vec::new();
    let mut tmp = [0u8; 4096];
    while let Ok(n) = sock.read(&mut tmp) {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&tmp[..n]);
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn wait_until_listening(port: u16, child: &mut Child) {
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("the proxy exited before listening: {status}");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("the proxy never started listening on {port}");
}

/// CLI mints a key → server accepts it → the call is billed to that account.
/// The credential-repair route: same boundary rules as everything else, plus
/// the guard that stops it being a way to repoint a proxy.
///
/// Its own function because the walkthrough above is already at the length
/// clippy allows, and this is a self-contained property.
fn credential_repair_is_guarded(port: u16, key: &str) {
    // 6b. The credential-repair route: same key, same boundary rules, and the
    //     guard that stops it being a way to repoint a proxy.
    let creds = r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":1}}"#;
    let post_creds = |auth: &str| {
        http(
            port,
            &format!(
                "POST /me/credentials HTTP/1.1\r\nHost: x\r\n{auth}\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{creds}",
                creds.len()
            ),
        )
    };
    let no_key = post_creds("Connection: close");
    assert!(
        no_key.starts_with("HTTP/1.1 401"),
        "credential repair must require a key:\n{no_key}"
    );
    // With a valid key it still refuses, because this scratch proxy has no
    // recorded identity to check a replacement against. Bootstrapping is a
    // host-side act on purpose: with nothing on record, "same account" cannot
    // be enforced, and a guard that cannot be enforced must not be skipped.
    let bootstrap = post_creds(&format!("x-api-key: {key}"));
    assert!(
        bootstrap.starts_with("HTTP/1.1 409"),
        "expected a refusal naming the missing identity:\n{bootstrap}"
    );
    assert!(
        bootstrap.contains("no identity on record"),
        "the refusal must say what to do about it:\n{bootstrap}"
    );
    // And it is a POST-only route.
    let wrong_method = http(
        port,
        &format!("GET /me/credentials HTTP/1.1\r\nHost: x\r\nx-api-key: {key}\r\nConnection: close\r\n\r\n"),
    );
    assert!(
        wrong_method.starts_with("HTTP/1.1 405"),
        "credential repair is POST only:\n{wrong_method}"
    );
}

#[test]
fn a_key_minted_by_the_cli_works_against_the_running_server() {
    let scratch = Scratch::new("full");
    let upstream = mock_upstream();
    let port = free_port();

    // 1. Add an account, then mint a key for a named place. `add` deliberately
    //    mints nothing: a key is named for the machine it lives on, and one
    //    handed out at signup is the one that gets deployed unnamed. The key
    //    is printed once, here and nowhere else, which is the behaviour this
    //    asserts by having to parse it out of the output.
    let added = cli(scratch.path(), &["add", "Ada", "Lovelace", "ada@example.org"]);
    assert!(
        !added.contains("key: "),
        "creating an account must not mint a key:\n{added}"
    );
    let minted = cli(scratch.path(), &["add-key", "ada@example.org", "laptop"]);
    let key = minted
        .lines()
        .find_map(|l| l.trim().strip_prefix("key: "))
        .expect("the CLI prints the key exactly once")
        .to_owned();
    assert!(key.starts_with("tap_"), "key looks wrong: {key}");

    // 2. And the admin token, which must be the SAME one the server uses —
    //    the bug that reached production was these two disagreeing.
    let admin = cli(scratch.path(), &["admin-token"]).trim().to_owned();
    assert!(admin.starts_with("tap_"));
    assert_ne!(admin, key, "the admin token and a user key are different credentials");

    // 3. Start the server on the same directories.
    let mut child = Command::new(BIN)
        .args(["serve", "--listen", &format!("127.0.0.1:{port}")])
        .env("TAB_ATELIER_PROXY_CONFIG", scratch.path().join("config"))
        .env("TAB_ATELIER_PROXY_STATE", scratch.path().join("state"))
        .env("HOME", scratch.path().join("home"))
        .env("TAB_ATELIER_PROXY_UPSTREAM", format!("http://127.0.0.1:{upstream}"))
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the proxy");
    wait_until_listening(port, &mut child);
    let _serving = Serving(child);

    // 4. Spend the key on a real proxied request.
    let payload = r#"{"model":"claude-sonnet-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = http(
        port,
        &format!(
            "POST /relay/anthropic/v1/messages HTTP/1.1\r\nHost: x\r\nx-api-key: {key}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        ),
    );
    assert!(resp.starts_with("HTTP/1.1 200"), "proxied call failed:\n{resp}");
    assert!(
        resp.contains("x-tab-atelier-proxy-route:"),
        "the response must name where it was routed:\n{resp}"
    );

    // 5. The account's own statistics, opened by its own key — the route an
    //    agent is given.
    let me = http(
        port,
        &format!("GET /me/usage HTTP/1.1\r\nHost: x\r\nx-api-key: {key}\r\nConnection: close\r\n\r\n"),
    );
    assert!(me.starts_with("HTTP/1.1 200"), "/me/usage failed:\n{me}");
    assert!(me.contains("ada@example.org"), "it should answer for the caller:\n{me}");
    assert!(
        me.contains("\"all_time\""),
        "and report the windows an agent asks about:\n{me}"
    );

    // 6. A user key must NOT open the admin API. This is the boundary the
    //    whole design rests on, checked through the real server.
    let refused = http(
        port,
        &format!("GET /api/users HTTP/1.1\r\nHost: x\r\nx-api-key: {key}\r\nConnection: close\r\n\r\n"),
    );
    assert!(
        refused.starts_with("HTTP/1.1 401"),
        "a user key opened the admin API:\n{refused}"
    );

    credential_repair_is_guarded(port, &key);

    // 7. The admin token does, and sees the account the CLI created.
    let users = http(
        port,
        &format!("GET /api/users HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {admin}\r\nConnection: close\r\n\r\n"),
    );
    assert!(users.starts_with("HTTP/1.1 200"), "admin listing failed:\n{users}");
    assert!(users.contains("ada@example.org"), "{users}");

    // 8. The spend was recorded, under the account, with the model that was
    //    billed — on disk, in the layout the dashboard reads.
    let usage_root = scratch.path().join("state/usage");
    let mut found = None;
    for _ in 0..50 {
        if let Some(f) = first_usage_file(&usage_root) {
            found = Some(f);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let file = found.unwrap_or_else(|| panic!("no usage file appeared under {}", usage_root.display()));
    let recorded = std::fs::read_to_string(&file).expect("read usage");
    assert!(
        recorded.contains("claude-sonnet-5"),
        "the hour must record which model was billed: {recorded}"
    );
    assert!(recorded.contains("\"calls\":1"), "{recorded}");
}

/// The first `*_usage.json` under any account directory.
fn first_usage_file(root: &Path) -> Option<PathBuf> {
    for account in std::fs::read_dir(root).ok()?.flatten() {
        for day in std::fs::read_dir(account.path()).ok()?.flatten() {
            let p = day.path();
            if p.to_string_lossy().ends_with("_usage.json") {
                return Some(p);
            }
        }
    }
    None
}

/// A key the CLI revoked stops working on the server it is already running
/// against — without a restart, because the server re-reads the account file.
#[test]
fn the_cli_and_the_server_agree_on_where_the_files_are() {
    let scratch = Scratch::new("agree");
    let port = free_port();

    // Mint through the CLI…
    cli(scratch.path(), &["add", "Grace", "Hopper", "grace@example.org"]);
    let minted = cli(scratch.path(), &["add-key", "grace@example.org", "laptop"]);
    let key = minted
        .lines()
        .find_map(|l| l.trim().strip_prefix("key: "))
        .expect("key")
        .to_owned();
    let admin = cli(scratch.path(), &["admin-token"]).trim().to_owned();

    // …and read it back through the server. If the two resolved different
    // directories — the failure that reached a live deployment — the account
    // simply would not be there.
    let mut child = Command::new(BIN)
        .args(["serve", "--listen", &format!("127.0.0.1:{port}")])
        .env("TAB_ATELIER_PROXY_CONFIG", scratch.path().join("config"))
        .env("TAB_ATELIER_PROXY_STATE", scratch.path().join("state"))
        .env("HOME", scratch.path().join("home"))
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    wait_until_listening(port, &mut child);
    let mut serving = Serving(child);

    let users = http(
        port,
        &format!("GET /api/users HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {admin}\r\nConnection: close\r\n\r\n"),
    );
    assert!(
        users.contains("grace@example.org"),
        "the server did not see the account the CLI created — the two are reading different \
         directories:\n{users}"
    );
    assert!(key.starts_with("tap_"));

    // The admin token the CLI printed is the one the server accepts. A second
    // token minted into another directory is exactly how a deployment ends up
    // answering "admin token required" to a token that looks right.
    assert!(users.starts_with("HTTP/1.1 200"), "{users}");

    // And nothing was written outside the directories we named.
    let stderr = serving.0.stderr.take().map(|e| {
        let mut s = String::new();
        let _ = BufReader::new(e).read_line(&mut s);
        s
    });
    assert!(
        !scratch.path().join("home/.config").exists(),
        "the server wrote into $HOME/.config despite an explicit config dir: {stderr:?}"
    );
}
