use anyhow::{Context, Result, bail};
use async_deflate_zip::Compression;
use clap::builder::PossibleValue;
use clap::{Arg, ArgAction, ArgMatches, Command, ValueEnum, value_parser};
use serde::{Deserialize, Deserializer};
use std::env;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use crate::auth::AccessControl;
use crate::http_logger::HttpLogger;
use crate::utils::encode_uri;

const DEFAULT_MAX_UPLOAD_SIZE: u64 = 100 * 1024 * 1024 * 1024;
const DEFAULT_UPLOAD_IDLE_TIMEOUT: u64 = 60;
const DEFAULT_UPLOAD_TOTAL_TIMEOUT: u64 = 24 * 60 * 60;
const DEFAULT_MAX_CONCURRENT_UPLOADS: usize = 4;
const DEFAULT_MIN_FREE_SPACE: u64 = 1024 * 1024 * 1024;
const DEFAULT_MAX_CONNECTIONS: usize = 256;
const DEFAULT_MAX_SEARCH_ENTRIES: usize = 10_000;
const DEFAULT_MAX_ZIP_ENTRIES: usize = 10_000;
const DEFAULT_MAX_ZIP_UNCOMPRESSED_SIZE: u64 = 10 * 1024 * 1024 * 1024;
const DEFAULT_MAX_ZIP_OUTPUT_SIZE: u64 = 10 * 1024 * 1024 * 1024;
const DEFAULT_MAX_CONCURRENT_SEARCHES: usize = 2;
const DEFAULT_MAX_CONCURRENT_ZIPS: usize = 1;
const DEFAULT_REQUEST_TIMEOUT: u64 = 300;

