use anyhow::{Context, Result, bail, ensure};
use clap::{Arg, ArgAction, ArgMatches, Command, builder::ValueParser, value_parser};
use ipnet::IpNet;
use rustix::{
    fs::fgetxattr,
    io::Errno,
    process::{getegid, geteuid},
};
use serde::{Deserialize, Deserializer};
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{File, Metadata, OpenOptions};
use std::io::Read;
use std::net::IpAddr;
use std::ops::Deref;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use crate::auth::AuthConfig;
use crate::http_logger::HttpLogger;

const DEFAULT_MAX_UPLOAD_SIZE: u64 = 100 * 1024 * 1024 * 1024;
const DEFAULT_UPLOAD_IDLE_TIMEOUT: u64 = 60;
const DEFAULT_UPLOAD_TOTAL_TIMEOUT: u64 = 24 * 60 * 60;
const DEFAULT_MAX_CONCURRENT_UPLOADS: usize = 4;
const DEFAULT_MIN_FREE_SPACE: u64 = 1024 * 1024 * 1024;
const DEFAULT_MAX_CONNECTIONS: usize = 256;
const DEFAULT_MAX_SEARCH_ENTRIES: usize = 10_000;
const MAX_SEARCH_ENTRIES: usize = 100_000;
const DEFAULT_MAX_CONCURRENT_SEARCHES: usize = 2;
const DEFAULT_REQUEST_TIMEOUT: u64 = 300;
const MAX_TIMEOUT_SECONDS: u64 = 365 * 24 * 60 * 60;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const CONFIG_FILE_TYPE_MASK: u32 = 0o170000;
const CONFIG_REGULAR_FILE_TYPE: u32 = 0o100000;
const CONFIG_PERMISSION_MASK: u32 = 0o7777;
const CONFIG_POSIX_ACL_XATTR: &str = "system.posix_acl_access";
const MAX_BIND_ADDRESSES: usize = 128;
const MAX_TRUSTED_PROXIES: usize = 128;
const STATE_DATABASE_FILE_NAME: &str = "state.sqlite3";
const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (git ",
    env!("DUFS_BUILD_GIT_SHA"),
    ")"
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ObjectIdentity {
    device: u64,
    inode: u64,
}

impl ObjectIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EntryIdentity {
    parent: ObjectIdentity,
    name: OsString,
}

#[derive(Clone, Debug)]
struct PathIdentity {
    entry_path: PathBuf,
    resolved_path: Option<PathBuf>,
    entry: EntryIdentity,
    object: Option<ObjectIdentity>,
    links: Option<u64>,
}

impl PathIdentity {
    fn inspect(path: &Path, path_name: &str) -> Result<Self> {
        Self::inspect_with_expected_object(path, path_name, None)
    }

    fn inspect_with_expected_object(
        path: &Path,
        path_name: &str,
        expected_object: Option<ObjectIdentity>,
    ) -> Result<Self> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            env::current_dir()
                .with_context(|| "Failed to determine the current directory")?
                .join(path)
        };
        let file_name = absolute.file_name().ok_or_else(|| {
            anyhow::anyhow!("{path_name} path `{}` must name a file", path.display())
        })?;
        let parent = absolute.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "{path_name} path `{}` must have a parent directory",
                path.display()
            )
        })?;
        let parent = std::fs::canonicalize(parent).with_context(|| {
            format!(
                "Failed to access {path_name} parent directory `{}`",
                parent.display()
            )
        })?;
        let parent_metadata = parent.metadata().with_context(|| {
            format!(
                "Failed to inspect {path_name} parent directory `{}`",
                parent.display()
            )
        })?;
        if !parent_metadata.is_dir() {
            bail!(
                "{path_name} parent `{}` must be a directory",
                parent.display()
            );
        }

        let entry_path = parent.join(file_name);
        let (object, links, resolved_path) = match std::fs::metadata(&entry_path) {
            Ok(metadata) => {
                let object = ObjectIdentity::from_metadata(&metadata);
                let resolved_path = std::fs::canonicalize(&entry_path).with_context(|| {
                    format!(
                        "Failed to resolve {path_name} path `{}`",
                        entry_path.display()
                    )
                })?;
                let resolved_metadata = resolved_path.metadata().with_context(|| {
                    format!(
                        "Failed to verify resolved {path_name} path `{}`",
                        resolved_path.display()
                    )
                })?;
                if ObjectIdentity::from_metadata(&resolved_metadata) != object {
                    bail!(
                        "{path_name} path `{}` changed while its identity was being inspected",
                        entry_path.display()
                    );
                }
                (
                    Some(object),
                    Some(resolved_metadata.nlink()),
                    Some(resolved_path),
                )
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None, None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to inspect {path_name} path `{}`",
                        entry_path.display()
                    )
                });
            }
        };
        if expected_object.is_some() && expected_object != object {
            bail!(
                "{path_name} path `{}` changed after it was securely read",
                entry_path.display()
            );
        }

        Ok(Self {
            entry_path,
            resolved_path,
            entry: EntryIdentity {
                parent: ObjectIdentity::from_metadata(&parent_metadata),
                name: file_name.to_os_string(),
            },
            object,
            links,
        })
    }

    fn shares_entry_or_object_with(&self, other: &Self) -> bool {
        self.entry == other.entry
            || matches!((self.object, other.object), (Some(left), Some(right)) if left == right)
    }

    fn resolves_within_shared_root(
        &self,
        shared_root: &Path,
        shared_root_object: ObjectIdentity,
    ) -> Result<bool> {
        if self.entry_path.starts_with(shared_root)
            || self
                .resolved_path
                .as_deref()
                .is_some_and(|path| path.starts_with(shared_root))
            || self.object == Some(shared_root_object)
        {
            return Ok(true);
        }

        let mut ancestor = self.entry_path.parent();
        while let Some(path) = ancestor {
            let metadata = path.metadata().with_context(|| {
                format!(
                    "Failed to inspect ancestor `{}` while checking the shared-path boundary",
                    path.display()
                )
            })?;
            if ObjectIdentity::from_metadata(&metadata) == shared_root_object {
                return Ok(true);
            }
            ancestor = path.parent();
        }
        Ok(false)
    }
}

