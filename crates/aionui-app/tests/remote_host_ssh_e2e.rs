//! E14 end-to-end: headless Host behind a real OpenSSH tunnel, resume across a
//! dropped tunnel and a Host restart (CSBU WorkMate).
//!
//! Runs the real `aioncore --headless` binary and a throwaway user-level `sshd`
//! on a high loopback port. Its config, host key, client key and
//! authorized_keys all live in a temp dir, so no system SSH config is read or
//! changed. The client side is system `ssh -N -L`, exactly as in
//! `docs/host/remote-ssh.md`.
//!
//! Checked:
//! 1. live events arrive through the tunnel;
//! 2. drop the tunnel, publish while away, reconnect: `resume` → `ok`, missed
//!    event replayed, other user's events never replayed;
//! 3. kill (SIGKILL) and restart the Host: same epoch, `ok`, sequences continue
//!    (no reuse);
//! 4. lose the journal and restart: `gap` with a new epoch.
//!
//! Skips (passes with a SKIP line) when `sshd` / `ssh` / `ssh-keygen` are
//! missing, unless `WORKMATE_E14_SSH_REQUIRED=1`, which CI sets.
//! Env:
//! - `WORKMATE_E14_SSHD`: sshd path (default `/usr/sbin/sshd`).
//! - `WORKMATE_E14_SSHD_LIBEXEC`: directory holding `sshd-session` and
//!   `sshd-auth`, for an sshd that is not installed at its compiled-in prefix.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::net::TcpListener as StdListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite;

const ADMIN_PW: &str = "e14-admin-pass-0001";
const BOB_PW: &str = "e14-bob-pass-0002";

/// Child process killed (and reaped) on drop.
struct Proc {
    name: &'static str,
    child: Option<Child>,
}

impl Proc {
    fn new(name: &'static str, child: Child) -> Self {
        Self {
            name,
            child: Some(child),
        }
    }

    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn assert_running(&mut self) {
        if let Some(c) = self.child.as_mut()
            && let Ok(Some(status)) = c.try_wait()
        {
            panic!("{} exited early: {status}", self.name);
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

fn free_port() -> u16 {
    StdListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|p| p.join(bin)).find(|p| p.is_file()))
}

struct Tools {
    sshd: PathBuf,
    ssh: PathBuf,
    ssh_keygen: PathBuf,
}

fn tools() -> Option<Tools> {
    let sshd = std::env::var_os("WORKMATE_E14_SSHD")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/sbin/sshd"));
    let found = Tools {
        ssh: which("ssh")?,
        ssh_keygen: which("ssh-keygen")?,
        sshd: sshd.is_file().then_some(sshd)?,
    };
    Some(found)
}

fn current_user() -> String {
    let out = Command::new("id").arg("-un").output().expect("id -un");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn keygen(tools: &Tools, path: &Path) {
    let status = Command::new(&tools.ssh_keygen)
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "ssh-keygen failed");
}

/// User-level sshd on a loopback high port; everything under `dir`.
fn start_sshd(tools: &Tools, dir: &Path, port: u16) -> Proc {
    keygen(tools, &dir.join("host_key"));
    keygen(tools, &dir.join("client_key"));
    std::fs::copy(dir.join("client_key.pub"), dir.join("authorized_keys")).unwrap();
    let mut cfg = format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {hk}\nPidFile {pid}\nAuthorizedKeysFile {ak}\n\
         PasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes\n\
         PermitRootLogin prohibit-password\nUsePAM no\nStrictModes no\nAllowTcpForwarding local\n\
         PermitTTY no\nX11Forwarding no\nAllowAgentForwarding no\nLogLevel VERBOSE\n",
        hk = dir.join("host_key").display(),
        pid = dir.join("sshd.pid").display(),
        ak = dir.join("authorized_keys").display(),
    );
    if let Some(libexec) = std::env::var_os("WORKMATE_E14_SSHD_LIBEXEC") {
        let libexec = PathBuf::from(libexec);
        cfg.push_str(&format!(
            "SshdSessionPath {}\nSshdAuthPath {}\n",
            libexec.join("sshd-session").display(),
            libexec.join("sshd-auth").display()
        ));
    }
    let cfg_path = dir.join("sshd_config");
    std::fs::write(&cfg_path, cfg).unwrap();
    let log = std::fs::File::create(dir.join("sshd.log")).unwrap();
    let child = Command::new(&tools.sshd)
        .arg("-D")
        .arg("-f")
        .arg(&cfg_path)
        .arg("-E")
        .arg(dir.join("sshd.log"))
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("spawn sshd");
    let mut proc = Proc::new("sshd", child);
    wait_port(port, &mut proc, Duration::from_secs(15));
    proc
}