pub fn build_cli() -> Command {
    Command::new(env!("CARGO_CRATE_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
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
            Arg::new("bind")
                .short('b')
                .long("bind")
                .value_parser(value_parser!(IpAddr))
                .help("Specify IP address to bind [default: 0.0.0.0]")
                .action(ArgAction::Append)
                .value_delimiter(',')
                .value_name("addrs"),
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
            Arg::new("path-prefix")
                .long("path-prefix")
                .value_name("path")
                .help("Specify a path prefix"),
        )
        .arg(
            Arg::new("hidden")
                .long("hidden")
                .action(ArgAction::Append)
                .value_delimiter(',')
                .help("Hide paths from directory listings, e.g. tmp,*.log,*.lock")
                .value_name("value"),
        )
        .arg(
            Arg::new("auth")
                .short('a')
                .long("auth")
                .help("Add a required full-access account as user:<argon2id PHC>")
                .action(ArgAction::Append)
                .value_name("account"),
        )
        .arg(
            Arg::new("log-format")
                .long("log-format")
                .value_name("format")
                .help("Customize http log format"),
        )
        .arg(
            Arg::new("log-file")
                .long("log-file")
                .value_name("file")
                .value_parser(value_parser!(PathBuf))
                .help("Specify the file to save logs to, other than stdout/stderr"),
        )
        .arg(
            Arg::new("compress")
                .value_parser(clap::builder::EnumValueParser::<Compress>::new())
                .long("compress")
                .value_name("level")
                .help("Set zip compress level [default: low]"),
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
                .help("Maximum idle time while receiving an upload [default: 60]"),
        )
        .arg(
            Arg::new("upload-total-timeout")
                .long("upload-total-timeout")
                .value_parser(value_parser!(u64))
                .value_name("seconds")
                .help("Maximum total time for one upload [default: 86400]"),
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
                .help("Reserved free disk space required before uploads [default: 1073741824]"),
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
                .help("Maximum entries examined by one search [default: 10000]"),
        )
        .arg(
            Arg::new("max-zip-entries")
                .long("max-zip-entries")
                .value_parser(value_parser!(usize))
                .value_name("count")
                .help("Maximum directory entries examined by one ZIP [default: 10000]"),
        )
        .arg(
            Arg::new("max-zip-uncompressed-size")
                .long("max-zip-uncompressed-size")
                .value_parser(value_parser!(u64))
                .value_name("bytes")
                .help("Maximum uncompressed bytes included in one ZIP [default: 10737418240]"),
        )
        .arg(
            Arg::new("max-zip-output-size")
                .long("max-zip-output-size")
                .value_parser(value_parser!(u64))
                .value_name("bytes")
                .help("Maximum bytes written to one ZIP tempfile [default: 10737418240]"),
        )
        .arg(
            Arg::new("max-concurrent-searches")
                .long("max-concurrent-searches")
                .value_parser(value_parser!(usize))
                .value_name("count")
                .help("Maximum number of concurrent searches [default: 2]"),
        )
        .arg(
            Arg::new("max-concurrent-zips")
                .long("max-concurrent-zips")
                .value_parser(value_parser!(usize))
                .value_name("count")
                .help("Maximum number of concurrent ZIP operations [default: 1]"),
        )
        .arg(
            Arg::new("request-timeout")
                .long("request-timeout")
                .value_parser(value_parser!(u64))
                .value_name("seconds")
                .help(
                    "Maximum time to process an ordinary request and produce response headers \
                     [default: 300]",
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
    #[serde(deserialize_with = "deserialize_bind_addrs")]
    #[serde(rename = "bind")]
    #[serde(default = "default_addrs")]
    pub addrs: Vec<IpAddr>,
    #[serde(default = "default_port")]
    pub port: u16,
    pub path_prefix: String,
    #[serde(skip)]
    pub uri_prefix: String,
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    pub hidden: Vec<String>,
    #[serde(deserialize_with = "deserialize_access_control")]
    pub auth: AccessControl,
    #[serde(deserialize_with = "deserialize_log_http")]
    #[serde(rename = "log-format")]
    pub http_logger: HttpLogger,
    pub log_file: Option<PathBuf>,
    pub compress: Compress,
    pub max_upload_size: u64,
    pub upload_idle_timeout: u64,
    pub upload_total_timeout: u64,
    pub max_concurrent_uploads: usize,
    pub min_free_space: u64,
    pub max_connections: usize,
    pub max_search_entries: usize,
    pub max_zip_entries: usize,
    pub max_zip_uncompressed_size: u64,
    pub max_zip_output_size: u64,
    pub max_concurrent_searches: usize,
    pub max_concurrent_zips: usize,
    pub request_timeout: u64,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            serve_path: default_serve_path(),
            addrs: default_addrs(),
            port: default_port(),
            path_prefix: String::new(),
            uri_prefix: String::new(),
            hidden: Vec::new(),
            auth: AccessControl::default(),
            http_logger: HttpLogger::default(),
            log_file: None,
            compress: Compress::default(),
            max_upload_size: DEFAULT_MAX_UPLOAD_SIZE,
            upload_idle_timeout: DEFAULT_UPLOAD_IDLE_TIMEOUT,
            upload_total_timeout: DEFAULT_UPLOAD_TOTAL_TIMEOUT,
            max_concurrent_uploads: DEFAULT_MAX_CONCURRENT_UPLOADS,
            min_free_space: DEFAULT_MIN_FREE_SPACE,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_search_entries: DEFAULT_MAX_SEARCH_ENTRIES,
            max_zip_entries: DEFAULT_MAX_ZIP_ENTRIES,
            max_zip_uncompressed_size: DEFAULT_MAX_ZIP_UNCOMPRESSED_SIZE,
            max_zip_output_size: DEFAULT_MAX_ZIP_OUTPUT_SIZE,
            max_concurrent_searches: DEFAULT_MAX_CONCURRENT_SEARCHES,
            max_concurrent_zips: DEFAULT_MAX_CONCURRENT_ZIPS,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

impl Args {
    /// Parse command-line arguments.
    ///
    /// If a parsing error occurred, exit the process and print out informative
    /// error message to user.
    pub fn parse(matches: ArgMatches) -> Result<Args> {
        let mut args = Self::default();

        if let Some(config_path) = matches.get_one::<PathBuf>("config") {
            let contents = std::fs::read_to_string(config_path)
                .with_context(|| format!("Failed to read config at {}", config_path.display()))?;
            args = serde_yaml::from_str(&contents)
                .with_context(|| format!("Failed to load config at {}", config_path.display()))?;
        }

        if let Some(path) = matches.get_one::<PathBuf>("serve-path") {
            args.serve_path.clone_from(path)
        }

        args.serve_path = Self::sanitize_path(args.serve_path)?;
        if !args
            .serve_path
            .metadata()
            .with_context(|| {
                format!(
                    "Failed to inspect shared path `{}`",
                    args.serve_path.display()
                )
            })?
            .is_dir()
        {
            bail!(
                "Shared path `{}` must be a directory",
                args.serve_path.display()
            );
        }

        if let Some(port) = matches.get_one::<u16>("port") {
            args.port = *port
        }

        if let Some(addrs) = matches.get_many::<IpAddr>("bind") {
            args.addrs = addrs.copied().collect();
        }

        if let Some(path_prefix) = matches.get_one::<String>("path-prefix") {
            args.path_prefix.clone_from(path_prefix)
        }
        args.path_prefix = args.path_prefix.trim_matches('/').to_string();

        args.uri_prefix = if args.path_prefix.is_empty() {
            "/".to_owned()
        } else {
            format!("/{}/", encode_uri(&args.path_prefix))
        };

        if let Some(hidden) = matches.get_many::<String>("hidden") {
            args.hidden = hidden.cloned().collect();
        } else {
            let mut hidden = vec![];
            std::mem::swap(&mut args.hidden, &mut hidden);
            args.hidden = hidden
                .into_iter()
                .flat_map(|v| v.split(',').map(|v| v.to_string()).collect::<Vec<String>>())
                .collect();
        }

        if let Some(rules) = matches.get_many::<String>("auth") {
            let rules: Vec<_> = rules.map(|v| v.as_str()).collect();
            args.auth = AccessControl::new(&rules)?;
        }

        if let Some(log_format) = matches.get_one::<String>("log-format") {
            args.http_logger = log_format.parse()?;
        }

        if let Some(log_file) = matches.get_one::<PathBuf>("log-file") {
            args.log_file = Some(log_file.clone());
        }

        if let Some(compress) = matches.get_one::<Compress>("compress") {
            args.compress = *compress;
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
        if let Some(max_zip_entries) = matches.get_one::<usize>("max-zip-entries") {
            args.max_zip_entries = *max_zip_entries;
        }
        if let Some(max_zip_uncompressed_size) = matches.get_one::<u64>("max-zip-uncompressed-size")
        {
            args.max_zip_uncompressed_size = *max_zip_uncompressed_size;
        }
        if let Some(max_zip_output_size) = matches.get_one::<u64>("max-zip-output-size") {
            args.max_zip_output_size = *max_zip_output_size;
        }
        if let Some(max_concurrent_searches) = matches.get_one::<usize>("max-concurrent-searches") {
            args.max_concurrent_searches = *max_concurrent_searches;
        }
        if let Some(max_concurrent_zips) = matches.get_one::<usize>("max-concurrent-zips") {
            args.max_concurrent_zips = *max_concurrent_zips;
        }
        if let Some(request_timeout) = matches.get_one::<u64>("request-timeout") {
            args.request_timeout = *request_timeout;
        }

        if args.upload_idle_timeout == 0 {
            bail!("upload-idle-timeout must be greater than 0");
        }
        if args.upload_total_timeout == 0 {
            bail!("upload-total-timeout must be greater than 0");
        }
        if args.upload_total_timeout < args.upload_idle_timeout {
            bail!("upload-total-timeout must be greater than or equal to upload-idle-timeout");
        }
        if args.max_concurrent_uploads == 0 {
            bail!("max-concurrent-uploads must be greater than 0");
        }
        for (name, value) in [
            ("max-connections", args.max_connections),
            ("max-search-entries", args.max_search_entries),
            ("max-zip-entries", args.max_zip_entries),
            ("max-concurrent-searches", args.max_concurrent_searches),
            ("max-concurrent-zips", args.max_concurrent_zips),
        ] {
            if value == 0 {
                bail!("{name} must be greater than 0");
            }
        }
        for (name, value) in [
            ("max-connections", args.max_connections),
            ("max-concurrent-uploads", args.max_concurrent_uploads),
            ("max-concurrent-searches", args.max_concurrent_searches),
            ("max-concurrent-zips", args.max_concurrent_zips),
        ] {
            if value > tokio::sync::Semaphore::MAX_PERMITS {
                bail!(
                    "{name} must not exceed {}",
                    tokio::sync::Semaphore::MAX_PERMITS
                );
            }
        }
        if args.max_zip_uncompressed_size == 0 {
            bail!("max-zip-uncompressed-size must be greater than 0");
        }
        if args.max_zip_output_size == 0 {
            bail!("max-zip-output-size must be greater than 0");
        }
        if args.request_timeout == 0 {
            bail!("request-timeout must be greater than 0");
        }

        if !args.auth.has_users() {
            bail!(
                "At least one account is required; generate a hash with `dufs hash-password`, \
                 then use `--auth 'user:<argon2id PHC>'`"
            );
        }

        Ok(args)
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
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Compress {
    None,
    #[default]
    Low,
    Medium,
    High,
}

impl ValueEnum for Compress {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::None, Self::Low, Self::Medium, Self::High]
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(match self {
            Compress::None => PossibleValue::new("none"),
            Compress::Low => PossibleValue::new("low"),
            Compress::Medium => PossibleValue::new("medium"),
            Compress::High => PossibleValue::new("high"),
        })
    }
}

impl Compress {
    pub fn to_compression(self) -> Compression {
        match self {
            Compress::None => Compression::none(),
            Compress::Low => Compression::fast(),
            Compress::Medium => Compression::default(),
            Compress::High => Compression::best(),
        }
    }
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

fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringOrVec;

    impl<'de> serde::de::Visitor<'de> for StringOrVec {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("string or list of strings")
        }

        fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(vec![s.to_owned()])
        }

        fn visit_seq<S>(self, seq: S) -> Result<Self::Value, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))
        }
    }

    deserializer.deserialize_any(StringOrVec)
}

