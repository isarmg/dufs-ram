use assert_fs::fixture::TempDir;
use assert_fs::prelude::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use indexmap::IndexSet;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{CONTENT_TYPE, COOKIE, SET_COOKIE};
use reqwest::{IntoUrl, Method, Url};
use rstest::fixture;
use sarmg_admin_auth::normalize_administrator_username;
use sarmg_contracts::{AdministratorRole, AdministratorSession};
use serde_json::Value;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::{JoinHandle, sleep};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[allow(dead_code)]
pub type Error = Box<dyn std::error::Error>;

#[allow(dead_code)]
pub const BIN_FILE: &str = "😀.bin";
pub const TEST_USER: &str = "test-admin";
pub const TEST_PASSWORD: &str = "test-password";
pub const TEST_ACCOUNT: &str = "test-admin:$argon2id$v=19$m=19456,t=2,p=1$HdPI2G8k0h+yEgnqIt2rSw$P+MRyz7wH+b/iPY+He/9DApcy6yB9TAoo7j2JG1Smzs";
#[allow(dead_code)]
pub const UPLOAD_STAGE_DIRECTORY: &str = ".dufs-upload-stages";
#[allow(dead_code)]
pub const USER_ACCOUNT: &str = "user:$argon2id$v=19$m=19456,t=2,p=1$HdPI2G8k0h+yEgnqIt2rSw$P+MRyz7wH+b/iPY+He/9DApcy6yB9TAoo7j2JG1Smzs";
#[allow(dead_code)]
pub const ADMIN_ACCOUNT: &str = "admin:$argon2id$v=19$m=19456,t=2,p=1$HdPI2G8k0h+yEgnqIt2rSw$P+MRyz7wH+b/iPY+He/9DApcy6yB9TAoo7j2JG1Smzs";
const SESSION_COOKIE_NAME: &str = "sarmg-dufs-ram-session";
const CSRF_HEADER: &str = "x-csrf-token";

#[allow(dead_code)]
pub struct TestAuthConfig {
    _directory: TempDir,
    path: PathBuf,
}

#[allow(dead_code)]
impl TestAuthConfig {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[allow(dead_code)]
pub fn test_auth_config(accounts: &[&str]) -> TestAuthConfig {
    let directory = TempDir::new().expect("Couldn't create an auth config dir for tests");
    let path = directory.path().join("dufs.yaml");
    let contents = serde_yaml::to_string(&serde_json::json!({ "auth": accounts }))
        .expect("Couldn't serialize test accounts");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("Couldn't create a private auth config for tests");
    file.write_all(contents.as_bytes())
        .expect("Couldn't write the private auth config for tests");
    file.flush()
        .expect("Couldn't flush the private auth config for tests");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("Couldn't enforce private auth config permissions for tests");
    if let Err(error) = rustix::fs::removexattr(&path, "system.posix_acl_access") {
        assert!(
            error == rustix::io::Errno::NODATA || error == rustix::io::Errno::NOTSUP,
            "Couldn't remove an inherited auth config ACL: {error}"
        );
    }
    TestAuthConfig {
        _directory: directory,
        path,
    }
}

#[allow(dead_code)]
pub fn dufs_command(accounts: &[&str]) -> (Command, TestAuthConfig) {
    let auth_config = test_auth_config(accounts);
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!());
    command.arg("--config").arg(auth_config.path());
    (command, auth_config)
}

#[allow(dead_code)]
pub fn with_new_upload_headers(request: RequestBuilder, upload_length: u64) -> RequestBuilder {
    with_upload_headers(request, Uuid::new_v4(), upload_length)
}

#[allow(dead_code)]
pub fn with_upload_headers(
    request: RequestBuilder,
    upload_id: Uuid,
    upload_length: u64,
) -> RequestBuilder {
    request
        .header("X-Dufs-Upload-Id", upload_id.to_string())
        .header("X-Dufs-Upload-Length", upload_length)
}