fn start_tunnel(tools: &Tools, dir: &Path, sshd_port: u16, local_port: u16, host_port: u16) -> Proc {
    let child = Command::new(&tools.ssh)
        .args(["-F", "/dev/null", "-N", "-T"])
        .arg("-i")
        .arg(dir.join("client_key"))
        .args([
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=no",
        ])
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", dir.join("known_hosts").display()))
        .args(["-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=5"])
        .arg("-p")
        .arg(sshd_port.to_string())
        .arg("-L")
        .arg(format!("127.0.0.1:{local_port}:127.0.0.1:{host_port}"))
        .arg(format!("{}@127.0.0.1", current_user()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(dir.join(format!("ssh-{local_port}.log"))).unwrap())
        .spawn()
        .expect("spawn ssh");
    let mut proc = Proc::new("ssh tunnel", child);
    wait_port(local_port, &mut proc, Duration::from_secs(20));
    proc
}

fn wait_port(port: u16, proc: &mut Proc, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        proc.assert_running();
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "{} never listened on {port}", proc.name);
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn aioncore(data_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aioncore"));
    cmd.arg("--data-dir").arg(data_dir).env("HOME", data_dir.join("home"));
    cmd
}

fn set_password(data_dir: &Path, args: &[&str], password: &str) {
    use std::io::Write;
    let mut child = aioncore(data_dir)
        .arg("user")
        .args(args)
        .arg("--password-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{password}").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "aioncore user {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn start_host(data_dir: &Path, port: u16) -> Proc {
    let mut child = aioncore(data_dir)
        .args(["--headless", "--host", "127.0.0.1", "--port"])
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(data_dir.join(format!("host-{port}-{}.err", now_ms()))).unwrap())
        .spawn()
        .expect("spawn aioncore --headless");
    let stdout = child.stdout.take().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut ready = Some(ready_tx);
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("WORKMATE_HOST_READY")
                && let Some(tx) = ready.take()
            {
                let _ = tx.send(line);
            }
        }
    });
    let line = ready_rx
        .recv_timeout(Duration::from_secs(120))
        .expect("headless Host printed no WORKMATE_HOST_READY line");
    assert!(line.contains(&format!("http://127.0.0.1:{port}/health")), "{line}");
    let mut proc = Proc::new("headless Host", child);
    wait_port(port, &mut proc, Duration::from_secs(30));
    proc
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

// ---------------------------------------------------------------- HTTP

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Value,
}

fn decode_chunked(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(pos) = raw.windows(2).position(|w| w == b"\r\n") {
        let size_line = std::str::from_utf8(&raw[..pos]).unwrap_or("0");
        let size = usize::from_str_radix(size_line.split(';').next().unwrap().trim(), 16).unwrap_or(0);
        raw = &raw[pos + 2..];
        if size == 0 || raw.len() < size {
            break;
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[(size + 2).min(raw.len())..];
    }
    out
}

async fn http(port: u16, method: &str, path: &str, headers: &[(&str, String)], body: Option<Value>) -> HttpResponse {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    req.push_str("\r\n");
    req.push_str(&body);
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut raw))
        .await
        .expect("HTTP response timed out")
        .unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("HTTP head");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let mut payload = raw[split + 4..].to_vec();
    if headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"))
    {
        payload = decode_chunked(&payload);
    }
    let body = serde_json::from_slice(&payload).unwrap_or(Value::Null);
    HttpResponse { status, headers, body }
}

#[derive(Clone)]
struct Session {
    token: String,
    csrf: String,
}

async fn login(port: u16, username: &str, password: &str) -> Session {
    let status = http(port, "GET", "/api/auth/status", &[], None).await;
    let csrf = status
        .headers
        .iter()
        .filter(|(k, _)| k == "set-cookie")
        .find_map(|(_, v)| v.strip_prefix("aionui-csrf-token="))
        .map(|v| v.split(';').next().unwrap().to_owned())
        .expect("csrf cookie");
    let resp = http(
        port,
        "POST",
        "/login",
        &[],
        Some(json!({"username": username, "password": password})),
    )
    .await;
    assert_eq!(resp.status, 200, "login {username}: {}", resp.body);
    Session {
        token: resp.body["token"].as_str().expect("token").to_owned(),
        csrf,
    }
}

/// Create a conversation; the Host broadcasts `conversation.listChanged` for it.
async fn create_conversation(port: u16, s: &Session) -> String {
    let resp = http(
        port,
        "POST",
        "/api/conversations",
        &[
            ("Authorization", format!("Bearer {}", s.token)),
            ("x-csrf-token", s.csrf.clone()),
            ("Cookie", format!("aionui-csrf-token={}", s.csrf)),
        ],
        Some(json!({"type": "acp", "extra": {}})),
    )
    .await;
    assert!(matches!(resp.status, 200 | 201), "create conversation: {}", resp.body);
    resp.body["data"]["id"].as_str().expect("conversation id").to_owned()
}