fn ensure_outside_shared_root(
    identity: &PathIdentity,
    path_name: &str,
    shared_root: &Path,
) -> Result<()> {
    let shared_root_metadata = shared_root.metadata().with_context(|| {
        format!(
            "Failed to inspect shared path `{}` while validating {path_name}",
            shared_root.display()
        )
    })?;
    if identity.resolves_within_shared_root(
        shared_root,
        ObjectIdentity::from_metadata(&shared_root_metadata),
    )? {
        bail!(
            "{path_name} `{}` must not be inside or resolve into shared path `{}`",
            identity.entry_path.display(),
            shared_root.display()
        );
    }
    Ok(())
}

fn ensure_distinct_path_identities(
    left: &PathIdentity,
    left_name: &str,
    right: &PathIdentity,
    right_name: &str,
) -> Result<()> {
    if left.shares_entry_or_object_with(right) {
        bail!(
            "{left_name} `{}` conflicts by entry or object identity with {right_name} `{}`",
            left.entry_path.display(),
            right.entry_path.display()
        );
    }
    Ok(())
}

/// An immediate proxy whose forwarded client and scheme headers may be trusted.
///
/// The wrapped network is private so CLI, YAML, and library callers all use the
/// same parsing and canonicalization rules.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TrustedProxy(IpNet);

impl TrustedProxy {
    pub fn contains(&self, address: &IpAddr) -> bool {
        self.0.contains(address)
    }
}

impl FromStr for TrustedProxy {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let network = if let Ok(address) = value.parse::<IpAddr>() {
            IpNet::from(address)
        } else {
            value.parse::<IpNet>().map_err(|_| {
                anyhow::anyhow!("Invalid trusted proxy `{value}`: expected an IP or CIDR")
            })?
        };
        Ok(Self(network.trunc()))
    }
}

impl fmt::Display for TrustedProxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