fn deserialize_access_control<'de, D>(deserializer: D) -> Result<AccessControl, D::Error>
where
    D: Deserializer<'de>,
{
    let rules: Vec<&str> = Vec::deserialize(deserializer)?;
    AccessControl::new(&rules).map_err(serde::de::Error::custom)
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
    vec![IpAddr::from([0, 0, 0, 0])]
}

fn default_port() -> u16 {
    5000
}

#[cfg(test)]
mod tests {
    use super::*;

    use assert_fs::prelude::*;

    const TEST_ACCOUNT: &str = "user:$argon2id$v=19$m=19456,t=2,p=1$HdPI2G8k0h+yEgnqIt2rSw$P+MRyz7wH+b/iPY+He/9DApcy6yB9TAoo7j2JG1Smzs";

    #[test]
    fn test_default() {
        let cli = build_cli();
        let matches = cli
            .try_get_matches_from(vec!["", "--auth", TEST_ACCOUNT])
            .unwrap();
        let args = Args::parse(matches).unwrap();
        let cwd = Args::sanitize_path(std::env::current_dir().unwrap()).unwrap();
        assert_eq!(args.serve_path, cwd);
        assert_eq!(args.port, default_port());
        assert_eq!(args.addrs, vec![IpAddr::from([0, 0, 0, 0])]);
        assert_eq!(args.max_upload_size, DEFAULT_MAX_UPLOAD_SIZE);
        assert_eq!(args.upload_idle_timeout, DEFAULT_UPLOAD_IDLE_TIMEOUT);
        assert_eq!(args.upload_total_timeout, DEFAULT_UPLOAD_TOTAL_TIMEOUT);
        assert_eq!(args.max_concurrent_uploads, DEFAULT_MAX_CONCURRENT_UPLOADS);
        assert_eq!(args.min_free_space, DEFAULT_MIN_FREE_SPACE);
        assert_eq!(args.max_connections, DEFAULT_MAX_CONNECTIONS);
        assert_eq!(args.max_search_entries, DEFAULT_MAX_SEARCH_ENTRIES);
        assert_eq!(args.max_zip_entries, DEFAULT_MAX_ZIP_ENTRIES);
        assert_eq!(
            args.max_zip_uncompressed_size,
            DEFAULT_MAX_ZIP_UNCOMPRESSED_SIZE
        );
        assert_eq!(args.max_zip_output_size, DEFAULT_MAX_ZIP_OUTPUT_SIZE);
        assert_eq!(
            args.max_concurrent_searches,
            DEFAULT_MAX_CONCURRENT_SEARCHES
        );
        assert_eq!(args.max_concurrent_zips, DEFAULT_MAX_CONCURRENT_ZIPS);
        assert_eq!(args.request_timeout, DEFAULT_REQUEST_TIMEOUT);
    }