#[allow(dead_code)]
pub fn with_resume_upload_headers(
    request: RequestBuilder,
    upload_id: Uuid,
    upload_length: u64,
    upload_offset: u64,
) -> RequestBuilder {
    with_upload_headers(request, upload_id, upload_length)
        .header("X-Dufs-Upload-Offset", upload_offset)
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadPreflightTarget {
    pub path: String,
    pub exists: bool,
    pub revision: Option<String>,
    pub replaceable: bool,
}

/// Ask the browser upload API for the current target revision.
///
/// Tests which intentionally replace an existing entry must use the returned
/// revision instead of silently opting into an unconditional overwrite.
#[allow(dead_code)]
pub fn preflight_upload_target(
    server: &TestServer,
    logical_path: &str,
) -> Result<UploadPreflightTarget, Error> {
    preflight_upload_target_request(
        server.request(
            Method::POST,
            server.url().join("__dufs__/api/upload/preflight")?,
        ),
        logical_path,
    )
}

#[allow(dead_code)]
pub fn preflight_upload_target_with(
    server: &TestServer,
    session: &TestSession,
    logical_path: &str,
) -> Result<UploadPreflightTarget, Error> {
    preflight_upload_target_request(
        server.request_with(
            session,
            Method::POST,
            server.url().join("__dufs__/api/upload/preflight")?,
        ),
        logical_path,
    )
}

fn preflight_upload_target_request(
    request: RequestBuilder,
    logical_path: &str,
) -> Result<UploadPreflightTarget, Error> {
    let response = request
        .header(CONTENT_TYPE, "application/json")
        .body(serde_json::json!({ "paths": [logical_path] }).to_string())
        .send()?;
    if response.status() != reqwest::StatusCode::OK {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(format!("upload preflight returned {status}: {body}").into());
    }
    let body: Value = serde_json::from_str(&response.text()?)?;
    let targets = body
        .get("targets")
        .and_then(Value::as_array)
        .ok_or("upload preflight response is missing targets")?;
    if targets.len() != 1 {
        return Err(format!(
            "single-target upload preflight returned {} targets",
            targets.len()
        )
        .into());
    }
    let target = &targets[0];
    let returned_path = target
        .get("path")
        .and_then(Value::as_str)
        .ok_or("upload preflight target is missing path")?;
    let exists = target
        .get("exists")
        .and_then(Value::as_bool)
        .ok_or("upload preflight target is missing exists")?;
    let revision = match target.get("revision") {
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Null) => None,
        _ => return Err("upload preflight target has an invalid revision".into()),
    };
    let replaceable = target
        .get("replaceable")
        .and_then(Value::as_bool)
        .ok_or("upload preflight target is missing replaceable")?;
    Ok(UploadPreflightTarget {
        path: returned_path.to_string(),
        exists,
        revision,
        replaceable,
    })
}

#[allow(dead_code)]
pub fn with_upload_overwrite_headers(
    request: RequestBuilder,
    upload_id: Uuid,
    upload_length: u64,
    revision: &str,
) -> RequestBuilder {
    with_upload_headers(request, upload_id, upload_length)
        .header("X-Dufs-Upload-Overwrite", "true")
        .header("X-Dufs-Target-Revision", revision)
}

#[allow(dead_code)]
pub fn with_new_upload_overwrite_headers(
    request: RequestBuilder,
    upload_length: u64,
    revision: &str,
) -> RequestBuilder {
    with_upload_overwrite_headers(request, Uuid::new_v4(), upload_length, revision)
}

/// File names used by the test fixtures.
#[allow(dead_code)]
pub static FILES: &[&str] = &["test.txt", "test.html", "index.html", BIN_FILE];

/// Directory name used when testing a missing directory.
#[allow(dead_code)]
pub static DIR_NOT_FOUND: &str = "dir-no-found/";

/// Directory name used when testing a directory without `index.html`.
#[allow(dead_code)]
pub static DIR_NO_INDEX: &str = "dir-no-index/";

/// Directory names used by the test fixtures.
#[allow(dead_code)]
pub static DIRECTORIES: &[&str] = &["dir1/", "dir2/", DIR_NO_INDEX];