// ---------------------------------------------------------------- WebSocket

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    tungstenite::Message,
>;

struct Ws {
    sink: WsSink,
    rx: mpsc::UnboundedReceiver<Option<Value>>,
    seen: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cursor {
    epoch: u64,
    sequence: u64,
}

fn cursor_of(v: &Value) -> Option<Cursor> {
    Some(Cursor {
        epoch: v["cursor"]["epoch"].as_u64()?,
        sequence: v["cursor"]["sequence"].as_u64()?,
    })
}

async fn ws_connect(port: u16, token: &str) -> Ws {
    let request = tungstenite::http::Request::builder()
        .uri(format!("ws://127.0.0.1:{port}/ws"))
        .header("Host", format!("127.0.0.1:{port}"))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
        .header("Authorization", format!("Bearer {token}"))
        .body(())
        .unwrap();
    let (ws, _) = tokio::time::timeout(Duration::from_secs(20), tokio_tungstenite::connect_async(request))
        .await
        .expect("ws connect timed out")
        .expect("ws connect");
    let (sink, mut stream) = ws.split();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(tungstenite::Message::Text(t)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        let _ = tx.send(Some(v));
                    }
                }
                Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let _ = tx.send(None);
    });
    Ws {
        sink,
        rx,
        seen: Vec::new(),
    }
}

impl Ws {
    /// Wait for a frame matching `pred`; frames are kept in `seen`.
    async fn wait_for(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let frame = tokio::time::timeout_at(deadline, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
            match frame {
                Some(Some(v)) => {
                    self.seen.push(v.clone());
                    if pred(&v) {
                        return v;
                    }
                }
                _ => panic!("socket closed while waiting for {what}"),
            }
        }
    }

    /// Wait until the socket closes (tunnel dropped).
    async fn wait_closed(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Err(_) => panic!("socket did not close after the tunnel dropped"),
                Ok(Some(Some(v))) => self.seen.push(v),
                Ok(_) => return,
            }
        }
    }

    async fn resume(&mut self, cursor: Option<Cursor>) -> (Value, Vec<Value>) {
        let mut msg = json!({"name": "resume", "data": {}});
        if let Some(c) = cursor {
            msg["cursor"] = json!({"epoch": c.epoch, "sequence": c.sequence});
        }
        self.sink
            .send(tungstenite::Message::Text(msg.to_string().into()))
            .await
            .unwrap();
        let start = self.seen.len();
        let complete = self
            .wait_for("resume.complete", |v| v["name"] == "resume.complete")
            .await;
        let replay = self.seen[start..self.seen.len() - 1].to_vec();
        (complete, replay)
    }

    fn max_cursor(&self) -> Option<Cursor> {
        self.seen
            .iter()
            .filter_map(cursor_of)
            .max_by_key(|c| (c.epoch, c.sequence))
    }
}

fn is_list_changed_for(v: &Value, conversation_id: &str) -> bool {
    v["name"] == "conversation.listChanged" && v["data"]["conversation_id"] == conversation_id
}

fn status_of(complete: &Value) -> &str {
    complete["data"]["status"].as_str().unwrap_or("")
}

fn complete_cursor(complete: &Value) -> Cursor {
    Cursor {
        epoch: complete["data"]["epoch"].as_u64().unwrap(),
        sequence: complete["data"]["sequence"].as_u64().unwrap(),
    }
}

