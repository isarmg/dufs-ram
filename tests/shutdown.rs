#[path = "support/fixtures.rs"]
mod fixtures;

use assert_cmd::cargo::cargo_bin;
use assert_fs::TempDir;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{
    blocking::Client,
    header::{CONTENT_TYPE, COOKIE, SET_COOKIE},
};
use rstest::rstest;
use rusqlite::{Connection, params};
use serde_json::Value;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::{
    error::Error,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream},
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    sync::Mutex,
    thread::JoinHandle,
    thread::sleep,
    time::{Duration, Instant},
};
use uuid::Uuid;

use fixtures::{TEST_ACCOUNT, TEST_PASSWORD, TEST_USER, UPLOAD_STAGE_DIRECTORY, read_bound_url};

const FILE_SIZE: usize = 8 * 1024 * 1024;
static SHUTDOWN_TEST_LOCK: Mutex<()> = Mutex::new(());

struct ChildGuard {
    child: Child,
    stdout_drain: Option<JoinHandle<()>>,
}

struct BrowserSession {
    cookie: String,
    csrf_token: String,
}

impl ChildGuard {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(stdout_drain) = self.stdout_drain.take() {
            let _ = stdout_drain.join();
        }
    }
}

#[rstest]
#[case("TERM")]
#[case("INT")]
fn signal_drains_an_active_download_and_stops_accepting(
    #[case] signal: &str,
) -> Result<(), Box<dyn Error>> {
    let _test_guard = SHUTDOWN_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = TempDir::new()?;
    let state_dir = TempDir::new()?;
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))?;
    std::fs::File::create(temp.path().join("large.bin"))?.set_len(FILE_SIZE as u64)?;
    let mut child = Command::new(cargo_bin!())
        .arg(temp.path())
        .args(["--bind", "127.0.0.1"])
        .args(["--port", "0"])
        .args(["--auth", TEST_ACCOUNT])
        .arg("--state-dir")
        .arg(state_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let port = read_bound_url(&mut child)?
        .port()
        .ok_or("Printed URL has no port")?;
    let stdout_drain = child.stdout.take().map(|mut stdout| {
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut stdout, &mut std::io::sink());
        })
    });
    let mut child = ChildGuard {
        child,
        stdout_drain,
    };
    wait_for_port(port)?;

    let session = login(port)?;
    let mut download = open_slow_download(port, &session.cookie)?;
    let (expected_length, body_already_read) = read_response_headers(&mut download)?;
    assert_eq!(expected_length, FILE_SIZE);
    assert!(body_already_read < FILE_SIZE);

    let signal_status = Command::new("kill")
        .args([format!("-{signal}"), child.id().to_string()])
        .status()?;
    assert!(signal_status.success());

    // With a deliberately tiny receive buffer, the response cannot have been
    // buffered in full. The process must remain alive while it drains this
    // connection, but its listening socket must disappear promptly.
    sleep(Duration::from_millis(150));
    assert!(child.try_wait()?.is_none());
    wait_until_not_accepting(port)?;

    let mut remaining = 0usize;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = download.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        remaining += count;
    }
    assert_eq!(body_already_read + remaining, expected_length);

    let status = wait_for_exit(&mut child, Duration::from_secs(5))?;
    assert!(status.success(), "server exited with {status}");
    Ok(())
}