/// Test fixture which creates a temporary directory with a few files and directories inside.
/// The directories also contain files.
#[fixture]
#[allow(dead_code)]
pub fn tmpdir() -> TempDir {
    let tmpdir = assert_fs::TempDir::new().expect("Couldn't create a temp dir for tests");
    for file in FILES {
        if *file == BIN_FILE {
            tmpdir.child(file).write_binary(b"bin\0\x00123").unwrap();
        } else {
            tmpdir
                .child(file)
                .write_str(&format!("This is {file}"))
                .unwrap();
        }
    }
    for directory in DIRECTORIES {
        for file in FILES {
            if *directory == DIR_NO_INDEX && *file == "index.html" {
                continue;
            }
            if *file == BIN_FILE {
                tmpdir
                    .child(format!("{directory}{file}"))
                    .write_binary(b"bin\0\x00123")
                    .unwrap();
            } else {
                tmpdir
                    .child(format!("{directory}{file}"))
                    .write_str(&format!("This is {directory}{file}"))
                    .unwrap();
            }
        }
    }
    tmpdir
        .child("content-types/bin.tar")
        .write_binary(b"\x7f\x45\x4c\x46\x02\x01\x00\x00")
        .unwrap();
    tmpdir
        .child("content-types/bin")
        .write_binary(b"\x7f\x45\x4c\x46\x02\x01\x00\x00")
        .unwrap();
    tmpdir
        .child("content-types/file-utf8.txt")
        .write_str("世界")
        .unwrap();
    tmpdir
        .child("content-types/file-gbk.txt")
        .write_binary(b"\xca\xc0\xbd\xe7")
        .unwrap();
    tmpdir
        .child("content-types/file")
        .write_str("世界")
        .unwrap();

    tmpdir
}

/// Run dufs with a temporary directory, a dynamically assigned port, and
/// optional arguments, then wait until it reports its bound URL.
#[fixture]
#[allow(dead_code)]
pub fn server<I, A>(
    #[default(&[] as &[&str])] args: I,
    #[default(&[TEST_ACCOUNT] as &[&str])] accounts: A,
) -> TestServer
where
    I: IntoIterator,
    I::Item: AsRef<std::ffi::OsStr>,
    A: IntoIterator,
    A::Item: AsRef<str>,
{
    server_in_mode(args, accounts, true)
}

#[allow(dead_code)]
pub fn production_server<I, A>(args: I, accounts: A) -> TestServer
where
    I: IntoIterator,
    I::Item: AsRef<std::ffi::OsStr>,
    A: IntoIterator,
    A::Item: AsRef<str>,
{
    server_in_mode(args, accounts, false)
}

fn server_in_mode<I, A>(args: I, accounts: A, development: bool) -> TestServer
where
    I: IntoIterator,
    I::Item: AsRef<std::ffi::OsStr>,
    A: IntoIterator,
    A::Item: AsRef<str>,
{
    let tmpdir = tmpdir();
    let args = args
        .into_iter()
        .map(|value| value.as_ref().to_os_string())
        .collect::<Vec<_>>();
    let accounts = accounts
        .into_iter()
        .map(|value| value.as_ref().to_owned())
        .collect::<Vec<_>>();
    let has_config = args.iter().any(|value| {
        value.to_str().is_some_and(|value| {
            value == "--config" || value == "-c" || value.starts_with("--config=")
        })
    });
    let has_min_free_space = args.iter().any(|value| {
        value.to_str().is_some_and(|value| {
            value == "--min-free-space" || value.starts_with("--min-free-space=")
        })
    });
    let has_state_dir = args.iter().any(|value| {
        value
            .to_str()
            .is_some_and(|value| value == "--state-dir" || value.starts_with("--state-dir="))
    });
    let auth_config = if has_config {
        None
    } else {
        let account_refs = accounts.iter().map(String::as_str).collect::<Vec<_>>();
        Some(test_auth_config(&account_refs))
    };
    let use_default_auth = auth_config.is_some()
        && accounts.len() == 1
        && accounts
            .first()
            .is_some_and(|account| account == TEST_ACCOUNT);
    let automatic_state_dir = if has_state_dir {
        None
    } else {
        let state_dir = TempDir::new().expect("Couldn't create a state dir for tests");
        std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("Couldn't make the test state dir private");
        Some(state_dir)
    };
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!());
    command.arg(tmpdir.path()).arg("-p").arg("0").args(&args);
    if development {
        command.arg("--development");
    }
    if let Some(auth_config) = auth_config.as_ref() {
        command.arg("--config").arg(auth_config.path());
    }
    if !has_min_free_space {
        command.args(["--min-free-space", "0"]);
    }
    if let Some(state_dir) = &automatic_state_dir {
        command.arg("--state-dir").arg(state_dir.path());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .spawn()
        .expect("Couldn't run test binary");
    let port = read_bound_url(&mut child)
        .expect("Couldn't read dynamically assigned test port")
        .port()
        .expect("Dynamically assigned URL has no port");
    let mut server = TestServer::new_with_automatic_state_dir(
        port,
        tmpdir,
        child,
        use_default_auth,
        automatic_state_dir,
        auth_config,
    );
    if use_default_auth {
        server
            .refresh_default_session()
            .expect("Couldn't create default authenticated test session");
    }
    server
}