pub fn build_cli() -> Command {
    Command::new(env!("CARGO_CRATE_NAME"))
        .version(LONG_VERSION)
        .long_version(LONG_VERSION)
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .subcommand(
            Command::new("hash-password").about("Interactively generate an Argon2id password hash"),
        )
        .arg(
            Arg::new("serve-path")
                .value_parser(value_parser!(PathBuf))
                .help("Existing directory to manage [default: .]"),
        )
        .arg(
            Arg::new("config")
                .short('c')
                .long("config")
                .value_parser(value_parser!(PathBuf))
                .help("Specify configuration file")
                .value_name("file"),
        )
        .arg(
            Arg::new("state-dir")
                .long("state-dir")
                .value_parser(value_parser!(PathBuf))
                .help("Specify the required private directory for persistent SQLite state")
                .value_name("dir"),
        )
        .arg(
            Arg::new("bind")
                .short('b')
                .long("bind")
                .value_parser(value_parser!(IpAddr))
                .help("Specify IP address to bind [default: 127.0.0.1]")
                .action(ArgAction::Append)
                .value_delimiter(',')
                .value_name("addrs"),
        )
        .arg(
            Arg::new("trusted-proxy")
                .long("trusted-proxy")
                .value_parser(ValueParser::new(parse_trusted_proxy))
                .help("Trust forwarded client/scheme headers only from this IP or CIDR")
                .action(ArgAction::Append)
                .value_delimiter(',')
                .value_name("networks"),
        )
        .arg(
            Arg::new("port")
                .short('p')
                .long("port")
                .value_parser(value_parser!(u16))
                .help("Specify port to listen on [default: 5000]")
                .value_name("port"),
        )
        .arg(
            Arg::new("log-format")
                .long("log-format")
                .value_name("format")
                .help("Customize HTTP log format"),
        )
        .arg(
            Arg::new("log-file")
                .long("log-file")
                .value_name("file")
                .value_parser(value_parser!(PathBuf))
                .help("Specify the file to save logs to instead of stderr"),
        )
        .arg(
            Arg::new("max-upload-size")
                .long("max-upload-size")
                .value_parser(value_parser!(u64))
                .value_name("bytes")
                .help("Maximum size of one upload in bytes [default: 107374182400]"),
        )
        .arg(
            Arg::new("upload-idle-timeout")
                .long("upload-idle-timeout")
                .value_parser(value_parser!(u64))
                .value_name("seconds")
                .help(
                    "Maximum idle time while receiving an upload, up to 31536000 seconds \
                     [default: 60]",
                ),
        )
        .arg(
            Arg::new("upload-total-timeout")
                .long("upload-total-timeout")
                .value_parser(value_parser!(u64))
                .value_name("seconds")
                .help("Maximum total time for one upload, up to 31536000 seconds [default: 86400]"),
        )
        .arg(
            Arg::new("max-concurrent-uploads")
                .long("max-concurrent-uploads")
                .value_parser(value_parser!(usize))
                .value_name("count")
                .help("Maximum number of concurrent uploads [default: 4]"),
        )
        .arg(
            Arg::new("min-free-space")
                .long("min-free-space")
                .value_parser(value_parser!(u64))
                .value_name("bytes")
                .help("Reserved free disk space required for uploads [default: 1073741824]"),
        )
        .arg(
            Arg::new("max-connections")
                .long("max-connections")
                .value_parser(value_parser!(usize))
                .value_name("count")
                .help("Maximum number of active client connections [default: 256]"),
        )
        .arg(
            Arg::new("max-search-entries")
                .long("max-search-entries")
                .value_parser(value_parser!(usize))
                .value_name("count")
                .help("Maximum entries examined by one search, up to 100000 [default: 10000]"),
        )
        .arg(
            Arg::new("max-concurrent-searches")
                .long("max-concurrent-searches")
                .value_parser(value_parser!(usize))
                .value_name("count")
                .help("Maximum number of concurrent directory listings and searches [default: 2]"),
        )
        .arg(
            Arg::new("request-timeout")
                .long("request-timeout")
                .value_parser(value_parser!(u64))
                .value_name("seconds")
                .help(
                    "Maximum time to process an ordinary request and produce response headers \
                     (up to 31536000 seconds) [default: 300]",
                ),
        )
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct Args {
    #[serde(default = "default_serve_path")]
    pub serve_path: PathBuf,
    pub state_dir: Option<PathBuf>,
    #[serde(deserialize_with = "deserialize_bind_addrs")]
    #[serde(rename = "bind")]
    #[serde(default = "default_addrs")]
    pub addrs: Vec<IpAddr>,
    #[serde(default, deserialize_with = "deserialize_trusted_proxies")]
    pub trusted_proxies: Vec<TrustedProxy>,
    #[serde(default = "default_port")]
    pub port: u16,
    pub auth: AuthConfig,
    #[serde(deserialize_with = "deserialize_log_http")]
    #[serde(rename = "log-format")]
    pub http_logger: HttpLogger,
    pub log_file: Option<PathBuf>,
    pub max_upload_size: u64,
    pub upload_idle_timeout: u64,
    pub upload_total_timeout: u64,
    pub max_concurrent_uploads: usize,
    pub min_free_space: u64,
    pub max_connections: usize,
    pub max_search_entries: usize,
    pub max_concurrent_searches: usize,
    pub request_timeout: u64,
}

/// Startup settings that have passed every cross-field and filesystem check.
///
/// The inner value is private so server internals can hold this type without
/// accidentally constructing an unchecked configuration. Public CLI and
/// library callers can keep using [`Args`]; [`TryFrom`] is available to code
/// that wants an explicit validation boundary.
#[derive(Debug)]
pub struct ValidatedConfig(Args);

impl TryFrom<Args> for ValidatedConfig {
    type Error = anyhow::Error;

    fn try_from(args: Args) -> Result<Self> {
        args.validate().map(Self)
    }
}

impl Deref for ValidatedConfig {
    type Target = Args;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<Args> for ValidatedConfig {
    fn as_ref(&self) -> &Args {
        &self.0
    }
}

impl ValidatedConfig {
    pub(crate) fn state_database_path(&self) -> PathBuf {
        self.0
            .state_dir
            .as_ref()
            .expect("validated configuration requires a persistent state directory")
            .join(STATE_DATABASE_FILE_NAME)
    }
}

impl Default for Args {
    fn default() -> Self {
        Self {
            serve_path: default_serve_path(),
            state_dir: None,
            addrs: default_addrs(),
            trusted_proxies: Vec::new(),
            port: default_port(),
            auth: AuthConfig::default(),
            http_logger: HttpLogger::default(),
            log_file: None,
            max_upload_size: DEFAULT_MAX_UPLOAD_SIZE,
            upload_idle_timeout: DEFAULT_UPLOAD_IDLE_TIMEOUT,
            upload_total_timeout: DEFAULT_UPLOAD_TOTAL_TIMEOUT,
            max_concurrent_uploads: DEFAULT_MAX_CONCURRENT_UPLOADS,
            min_free_space: DEFAULT_MIN_FREE_SPACE,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_search_entries: DEFAULT_MAX_SEARCH_ENTRIES,
            max_concurrent_searches: DEFAULT_MAX_CONCURRENT_SEARCHES,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

impl Args {
    /// Merge command-line arguments with the optional configuration file and
    /// validate the resulting runtime settings.
    pub fn parse(matches: ArgMatches) -> Result<Args> {
        let mut args = Self::default();
        let config_path = matches.get_one::<PathBuf>("config").cloned();
        let mut config_identity = None;

        if let Some(config_path) = config_path.as_deref() {
            let config = read_config(config_path)?;
            config_identity = Some(PathIdentity::inspect_with_expected_object(
                config_path,
                "Configuration file",
                Some(config.object),
            )?);
            args = serde_yaml::from_str(&config.contents)
                .with_context(|| format!("Failed to load config at {}", config_path.display()))?;
        }

        if let Some(path) = matches.get_one::<PathBuf>("serve-path") {
            args.serve_path.clone_from(path)
        }

        if let Some(state_dir) = matches.get_one::<PathBuf>("state-dir") {
            args.state_dir = Some(state_dir.clone());
        }

        if let Some(port) = matches.get_one::<u16>("port") {
            args.port = *port
        }

        if let Some(addrs) = matches.get_many::<IpAddr>("bind") {
            args.addrs = addrs.copied().collect();
        }

        if let Some(trusted_proxies) = matches.get_many::<TrustedProxy>("trusted-proxy") {
            args.trusted_proxies = trusted_proxies.copied().collect();
        }

        if let Some(log_format) = matches.get_one::<String>("log-format") {
            args.http_logger = log_format.parse()?;
        }

        if let Some(log_file) = matches.get_one::<PathBuf>("log-file") {
            args.log_file = Some(log_file.clone());
        }

        if let Some(max_upload_size) = matches.get_one::<u64>("max-upload-size") {
            args.max_upload_size = *max_upload_size;
        }
        if let Some(upload_idle_timeout) = matches.get_one::<u64>("upload-idle-timeout") {
            args.upload_idle_timeout = *upload_idle_timeout;
        }
        if let Some(upload_total_timeout) = matches.get_one::<u64>("upload-total-timeout") {
            args.upload_total_timeout = *upload_total_timeout;
        }
        if let Some(max_concurrent_uploads) = matches.get_one::<usize>("max-concurrent-uploads") {
            args.max_concurrent_uploads = *max_concurrent_uploads;
        }
        if let Some(min_free_space) = matches.get_one::<u64>("min-free-space") {
            args.min_free_space = *min_free_space;
        }
        if let Some(max_connections) = matches.get_one::<usize>("max-connections") {
            args.max_connections = *max_connections;
        }
        if let Some(max_search_entries) = matches.get_one::<usize>("max-search-entries") {
            args.max_search_entries = *max_search_entries;
        }
        if let Some(max_concurrent_searches) = matches.get_one::<usize>("max-concurrent-searches") {
            args.max_concurrent_searches = *max_concurrent_searches;
        }
        if let Some(request_timeout) = matches.get_one::<u64>("request-timeout") {
            args.request_timeout = *request_timeout;
        }

        let args = args.validate()?;
        if let Some(config_identity) = config_identity.as_ref() {
            ensure_outside_shared_root(config_identity, "Configuration file", &args.serve_path)?;
            if let Some(log_file) = args.log_file.as_deref() {
                let log_identity = PathIdentity::inspect(log_file, "Log file")?;
                ensure_distinct_path_identities(
                    config_identity,
                    "Configuration file",
                    &log_identity,
                    "log file",
                )?;
            }
            if let Some(state_db) = args.state_database_path() {
                Self::ensure_no_state_db_identity_conflict(
                    &state_db,
                    config_identity,
                    "Configuration file",
                )?;
            }
        }
        Ok(args)
    }

    /// Validate and normalize settings supplied through either the CLI or the
    /// reusable library API.
    ///
    /// Keeping this boundary on `Args` prevents embedders from bypassing the
    /// invariants that the runtime relies on when it constructs semaphores and
    /// computes request deadlines.
    pub fn validate(mut self) -> Result<Self> {
        self.serve_path = Self::sanitize_path(&self.serve_path)?;
        if !self
            .serve_path
            .metadata()
            .with_context(|| {
                format!(
                    "Failed to inspect shared path `{}`",
                    self.serve_path.display()
                )
            })?
            .is_dir()
        {
            bail!(
                "Shared path `{}` must be a directory",
                self.serve_path.display()
            );
        }

        let log_identity = if let Some(log_file) = self.log_file.as_deref() {
            let identity = PathIdentity::inspect(log_file, "Log file")?;
            ensure_outside_shared_root(&identity, "Log file", &self.serve_path)?;
            if identity.links.is_some_and(|links| links != 1) {
                bail!(
                    "Existing log file `{}` must have exactly one hard link",
                    identity.entry_path.display()
                );
            }
            self.log_file = Some(identity.entry_path.clone());
            Some(identity)
        } else {
            None
        };

        if let Some(state_dir) = self.state_dir.as_deref() {
            self.state_dir = Some(Self::sanitize_state_dir_path(state_dir, &self.serve_path)?);
        }
        if let (Some(state_db), Some(log_identity)) =
            (self.state_database_path(), log_identity.as_ref())
        {
            Self::ensure_no_state_db_identity_conflict(&state_db, log_identity, "Log file")?;
        }

        let timeout_origin = Instant::now();
        validate_timeout(
            "upload-idle-timeout",
            self.upload_idle_timeout,
            timeout_origin,
        )?;
        validate_timeout(
            "upload-total-timeout",
            self.upload_total_timeout,
            timeout_origin,
        )?;
        if self.upload_total_timeout < self.upload_idle_timeout {
            bail!("upload-total-timeout must be greater than or equal to upload-idle-timeout");
        }
        if self.max_concurrent_uploads == 0 {
            bail!("max-concurrent-uploads must be greater than 0");
        }
        for (name, value) in [
            ("max-connections", self.max_connections),
            ("max-search-entries", self.max_search_entries),
            ("max-concurrent-searches", self.max_concurrent_searches),
        ] {
            if value == 0 {
                bail!("{name} must be greater than 0");
            }
        }
        if self.addrs.is_empty() {
            bail!("bind must contain at least one IP address");
        }
        if self.addrs.len() > MAX_BIND_ADDRESSES {
            bail!("bind must not contain more than {MAX_BIND_ADDRESSES} IP addresses");
        }
        let mut sorted_addrs = self.addrs.clone();
        sorted_addrs.sort_unstable();
        if sorted_addrs.windows(2).any(|pair| pair[0] == pair[1]) {
            bail!("bind must not contain duplicate IP addresses");
        }
        if self.trusted_proxies.len() > MAX_TRUSTED_PROXIES {
            bail!("trusted-proxies must not contain more than {MAX_TRUSTED_PROXIES} networks");
        }
        if trusted_proxy_union_covers_entire_family(&self.trusted_proxies, true)
            || trusted_proxy_union_covers_entire_family(&self.trusted_proxies, false)
        {
            bail!("trusted-proxies must not trust the entire IPv4 or IPv6 address space");
        }
        self.trusted_proxies.sort_unstable();
        self.trusted_proxies.dedup();
        if self.max_search_entries > MAX_SEARCH_ENTRIES {
            bail!("max-search-entries must not exceed {MAX_SEARCH_ENTRIES}");
        }
        for (name, value) in [
            ("max-connections", self.max_connections),
            ("max-concurrent-uploads", self.max_concurrent_uploads),
            ("max-concurrent-searches", self.max_concurrent_searches),
        ] {
            if value > tokio::sync::Semaphore::MAX_PERMITS {
                bail!(
                    "{name} must not exceed {}",
                    tokio::sync::Semaphore::MAX_PERMITS
                );
            }
        }
        validate_timeout("request-timeout", self.request_timeout, timeout_origin)?;

        if !self.auth.has_users() {
            bail!(
                "At least one account is required; generate a hash with `dufs hash-password`, \
                 add it under `auth:` in a protected YAML file, then start with `--config <file>`"
            );
        }

        if self.state_dir.is_none() {
            bail!(
                "A persistent SQLite state directory is required; use `--state-dir <dir>` or set `state-dir` in the configuration file"
            );
        }

        Ok(self)
    }

    fn sanitize_path<P: AsRef<Path>>(path: P) -> Result<PathBuf> {
        let path = path.as_ref();
        if !path.exists() {
            bail!("Path `{}` doesn't exist", path.display());
        }

        env::current_dir()
            .and_then(|mut p| {
                p.push(path); // If path is absolute, it replaces the current path.
                std::fs::canonicalize(p)
            })
            .with_context(|| format!("Failed to access path `{}`", path.display()))
    }

    fn sanitize_state_dir_path(path: &Path, serve_path: &Path) -> Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            env::current_dir()
                .with_context(|| "Failed to determine the current directory")?
                .join(path)
        };
        // Remove trailing separators and `.` components before lstat. On
        // Linux, a trailing slash otherwise asks the kernel to follow a final
        // symlink to a directory even when using symlink_metadata.
        let absolute = absolute.components().collect::<PathBuf>();
        let metadata = std::fs::symlink_metadata(&absolute)
            .with_context(|| format!("Failed to inspect state directory `{}`", path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "State directory `{}` must not be a symbolic link",
                path.display()
            );
        }
        if !metadata.file_type().is_dir() {
            bail!("State directory `{}` must be a directory", path.display());
        }

        let state_dir = std::fs::canonicalize(&absolute)
            .with_context(|| format!("Failed to access state directory `{}`", path.display()))?;
        let metadata = state_dir.metadata().with_context(|| {
            format!(
                "Failed to inspect state directory `{}`",
                state_dir.display()
            )
        })?;
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            bail!(
                "State directory `{}` must be owned by the effective service user",
                state_dir.display()
            );
        }
        if metadata.permissions().mode() & 0o7777 != 0o700 {
            bail!(
                "State directory `{}` must have permissions 0700",
                state_dir.display()
            );
        }
        Self::validate_state_dir_ancestor_chain(&state_dir)?;
        if state_dir.starts_with(serve_path) || serve_path.starts_with(&state_dir) {
            bail!(
                "State directory `{}` must not overlap shared path `{}`",
                state_dir.display(),
                serve_path.display()
            );
        }

        // Inspect the fixed target now so a pre-existing symlink, directory,
        // or other unsupported file type fails before the server starts.
        Self::sanitize_state_db_path(&state_dir.join(STATE_DATABASE_FILE_NAME), serve_path)?;
        Ok(state_dir)
    }

    fn validate_state_dir_ancestor_chain(state_dir: &Path) -> Result<()> {
        let service_uid = rustix::process::geteuid().as_raw();
        let trusted_owner = |uid: u32| uid == 0 || uid == service_uid;
        let mut child = state_dir.to_path_buf();
        let mut child_metadata = child.metadata().with_context(|| {
            format!(
                "Failed to inspect state directory `{}` while validating its ancestor chain",
                child.display()
            )
        })?;

        while let Some(parent) = child.parent() {
            let parent_metadata = parent.metadata().with_context(|| {
                format!(
                    "Failed to inspect state directory ancestor `{}`",
                    parent.display()
                )
            })?;
            ensure!(
                parent_metadata.is_dir(),
                "State directory ancestor `{}` must be a directory",
                parent.display()
            );
            ensure!(
                trusted_owner(parent_metadata.uid()),
                "State directory ancestor `{}` must be owned by root or the effective service user",
                parent.display()
            );

            let parent_mode = parent_metadata.permissions().mode() & 0o7777;
            if parent_mode & 0o022 != 0 {
                ensure!(
                    parent_mode & 0o1000 != 0 && trusted_owner(child_metadata.uid()),
                    "State directory ancestor `{}` can be renamed by untrusted local users; group/other-writable ancestors require sticky-bit protection for a root- or service-owned child",
                    parent.display()
                );
            }

            child = parent.to_path_buf();
            child_metadata = parent_metadata;
        }
        Ok(())
    }

    pub(crate) fn state_database_path(&self) -> Option<PathBuf> {
        self.state_dir
            .as_ref()
            .map(|path| path.join(STATE_DATABASE_FILE_NAME))
    }

    fn sanitize_state_db_path(path: &Path, serve_path: &Path) -> Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            env::current_dir()
                .with_context(|| "Failed to determine the current directory")?
                .join(path)
        };
        let file_name = absolute.file_name().ok_or_else(|| {
            anyhow::anyhow!("State database path `{}` must name a file", path.display())
        })?;
        let parent = absolute.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "State database path `{}` must have a parent directory",
                path.display()
            )
        })?;
        let parent = std::fs::canonicalize(parent).with_context(|| {
            format!(
                "Failed to access state database parent directory `{}`",
                parent.display()
            )
        })?;
        if !parent
            .metadata()
            .with_context(|| {
                format!(
                    "Failed to inspect state database parent directory `{}`",
                    parent.display()
                )
            })?
            .is_dir()
        {
            bail!(
                "State database parent `{}` must be a directory",
                parent.display()
            );
        }

        let state_db = parent.join(file_name);
        if state_db.starts_with(serve_path) {
            bail!(
                "State database `{}` must not be inside shared path `{}`",
                state_db.display(),
                serve_path.display()
            );
        }

        match std::fs::symlink_metadata(&state_db) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "State database `{}` must not be a symbolic link",
                    state_db.display()
                );
            }
            Ok(metadata) if !metadata.file_type().is_file() => {
                bail!(
                    "State database `{}` must be a regular file",
                    state_db.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to inspect state database `{}`", state_db.display())
                });
            }
        }

        Ok(state_db)
    }

    fn ensure_no_state_db_identity_conflict(
        state_db: &Path,
        other: &PathIdentity,
        other_name: &str,
    ) -> Result<()> {
        let conflicts = std::iter::once(state_db.to_path_buf()).chain(
            ["-journal", "-wal", "-shm"].into_iter().map(|suffix| {
                let mut sidecar = state_db.as_os_str().to_os_string();
                sidecar.push(suffix);
                PathBuf::from(sidecar)
            }),
        );
        for conflict in conflicts {
            let identity =
                PathIdentity::inspect(&conflict, "SQLite state database or sidecar file")?;
            if other.shares_entry_or_object_with(&identity) {
                bail!(
                    "{other_name} `{}` conflicts with SQLite state database `{}` or one of its sidecar files by entry or object identity",
                    other.entry_path.display(),
                    state_db.display()
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ConfigFileSnapshot {
    device: u64,
    inode: u64,
    mode: u32,
    links: u64,
    uid: u32,
    gid: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl ConfigFileSnapshot {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            links: metadata.nlink(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

struct ReadConfig {
    contents: String,
    object: ObjectIdentity,
}

impl fmt::Debug for ReadConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReadConfig")
            .field("contents", &"<redacted>")
            .field("object", &self.object)
            .finish()
    }
}

fn read_config(path: &Path) -> Result<ReadConfig> {
    let mut file = OpenOptions::new()
        .read(true)
        // Opening a FIFO or device supplied in place of the config must not
        // block startup before the descriptor type can be verified.
        .custom_flags((rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::NOFOLLOW).bits() as i32)
        .open(path)
        .with_context(|| format!("Failed to read config at {}", path.display()))?;
    let expected_uid = geteuid().as_raw();
    let expected_gid = getegid().as_raw();
    read_open_config(&mut file, path, |file| {
        inspect_open_config(file, path, expected_uid, expected_gid)
    })
}

fn read_open_config(
    file: &mut File,
    path: &Path,
    mut inspect: impl FnMut(&File) -> Result<ConfigFileSnapshot>,
) -> Result<ReadConfig> {
    let before = inspect(file)?;

    let mut bytes = Vec::with_capacity(before.size.min(MAX_CONFIG_BYTES) as usize);
    file.by_ref()
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Failed to read config at {}", path.display()))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        bail!(
            "Config at {} exceeds the {MAX_CONFIG_BYTES}-byte limit",
            path.display()
        );
    }
    let after = inspect(file)?;
    ensure_config_snapshot_stable(before, after, path, "while it was being read")?;

    let contents = String::from_utf8(bytes)
        .with_context(|| format!("Config at {} is not valid UTF-8", path.display()))?;
    Ok(ReadConfig {
        contents,
        object: ObjectIdentity {
            device: after.device,
            inode: after.inode,
        },
    })
}

fn inspect_open_config(
    file: &File,
    path: &Path,
    expected_uid: u32,
    expected_gid: u32,
) -> Result<ConfigFileSnapshot> {
    inspect_open_config_with(
        path,
        expected_uid,
        expected_gid,
        || snapshot_open_config(file, path),
        || config_has_extended_acl(file, path),
    )
}

fn inspect_open_config_with(
    path: &Path,
    expected_uid: u32,
    expected_gid: u32,
    mut snapshot: impl FnMut() -> Result<ConfigFileSnapshot>,
    inspect_acl: impl FnOnce() -> Result<bool>,
) -> Result<ConfigFileSnapshot> {
    let before_acl = snapshot()?;
    let has_extended_acl = inspect_acl()?;
    validate_config_security(
        before_acl,
        has_extended_acl,
        expected_uid,
        expected_gid,
        path,
    )?;
    let after_acl = snapshot()?;
    ensure_config_snapshot_stable(
        before_acl,
        after_acl,
        path,
        "while its security properties were being verified",
    )?;
    Ok(after_acl)
}

fn snapshot_open_config(file: &File, path: &Path) -> Result<ConfigFileSnapshot> {
    // `File::metadata` performs fstat on this already-open descriptor; it
    // never resolves the configured path again.
    file.metadata()
        .map(|metadata| ConfigFileSnapshot::from_metadata(&metadata))
        .with_context(|| format!("Failed to inspect config at {}", path.display()))
}

fn config_has_extended_acl(file: &File, path: &Path) -> Result<bool> {
    let mut empty_value = [0_u8; 0];
    match fgetxattr(file, CONFIG_POSIX_ACL_XATTR, &mut empty_value) {
        Ok(_) => Ok(true),
        Err(error) if error == Errno::NODATA || error == Errno::NOTSUP => Ok(false),
        Err(error) => Err(std::io::Error::from(error)).with_context(|| {
            format!(
                "Failed to inspect the POSIX access ACL on config at {}",
                path.display()
            )
        }),
    }
}

fn validate_config_security(
    snapshot: ConfigFileSnapshot,
    has_extended_acl: bool,
    expected_uid: u32,
    expected_gid: u32,
    path: &Path,
) -> Result<()> {
    if snapshot.mode & CONFIG_FILE_TYPE_MASK != CONFIG_REGULAR_FILE_TYPE {
        bail!("Config at {} must be a regular file", path.display());
    }
    if snapshot.links != 1 {
        bail!(
            "Config at {} must have exactly one hard link",
            path.display()
        );
    }
    if snapshot.uid != 0 && snapshot.uid != expected_uid {
        bail!(
            "Config at {} must be owned by root (uid 0) or the effective service user (uid {expected_uid})",
            path.display(),
        );
    }

    let permissions = snapshot.mode & CONFIG_PERMISSION_MASK;
    if !matches!(permissions, 0o400 | 0o440 | 0o600 | 0o640) {
        bail!(
            "Config at {} must have permissions 0400, 0440, 0600, or 0640",
            path.display()
        );
    }
    if permissions & 0o040 != 0 && snapshot.gid != expected_gid {
        bail!(
            "Config at {} with group-read permissions must belong to the effective service group (gid {expected_gid})",
            path.display(),
        );
    }
    if has_extended_acl {
        bail!(
            "Config at {} must not have an extended POSIX access ACL",
            path.display()
        );
    }
    if snapshot.size > MAX_CONFIG_BYTES {
        bail!(
            "Config at {} exceeds the {MAX_CONFIG_BYTES}-byte limit",
            path.display()
        );
    }
    Ok(())
}

fn ensure_config_snapshot_stable(
    before: ConfigFileSnapshot,
    after: ConfigFileSnapshot,
    path: &Path,
    operation: &str,
) -> Result<()> {
    if before != after {
        bail!("Config at {} changed {operation}", path.display());
    }
    Ok(())
}

fn validate_timeout(name: &str, seconds: u64, origin: Instant) -> Result<()> {
    if seconds == 0 {
        bail!("{name} must be greater than 0");
    }
    if seconds > MAX_TIMEOUT_SECONDS {
        bail!("{name} must not exceed {MAX_TIMEOUT_SECONDS} seconds");
    }
    if origin.checked_add(Duration::from_secs(seconds)).is_none() {
        bail!("{name} cannot be represented by the platform monotonic clock");
    }
    Ok(())
}

fn parse_bind_addrs(addrs: &[&str]) -> Result<Vec<IpAddr>> {
    let mut bind_addrs = Vec::with_capacity(addrs.len());
    for addr in addrs {
        let Ok(ip) = addr.parse::<IpAddr>() else {
            bail!("Invalid bind address `{addr}`: expected an IP address");
        };
        bind_addrs.push(ip);
    }
    Ok(bind_addrs)
}

fn parse_trusted_proxy(value: &str) -> Result<TrustedProxy> {
    value.parse()
}

fn parse_trusted_proxies(values: &[&str]) -> Result<Vec<TrustedProxy>> {
    values
        .iter()
        .map(|value| parse_trusted_proxy(value))
        .collect()
}

fn trusted_proxy_union_covers_entire_family(networks: &[TrustedProxy], ipv4: bool) -> bool {
    let mut ranges = networks
        .iter()
        .filter_map(
            |network| match (network.0.network(), network.0.broadcast()) {
                (IpAddr::V4(start), IpAddr::V4(end)) if ipv4 => {
                    Some((u32::from(start) as u128, u32::from(end) as u128))
                }
                (IpAddr::V6(start), IpAddr::V6(end)) if !ipv4 => {
                    Some((u128::from(start), u128::from(end)))
                }
                _ => None,
            },
        )
        .collect::<Vec<_>>();
    ranges.sort_unstable();

    let maximum = if ipv4 { u32::MAX as u128 } else { u128::MAX };
    let Some(&(first_start, mut covered_to)) = ranges.first() else {
        return false;
    };
    if first_start != 0 {
        return false;
    }
    for &(start, end) in &ranges[1..] {
        if start > covered_to.saturating_add(1) {
            return false;
        }
        covered_to = covered_to.max(end);
    }
    covered_to == maximum
}

fn deserialize_bind_addrs<'de, D>(deserializer: D) -> Result<Vec<IpAddr>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringOrVec;

    impl<'de> serde::de::Visitor<'de> for StringOrVec {
        type Value = Vec<IpAddr>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("IP address string or list of IP address strings")
        }

        fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            parse_bind_addrs(&[s]).map_err(serde::de::Error::custom)
        }

        fn visit_seq<S>(self, seq: S) -> Result<Self::Value, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            let addrs: Vec<&'de str> =
                Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))?;
            parse_bind_addrs(&addrs).map_err(serde::de::Error::custom)
        }
    }

    deserializer.deserialize_any(StringOrVec)
}

fn deserialize_trusted_proxies<'de, D>(deserializer: D) -> Result<Vec<TrustedProxy>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringOrVec;

    impl<'de> serde::de::Visitor<'de> for StringOrVec {
        type Value = Vec<TrustedProxy>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("trusted proxy IP/CIDR string or list of strings")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            parse_trusted_proxies(&[value]).map_err(serde::de::Error::custom)
        }

        fn visit_seq<S>(self, seq: S) -> Result<Self::Value, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            let values: Vec<&'de str> =
                Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))?;
            parse_trusted_proxies(&values).map_err(serde::de::Error::custom)
        }
    }

    deserializer.deserialize_any(StringOrVec)
}

fn deserialize_log_http<'de, D>(deserializer: D) -> Result<HttpLogger, D::Error>
where
    D: Deserializer<'de>,
{
    let value: String = Deserialize::deserialize(deserializer)?;
    value.parse().map_err(serde::de::Error::custom)
}

fn default_serve_path() -> PathBuf {
    PathBuf::from(".")
}

fn default_addrs() -> Vec<IpAddr> {
    vec![IpAddr::from([127, 0, 0, 1])]
}

fn default_port() -> u16 {
    5000
}

#[cfg(test)]
mod tests;