#[test]
fn shutdown_deadline_checkpoints_a_stalled_upload_before_exit() -> Result<(), Box<dyn Error>> {
    const DECLARED_SIZE: usize = 21 * 1024 * 1024;
    const SENT_SIZE: usize = 20 * 1024 * 1024;

    let _test_guard = SHUTDOWN_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = TempDir::new()?;
    let state_dir = TempDir::new()?;
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))?;
    let state_db = state_dir.path().join("state.sqlite3");
    let mut child = Command::new(cargo_bin!())
        .arg(temp.path())
        .args(["--bind", "127.0.0.1"])
        .args(["--port", "0"])
        .args(["--auth", TEST_ACCOUNT])
        .args(["--min-free-space", "0"])
        .args(["--upload-idle-timeout", "120"])
        .args(["--upload-total-timeout", "3600"])
        .arg("--state-dir")
        .arg(state_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let port = read_bound_url(&mut child)?
        .port()
        .ok_or("Printed URL has no port")?;
    let stdout_drain = child.stdout.take().map(|mut stdout| {
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut stdout, &mut std::io::sink());
        })
    });
    let mut child = ChildGuard {
        child,
        stdout_drain,
    };
    wait_for_port(port)?;
    let session = login(port)?;

    let mut upload = TcpStream::connect(("127.0.0.1", port))?;
    upload.set_write_timeout(Some(Duration::from_secs(10)))?;
    let upload_id = Uuid::new_v4();
    write!(
        upload,
        "PUT /forced-upload.bin HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Origin: http://127.0.0.1:{port}\r\n\
         Sec-Fetch-Site: same-origin\r\n\
         Cookie: {}\r\n\
         X-Dufs-Csrf-Token: {}\r\n\
         X-Dufs-Upload-Id: {upload_id}\r\n\
         X-Dufs-Upload-Length: {DECLARED_SIZE}\r\n\
         Content-Length: {DECLARED_SIZE}\r\n\
         Connection: close\r\n\r\n",
        session.cookie, session.csrf_token,
    )?;
    let chunk = vec![0xA5; 64 * 1024];
    for _ in 0..SENT_SIZE / chunk.len() {
        upload.write_all(&chunk)?;
    }
    upload.flush()?;
    wait_for_staged_upload(temp.path(), SENT_SIZE as u64)?;

    let shutdown_started = Instant::now();
    let signal_status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()?;
    assert!(signal_status.success());
    wait_until_not_accepting(port)?;
    let status = wait_for_exit(&mut child, Duration::from_secs(40))?;
    assert!(status.success(), "server exited with {status}");
    assert!(
        shutdown_started.elapsed() >= Duration::from_secs(29),
        "forced upload cancellation happened before the 30-second grace period"
    );

    assert!(!temp.path().join("forced-upload.bin").exists());
    let stage = staged_upload_path(temp.path())?;
    assert_eq!(std::fs::metadata(stage)?.len(), SENT_SIZE as u64);
    let connection = Connection::open(state_db)?;
    let durable_offset: i64 = connection.query_row(
        "SELECT durable_offset FROM upload_sessions WHERE upload_id = ?1",
        params![upload_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    assert_eq!(durable_offset, SENT_SIZE as i64);
    drop(upload);
    Ok(())
}

fn login(port: u16) -> Result<BrowserSession, Box<dyn Error>> {
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let form = form_urlencoded::Serializer::new(String::new())
        .append_pair("username", TEST_USER)
        .append_pair("password", TEST_PASSWORD)
        .finish();
    let response = client
        .post(format!("http://127.0.0.1:{port}/__dufs__/login"))
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form)
        .send()?;
    assert!(response.status().is_redirection());
    let cookie = response
        .headers()
        .get(SET_COOKIE)
        .ok_or("login response has no cookie")?
        .to_str()?
        .split(';')
        .next()
        .ok_or("empty set-cookie header")?
        .to_string();
    let page = client
        .get(format!("http://127.0.0.1:{port}/"))
        .header(COOKIE, &cookie)
        .send()?
        .error_for_status()?
        .text()?;
    let marker = "<template id=\"index-data\">";
    let start = page.find(marker).ok_or("page has no index data")? + marker.len();
    let end = start
        + page[start..]
            .find("</template>")
            .ok_or("invalid index data")?;
    let data: Value = serde_json::from_slice(&STANDARD.decode(&page[start..end])?)?;
    let csrf_token = data["csrf_token"]
        .as_str()
        .ok_or("page has no CSRF token")?
        .to_string();
    Ok(BrowserSession { cookie, csrf_token })
}

fn staged_upload_path(root: &std::path::Path) -> Result<std::path::PathBuf, Box<dyn Error>> {
    let stage = std::fs::read_dir(root.join(UPLOAD_STAGE_DIRECTORY))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".dufs-upload-") && name.ends_with(".part"))
        })
        .ok_or("upload staging file was not created")?;
    Ok(stage)
}

fn wait_for_staged_upload(root: &std::path::Path, expected: u64) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if staged_upload_path(root)
            .ok()
            .and_then(|stage| std::fs::metadata(stage).ok())
            .is_some_and(|metadata| metadata.len() == expected)
        {
            return Ok(());
        }
        sleep(Duration::from_millis(25));
    }
    Err(format!("upload staging file did not reach {expected} bytes").into())
}

fn open_slow_download(port: u16, cookie: &str) -> Result<TcpStream, Box<dyn Error>> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_recv_buffer_size(4096)?;
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    socket.connect(&SockAddr::from(addr))?;
    let mut stream: TcpStream = socket.into();
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    write!(
        stream,
        "GET /large.bin HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Cookie: {cookie}\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;
    Ok(stream)
}

fn read_response_headers(stream: &mut TcpStream) -> Result<(usize, usize), Box<dyn Error>> {
    let mut received = Vec::new();
    let header_end = loop {
        let mut buffer = [0u8; 1024];
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err("connection closed before response headers".into());
        }
        received.extend_from_slice(&buffer[..count]);
        if let Some(index) = received.windows(4).position(|value| value == b"\r\n\r\n") {
            break index + 4;
        }
        if received.len() > 16 * 1024 {
            return Err("response headers are too large".into());
        }
    };

    let headers = std::str::from_utf8(&received[..header_end])?;
    assert!(headers.starts_with("HTTP/1.1 200 "));
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>())
        })
        .ok_or("response has no content-length")??;
    Ok((content_length, received.len() - header_end))
}

fn wait_for_port(port: u16) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(25));
    }
    Err(format!("server did not listen on port {port}").into())
}

fn wait_until_not_accepting(port: u16) -> Result<(), Box<dyn Error>> {
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_err() {
            return Ok(());
        }
        sleep(Duration::from_millis(20));
    }
    Err(format!("server continued accepting connections on port {port}").into())
}

fn wait_for_exit(
    child: &mut ChildGuard,
    timeout: Duration,
) -> Result<std::process::ExitStatus, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        sleep(Duration::from_millis(20));
    }
    Err("server did not exit after draining the active request".into())
}