#[allow(dead_code)]
pub struct TestServer {
    port: u16,
    tmpdir: TempDir,
    child: Child,
    use_default_auth: bool,
    client: Client,
    default_session: Option<TestSession>,
    stdout_drain: Option<JoinHandle<()>>,
    automatic_state_dir: Option<TempDir>,
    auth_config: Option<TestAuthConfig>,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct TestSession {
    cookie: String,
    csrf_token: String,
}

#[allow(dead_code)]
impl TestSession {
    pub fn cookie(&self) -> &str {
        &self.cookie
    }

    pub fn csrf_token(&self) -> &str {
        &self.csrf_token
    }
}

#[allow(dead_code)]
impl TestServer {
    pub fn new(port: u16, tmpdir: TempDir, child: Child, use_default_auth: bool) -> Self {
        Self::new_with_automatic_state_dir(port, tmpdir, child, use_default_auth, None, None)
    }

    fn new_with_automatic_state_dir(
        port: u16,
        tmpdir: TempDir,
        mut child: Child,
        use_default_auth: bool,
        automatic_state_dir: Option<TempDir>,
        auth_config: Option<TestAuthConfig>,
    ) -> Self {
        let client = Client::builder()
            .build()
            .expect("Couldn't create test HTTP client");
        let stdout_drain = child.stdout.take().map(|mut stdout| {
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut stdout, &mut std::io::sink());
            })
        });
        Self {
            port,
            tmpdir,
            child,
            use_default_auth,
            client,
            default_session: None,
            stdout_drain,
            automatic_state_dir,
            auth_config,
        }
    }

    pub fn url(&self) -> Url {
        Url::parse(&format!("http://localhost:{}/", self.port)).unwrap()
    }

    pub fn process_id(&self) -> u32 {
        self.child.id()
    }

    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> RequestBuilder {
        self.request_with(
            self.default_session
                .as_ref()
                .expect("Default authenticated test session is unavailable"),
            method,
            url,
        )
    }

    pub fn request_with<U: IntoUrl>(
        &self,
        session: &TestSession,
        method: Method,
        url: U,
    ) -> RequestBuilder {
        let is_unsafe = matches!(
            method,
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        );
        let is_delete = method == Method::DELETE;
        let mut request = self
            .client
            .request(method, url)
            .header(COOKIE, &session.cookie);
        if is_unsafe {
            request = request
                .header("origin", self.url().origin().ascii_serialization())
                .header("sec-fetch-site", "same-origin")
                .header(CSRF_HEADER, &session.csrf_token);
        }
        if is_delete {
            request = request.header("X-Dufs-Operation-Id", Uuid::new_v4().to_string());
        }
        request
    }

    pub fn raw_request<U: IntoUrl>(&self, method: Method, url: U) -> RequestBuilder {
        self.client.request(method, url)
    }

    pub fn get<U: IntoUrl>(&self, url: U) -> Result<Response, reqwest::Error> {
        self.request(Method::GET, url).send()
    }

    pub fn get_with<U: IntoUrl>(
        &self,
        session: &TestSession,
        url: U,
    ) -> Result<Response, reqwest::Error> {
        self.request_with(session, Method::GET, url).send()
    }

    pub fn paths_from_page(&self, response: Response) -> Result<IndexSet<String>, Error> {
        let session = self
            .default_session
            .as_ref()
            .ok_or("Default authenticated test session is unavailable")?;
        self.paths_from_page_with(session, response)
    }

    pub fn paths_from_page_with(
        &self,
        session: &TestSession,
        response: Response,
    ) -> Result<IndexSet<String>, Error> {
        let page_url = response.url().clone();
        if !response.status().is_success() {
            return Err(format!("Directory page returned {}", response.status()).into());
        }
        let page_data = extract_index_data(&response.text()?)
            .ok_or("Authenticated page is missing index data")?;
        let logical_path = page_data
            .get("href")
            .and_then(Value::as_str)
            .ok_or("Authenticated page is missing its logical path")?;
        let page_parameters: Vec<(String, String)> = page_url
            .query_pairs()
            .filter(|(key, _)| matches!(key.as_ref(), "q" | "sort" | "order"))
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        let mut cursor: Option<String> = None;
        let mut paths = IndexSet::new();

        loop {
            let mut list_url = self.url().join("__dufs__/api/list")?;
            {
                let mut query = list_url.query_pairs_mut();
                query
                    .append_pair("path", logical_path)
                    .append_pair("limit", "500");
                for (key, value) in &page_parameters {
                    query.append_pair(key, value);
                }
                if let Some(cursor) = &cursor {
                    query.append_pair("cursor", cursor);
                }
            }
            let list_response = self.get_with(session, list_url)?;
            if !list_response.status().is_success() {
                let status = list_response.status();
                let body = list_response.text().unwrap_or_default();
                return Err(format!("List API returned {status}: {body}").into());
            }
            let data: Value = serde_json::from_str(&list_response.text()?)?;
            for item in data
                .get("paths")
                .and_then(Value::as_array)
                .ok_or("List API response is missing paths")?
            {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("List item is missing a name")?;
                let path_type = item
                    .get("path_type")
                    .and_then(Value::as_str)
                    .ok_or("List item is missing a path type")?;
                if path_type.ends_with("Dir") {
                    paths.insert(format!("{name}/"));
                } else {
                    paths.insert(name.to_owned());
                }
            }
            cursor = data
                .get("next_cursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(paths)
    }

    pub fn list_api(
        &self,
        logical_path: &str,
        parameters: &[(&str, &str)],
    ) -> Result<Response, Error> {
        let mut list_url = self.url().join("__dufs__/api/list")?;
        {
            let mut query = list_url.query_pairs_mut();
            query.append_pair("path", logical_path);
            for (key, value) in parameters {
                query.append_pair(key, value);
            }
        }
        Ok(self.get(list_url)?)
    }

    pub fn login(&self, username: &str, password: &str) -> Result<TestSession, Error> {
        let login_client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let login_url = self.url().join("api/v2/auth/login")?;
        let response = login_client
            .post(login_url)
            .header("origin", self.url().origin().ascii_serialization())
            .header("sec-fetch-site", "same-origin")
            // A fixture may configure max-connections=1. Closing this setup
            // request before returning keeps that sole permit from racing the
            // test's first deliberately held connection.
            .header("connection", "close")
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&serde_json::json!({
                "username": username,
                "password": password,
            }))?)
            .send()?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(format!("Login failed with status {}", response.status()).into());
        }
        let cookie = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|value| value.split(';').next())
            .find(|value| value.starts_with(&format!("{SESSION_COOKIE_NAME}=")))
            .ok_or("Login response is missing the session cookie")?
            .to_string();
        let session: AdministratorSession = serde_json::from_str(&response.text()?)?;
        session.validate()?;
        let canonical_username = normalize_administrator_username(username)?;
        if session.username != canonical_username || session.role != AdministratorRole::Admin {
            return Err("Login response contains the wrong administrator identity".into());
        }
        let csrf_token = session.csrf_token;

        Ok(TestSession { cookie, csrf_token })
    }

    pub fn path(&self) -> &std::path::Path {
        self.tmpdir.path()
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn restart_with_default_auth(&mut self) {
        self.restart_with_default_auth_args(&[] as &[&str]);
    }

    pub fn restart_with_default_auth_args<I>(&mut self, args: I)
    where
        I: IntoIterator,
        I::Item: AsRef<std::ffi::OsStr>,
    {
        assert!(self.use_default_auth);
        self.child.kill().expect("Couldn't kill test server");
        self.child.wait().expect("Couldn't wait for test server");
        if let Some(stdout_drain) = self.stdout_drain.take() {
            let _ = stdout_drain.join();
        }

        let args = args
            .into_iter()
            .map(|value| value.as_ref().to_os_string())
            .collect::<Vec<OsString>>();
        let has_state_dir = args.iter().any(|value| {
            value
                .to_str()
                .is_some_and(|value| value == "--state-dir" || value.starts_with("--state-dir="))
        });
        let has_min_free_space = args.iter().any(|value| {
            value.to_str().is_some_and(|value| {
                value == "--min-free-space" || value.starts_with("--min-free-space=")
            })
        });
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!());
        command
            .arg(self.tmpdir.path())
            .arg("--development")
            .arg("-p")
            .arg("0")
            .arg("--config")
            .arg(
                self.auth_config
                    .as_ref()
                    .expect("default-auth restart requires the fixture-owned auth config")
                    .path(),
            )
            .args(args);
        if !has_min_free_space {
            command.args(["--min-free-space", "0"]);
        }
        if !has_state_dir {
            command.arg("--state-dir").arg(
                self.automatic_state_dir
                    .as_ref()
                    .expect(
                        "restart without --state-dir requires the fixture-owned state directory",
                    )
                    .path(),
            );
        }
        self.child = command
            .stdout(Stdio::piped())
            .spawn()
            .expect("Couldn't restart test binary");
        self.port = read_bound_url(&mut self.child)
            .expect("Couldn't read dynamically assigned restart port")
            .port()
            .expect("Dynamically assigned restart URL has no port");
        self.stdout_drain = self.child.stdout.take().map(|mut stdout| {
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut stdout, &mut std::io::sink());
            })
        });
        self.refresh_default_session()
            .expect("Couldn't recreate default authenticated test session");
    }

    fn refresh_default_session(&mut self) -> Result<(), Error> {
        self.default_session = Some(self.login(TEST_USER, TEST_PASSWORD)?);
        Ok(())
    }
}