// ---------------------------------------------------------------- test

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_host_over_ssh_tunnel_resumes_across_drop_and_restart() {
    let required = std::env::var("WORKMATE_E14_SSH_REQUIRED").as_deref() == Ok("1");
    let Some(tools) = tools() else {
        assert!(
            !required,
            "WORKMATE_E14_SSH_REQUIRED=1 but sshd/ssh/ssh-keygen were not found"
        );
        eprintln!("SKIP: sshd, ssh or ssh-keygen not available");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let ssh_dir = tmp.path().join("ssh");
    let data_dir = tmp.path().join("host-data");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    std::fs::create_dir_all(data_dir.join("home")).unwrap();
    let journal = data_dir.join("realtime").join("events.ndjson");

    let (sshd_port, host_port, local_port) = (free_port(), free_port(), free_port());
    let _sshd = start_sshd(&tools, &ssh_dir, sshd_port);

    // Accounts (password via stdin, never argv: D14-3).
    set_password(&data_dir, &["set-password"], ADMIN_PW);
    set_password(&data_dir, &["create", "--username", "bob"], BOB_PW);

    // ---- 1. Host up, tunnel up, live events through the tunnel.
    let mut host = start_host(&data_dir, host_port);
    let mut tunnel = start_tunnel(&tools, &ssh_dir, sshd_port, local_port, host_port);
    let admin = login(local_port, "admin", ADMIN_PW).await;
    let bob = login(local_port, "bob", BOB_PW).await;

    let mut ws_admin = ws_connect(local_port, &admin.token).await;
    let mut ws_bob = ws_connect(local_port, &bob.token).await;
    let (first, _) = ws_admin.resume(None).await;
    let (_, _) = ws_bob.resume(None).await;
    let epoch = complete_cursor(&first).epoch;

    let conv1 = create_conversation(local_port, &admin).await;
    let live = ws_admin
        .wait_for("live listChanged", |v| is_list_changed_for(v, &conv1))
        .await;
    assert_eq!(cursor_of(&live).unwrap().epoch, epoch);
    let admin_cursor = ws_admin.max_cursor().unwrap();
    let bob_cursor = complete_cursor(&ws_bob.resume(None).await.0);

    // ---- 2. Drop the tunnel; the Host keeps publishing while the client is away.
    tunnel.kill();
    ws_admin.wait_closed().await;
    ws_bob.wait_closed().await;
    let conv2 = create_conversation(host_port, &admin).await; // Host-side activity

    let mut tunnel = start_tunnel(&tools, &ssh_dir, sshd_port, local_port, host_port);
    let mut ws_admin = ws_connect(local_port, &admin.token).await;
    let (complete, replay) = ws_admin.resume(Some(admin_cursor)).await;
    assert_eq!(status_of(&complete), "ok", "resume after tunnel drop: {complete}");
    assert_eq!(complete_cursor(&complete).epoch, epoch);
    assert!(
        replay.iter().any(|v| is_list_changed_for(v, &conv2)),
        "missed event not replayed: {replay:?}"
    );
    assert!(
        replay
            .iter()
            .all(|v| cursor_of(v).unwrap().sequence > admin_cursor.sequence),
        "replay must start after the client cursor"
    );

    // Bob resumes too: own/global events only, never admin's conversation ids.
    let mut ws_bob = ws_connect(local_port, &bob.token).await;
    let (bob_complete, bob_replay) = ws_bob.resume(Some(bob_cursor)).await;
    assert_eq!(status_of(&bob_complete), "ok", "{bob_complete}");
    let bob_text = serde_json::to_string(&bob_replay).unwrap();
    assert!(
        !bob_text.contains(&conv1) && !bob_text.contains(&conv2),
        "another user's events were replayed to bob: {bob_text}"
    );
    drop(ws_bob);

    // ---- 3. Crash + restart the Host: same epoch, resume ok, no sequence reuse.
    let before_restart = complete_cursor(&complete);
    tunnel.kill();
    ws_admin.wait_closed().await;
    host.kill();
    let mut host = start_host(&data_dir, host_port);
    let mut tunnel = start_tunnel(&tools, &ssh_dir, sshd_port, local_port, host_port);
    let admin = login(local_port, "admin", ADMIN_PW).await;
    let mut ws_admin = ws_connect(local_port, &admin.token).await;
    let (complete, replay) = ws_admin.resume(Some(before_restart)).await;
    assert_eq!(status_of(&complete), "ok", "resume after Host restart: {complete}");
    let after_restart = complete_cursor(&complete);
    assert_eq!(after_restart.epoch, epoch, "durable journal keeps the epoch");
    assert!(after_restart.sequence >= before_restart.sequence);
    assert!(
        replay
            .iter()
            .all(|v| cursor_of(v).unwrap().sequence > before_restart.sequence),
        "no replayed event may reuse an already-seen sequence"
    );
    let conv3 = create_conversation(local_port, &admin).await;
    let live = ws_admin
        .wait_for("live listChanged after restart", |v| is_list_changed_for(v, &conv3))
        .await;
    let live_cursor = cursor_of(&live).unwrap();
    assert_eq!(live_cursor.epoch, epoch);
    assert!(
        live_cursor.sequence > before_restart.sequence,
        "sequence reused after restart: {live_cursor:?} <= {before_restart:?}"
    );
    let last_seen = ws_admin.max_cursor().unwrap();

    // ---- 4. Journal lost across a restart: gap with a new epoch.
    tunnel.kill();
    ws_admin.wait_closed().await;
    host.kill();
    std::fs::write(&journal, b"").unwrap();
    let _host = start_host(&data_dir, host_port);
    let _tunnel = start_tunnel(&tools, &ssh_dir, sshd_port, local_port, host_port);
    let mut ws_admin = ws_connect(local_port, &admin.token).await;
    let (complete, _) = ws_admin.resume(Some(last_seen)).await;
    assert_eq!(status_of(&complete), "gap", "lost journal must be a gap: {complete}");
    assert!(complete_cursor(&complete).epoch > epoch, "{complete}");
}