    #[test]
    fn test_args_from_cli1() {
        let tmpdir = assert_fs::TempDir::new().unwrap();
        let cli = build_cli();
        let matches = cli
            .try_get_matches_from(vec![
                "",
                "--hidden",
                "tmp,*.log,*.lock",
                "--auth",
                TEST_ACCOUNT,
                &tmpdir.to_string_lossy(),
            ])
            .unwrap();
        let args = Args::parse(matches).unwrap();
        assert_eq!(args.serve_path, Args::sanitize_path(&tmpdir).unwrap());
        assert_eq!(args.hidden, ["tmp", "*.log", "*.lock"]);
    }

    #[test]
    fn test_args_from_cli2() {
        let cli = build_cli();
        let matches = cli
            .try_get_matches_from(vec![
                "",
                "--hidden",
                "tmp",
                "--hidden",
                "*.log",
                "--hidden",
                "*.lock",
                "--auth",
                TEST_ACCOUNT,
            ])
            .unwrap();
        let args = Args::parse(matches).unwrap();
        assert_eq!(args.hidden, ["tmp", "*.log", "*.lock"]);
    }

    #[test]
    fn test_args_from_empty_config_file_requires_auth() {
        let tmpdir = assert_fs::TempDir::new().unwrap();
        let config_file = tmpdir.child("config.yaml");
        config_file.write_str("").unwrap();

        let cli = build_cli();
        let matches = cli
            .try_get_matches_from(vec!["", "-c", &config_file.to_string_lossy()])
            .unwrap();
        let err = Args::parse(matches).unwrap_err();
        assert!(err.to_string().contains("At least one account is required"));
    }