fn extract_index_data(content: &str) -> Option<Value> {
    let start_tag = "<template id=\"index-data\">";
    let start = content.find(start_tag)? + start_tag.len();
    let end = start + content[start..].find("</template>")?;
    let decoded = STANDARD.decode(&content[start..end]).ok()?;
    serde_json::from_slice(&decoded).ok()
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let pid = i32::try_from(self.child.id())
                .ok()
                .and_then(rustix::process::Pid::from_raw);
            let term_sent = pid.is_some_and(|pid| {
                rustix::process::kill_process(pid, rustix::process::Signal::TERM).is_ok()
            });
            if term_sent {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    match self.child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => sleep(Duration::from_millis(10)),
                        Err(_) => break,
                    }
                }
            }
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
        if let Some(stdout_drain) = self.stdout_drain.take() {
            let _ = stdout_drain.join();
        }
    }
}

#[allow(dead_code)]
pub fn read_bound_url(child: &mut Child) -> Result<Url, Error> {
    let mut stdout = child
        .stdout
        .take()
        .ok_or("Test server did not expose startup output")?;
    let url = loop {
        let mut line = Vec::new();
        loop {
            let mut byte = [0_u8; 1];
            if stdout.read(&mut byte)? == 0 {
                return Err("Test server exited before reporting its listen address".into());
            }
            line.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        let line = std::str::from_utf8(&line)?;
        let Some(url_start) = line.find("http") else {
            continue;
        };
        let url = Url::parse(line[url_start..].trim())?;
        if url.port().is_some() {
            break url;
        }
    };
    child.stdout = Some(stdout);
    Ok(url)
}