    #[test]
    fn test_args_from_config_file1() {
        let tmpdir = assert_fs::TempDir::new().unwrap();
        let config_file = tmpdir.child("config.yaml");
        let contents = format!(
            r#"
serve-path: {}
bind: 0.0.0.0
port: 3000
hidden: tmp,*.log,*.lock
auth:
  - {TEST_ACCOUNT}
"#,
            tmpdir.display()
        );
        config_file.write_str(&contents).unwrap();

        let cli = build_cli();
        let matches = cli
            .try_get_matches_from(vec!["", "-c", &config_file.to_string_lossy()])
            .unwrap();
        let args = Args::parse(matches).unwrap();
        assert_eq!(args.serve_path, Args::sanitize_path(&tmpdir).unwrap());
        assert_eq!(args.addrs, vec!["0.0.0.0".parse::<IpAddr>().unwrap()]);
        assert_eq!(args.hidden, ["tmp", "*.log", "*.lock"]);
        assert_eq!(args.port, 3000);
    }

    #[test]
    fn test_file_serve_path_from_config_is_rejected() {
        let tmpdir = assert_fs::TempDir::new().unwrap();
        let shared_file = tmpdir.child("shared.txt");
        shared_file.write_str("contents").unwrap();
        let config_file = tmpdir.child("config.yaml");
        let contents = format!(
            r#"
serve-path: {}
auth:
  - {TEST_ACCOUNT}
"#,
            shared_file.display()
        );
        config_file.write_str(&contents).unwrap();

        let cli = build_cli();
        let matches = cli
            .try_get_matches_from(vec!["", "-c", &config_file.to_string_lossy()])
            .unwrap();
        let err = Args::parse(matches).unwrap_err();
        assert!(
            err.to_string().contains("must be a directory"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn test_args_from_config_file2() {
        let tmpdir = assert_fs::TempDir::new().unwrap();
        let config_file = tmpdir.child("config.yaml");
        let contents = format!(
            r#"
bind:
  - 127.0.0.1
  - 192.168.8.10
hidden:
  - tmp
  - '*.log'
  - '*.lock'
auth:
  - {TEST_ACCOUNT}
"#
        );
        config_file.write_str(&contents).unwrap();

        let cli = build_cli();
        let matches = cli
            .try_get_matches_from(vec!["", "-c", &config_file.to_string_lossy()])
            .unwrap();
        let args = Args::parse(matches).unwrap();
        assert_eq!(
            args.addrs,
            vec![
                "127.0.0.1".parse::<IpAddr>().unwrap(),
                "192.168.8.10".parse::<IpAddr>().unwrap()
            ]
        );
        assert_eq!(args.hidden, ["tmp", "*.log", "*.lock"]);
    }

    #[test]
    fn test_non_ip_bind_config_is_rejected() {
        let tmpdir = assert_fs::TempDir::new().unwrap();
        let config_file = tmpdir.child("config.yaml");
        let contents = format!(
            r#"
serve-path: {}
bind: not-an-ip-address
auth:
  - {TEST_ACCOUNT}
"#,
            tmpdir.display()
        );
        config_file.write_str(&contents).unwrap();

        let cli = build_cli();
        let matches = cli
            .try_get_matches_from(vec!["", "-c", &config_file.to_string_lossy()])
            .unwrap();
        let err = Args::parse(matches).unwrap_err();
        assert!(format!("{err:#}").contains("expected an IP address"));
    }

    #[test]
    fn upload_limits_from_cli_override_defaults() {
        let cli = build_cli();
        let matches = cli
            .try_get_matches_from([
                "",
                "--auth",
                TEST_ACCOUNT,
                "--max-upload-size",
                "0",
                "--upload-idle-timeout",
                "12",
                "--upload-total-timeout",
                "34",
                "--max-concurrent-uploads",
                "2",
                "--min-free-space",
                "0",
                "--max-connections",
                "11",
                "--max-search-entries",
                "12",
                "--max-zip-entries",
                "13",
                "--max-zip-uncompressed-size",
                "14",
                "--max-zip-output-size",
                "15",
                "--max-concurrent-searches",
                "3",
                "--max-concurrent-zips",
                "2",
                "--request-timeout",
                "16",
            ])
            .unwrap();
        let args = Args::parse(matches).unwrap();
        assert_eq!(args.max_upload_size, 0);
        assert_eq!(args.upload_idle_timeout, 12);
        assert_eq!(args.upload_total_timeout, 34);
        assert_eq!(args.max_concurrent_uploads, 2);
        assert_eq!(args.min_free_space, 0);
        assert_eq!(args.max_connections, 11);
        assert_eq!(args.max_search_entries, 12);
        assert_eq!(args.max_zip_entries, 13);
        assert_eq!(args.max_zip_uncompressed_size, 14);
        assert_eq!(args.max_zip_output_size, 15);
        assert_eq!(args.max_concurrent_searches, 3);
        assert_eq!(args.max_concurrent_zips, 2);
        assert_eq!(args.request_timeout, 16);
    }

    #[test]
    fn invalid_upload_time_and_concurrency_limits_are_rejected() {
        for invalid_args in [
            ["--upload-idle-timeout", "0"],
            ["--upload-total-timeout", "0"],
            ["--max-concurrent-uploads", "0"],
            ["--upload-idle-timeout", "60"],
        ] {
            let mut values = vec!["", "--auth", TEST_ACCOUNT];
            values.extend(invalid_args);
            if invalid_args[0] == "--upload-idle-timeout" && invalid_args[1] == "60" {
                values.extend(["--upload-total-timeout", "59"]);
            }
            let matches = build_cli().try_get_matches_from(values).unwrap();
            assert!(Args::parse(matches).is_err(), "accepted {invalid_args:?}");
        }
    }

    #[test]
    fn zero_ordinary_request_budgets_are_rejected() {
        for name in [
            "--max-connections",
            "--max-search-entries",
            "--max-zip-entries",
            "--max-zip-uncompressed-size",
            "--max-zip-output-size",
            "--max-concurrent-searches",
            "--max-concurrent-zips",
            "--request-timeout",
        ] {
            let matches = build_cli()
                .try_get_matches_from(["", "--auth", TEST_ACCOUNT, name, "0"])
                .unwrap();
            assert!(Args::parse(matches).is_err(), "accepted {name}=0");
        }
    }

    #[test]
    fn semaphore_limits_above_tokios_maximum_are_rejected() {
        let too_many = (tokio::sync::Semaphore::MAX_PERMITS + 1).to_string();
        for name in [
            "--max-connections",
            "--max-concurrent-uploads",
            "--max-concurrent-searches",
            "--max-concurrent-zips",
        ] {
            let matches = build_cli()
                .try_get_matches_from(["", "--auth", TEST_ACCOUNT, name, too_many.as_str()])
                .unwrap();
            let error = Args::parse(matches).expect_err("oversized semaphore count was accepted");
            assert!(
                error.to_string().contains("must not exceed"),
                "unexpected error for {name}: {error:#}"
            );
        }
    }
}
