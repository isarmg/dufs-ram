use anyhow::{Context, Result, bail};
use chrono::{Local, SecondsFormat};
use log::{Level, LevelFilter, Metadata, Record};
use rustix::{
    fs::{FileType, Mode, OFlags, fchmod, fstat, open},
    process::geteuid,
};
use std::fs::File;
use std::io::{BufWriter, Stderr, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
};
use std::thread;
use std::time::{Duration, Instant};

// Request threads must never wait for a slow terminal, pipe or filesystem.
// When this bounded queue is full, the newest entry is dropped and the writer
// emits one aggregate warning as soon as it can make progress again.
const LOG_QUEUE_CAPACITY: usize = 4096;
const LOG_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const LOG_FLUSH_DEADLINE: Duration = Duration::from_secs(5);
const LOG_DROP_REPORT_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const MAX_LOG_ENTRY_BYTES: usize = 16 * 1024;
pub(crate) const LOG_TRUNCATION_SUFFIX: &str = "...[truncated]";

/// Incrementally build one bounded log entry without first allocating the
/// potentially much larger untruncated value.
pub(crate) struct BoundedLogLine {
    value: String,
    truncated: bool,
}

impl BoundedLogLine {
    pub(crate) fn new() -> Self {
        Self {
            value: String::new(),
            truncated: false,
        }
    }

    pub(crate) fn push_str(&mut self, value: &str) {
        if self.truncated {
            return;
        }
        if value.len() <= MAX_LOG_ENTRY_BYTES.saturating_sub(self.value.len()) {
            self.value.push_str(value);
            return;
        }

        let content_limit = MAX_LOG_ENTRY_BYTES - LOG_TRUNCATION_SUFFIX.len();
        if self.value.len() > content_limit {
            let mut end = content_limit;
            while !self.value.is_char_boundary(end) {
                end -= 1;
            }
            self.value.truncate(end);
        }

        let mut end = value.len().min(content_limit - self.value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.value.push_str(&value[..end]);
        self.value.push_str(LOG_TRUNCATION_SUFFIX);
        self.truncated = true;
    }

    pub(crate) fn finish(self) -> String {
        self.value
    }

    pub(crate) fn is_truncated(&self) -> bool {
        self.truncated
    }
}

struct AsyncLogger {
    sender: SyncSender<WriterCommand>,
    dropped: Arc<AtomicU64>,
}

enum WriterCommand {
    Entry(LogEntry),
    Flush(std::sync::mpsc::Sender<()>),
}

struct LogEntry {
    text: String,
    stderr: bool,
}

enum LogOutput {
    File(BufWriter<File>),
    Console {
        stdout: BufWriter<Stdout>,
        stderr: BufWriter<Stderr>,
    },
}

impl LogOutput {
    fn write_line(&mut self, entry: &LogEntry) -> std::io::Result<()> {
        match self {
            Self::File(file) => writeln!(file, "{}", entry.text),
            Self::Console { stdout, stderr } => {
                if entry.stderr {
                    writeln!(stderr, "{}", entry.text)
                } else {
                    writeln!(stdout, "{}", entry.text)
                }
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::File(file) => file.flush(),
            Self::Console { stdout, stderr } => {
                stdout.flush()?;
                stderr.flush()
            }
        }
    }
}

impl log::Log for AsyncLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let message = sanitize_log_line(&record.args().to_string());
        let text = if record.target() == "http_access" {
            message
        } else {
            format!("{} {} {}", timestamp(), record.level(), message)
        };
        let entry = LogEntry {
            text: truncate_log_entry(text),
            stderr: record.level() < Level::Info,
        };
        match self.sender.try_send(WriterCommand::Entry(entry)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn flush(&self) {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let deadline = Instant::now() + LOG_FLUSH_DEADLINE;
        let mut command = WriterCommand::Flush(done_tx);
        loop {
            match self.sender.try_send(command) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) if Instant::now() < deadline => {
                    command = returned;
                    thread::sleep(Duration::from_millis(1));
                }
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => return,
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            let _ = done_rx.recv_timeout(remaining);
        }
    }
}

pub fn init(log_file: Option<PathBuf>) -> Result<()> {
    let output = match log_file {
        None => LogOutput::Console {
            stdout: BufWriter::new(std::io::stdout()),
            stderr: BufWriter::new(std::io::stderr()),
        },
        Some(log_file) => LogOutput::File(BufWriter::new(open_log_file(&log_file)?)),
    };

    let (sender, receiver) = sync_channel(LOG_QUEUE_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));
    let writer_dropped = dropped.clone();
    thread::Builder::new()
        .name("dufs-log-writer".to_string())
        .spawn(move || writer_loop(receiver, output, &writer_dropped))
        .context("Failed to start the log writer")?;

    let logger = AsyncLogger { sender, dropped };
    log::set_boxed_logger(Box::new(logger))
        .map(|_| log::set_max_level(LevelFilter::Info))
        .with_context(|| "Failed to init logger")?;
    Ok(())
}

fn open_log_file(path: &Path) -> Result<File> {
    open_log_file_for_owner(path, geteuid().as_raw())
}

fn open_log_file_for_owner(path: &Path, expected_owner: u32) -> Result<File> {
    let private_mode = Mode::RUSR | Mode::WUSR;
    let fd = open(
        path,
        OFlags::WRONLY
            | OFlags::APPEND
            | OFlags::CREATE
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | OFlags::CLOEXEC,
        private_mode,
    )
    .map_err(std::io::Error::from)
    .with_context(|| format!("Failed to securely open log file '{}'", path.display()))?;

    let metadata = fstat(&fd)
        .map_err(std::io::Error::from)
        .with_context(|| format!("Failed to inspect log file '{}'", path.display()))?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        bail!("Log file '{}' must be a regular file", path.display());
    }
    if metadata.st_nlink != 1 {
        bail!(
            "Log file '{}' must have exactly one hard link",
            path.display()
        );
    }
    if metadata.st_uid != expected_owner {
        bail!(
            "Log file '{}' must be owned by the effective service user",
            path.display()
        );
    }

    fchmod(&fd, private_mode)
        .map_err(std::io::Error::from)
        .with_context(|| {
            format!(
                "Failed to set private log permissions on '{}'",
                path.display()
            )
        })?;
    let verified = fstat(&fd)
        .map_err(std::io::Error::from)
        .with_context(|| format!("Failed to verify log file '{}'", path.display()))?;
    if FileType::from_raw_mode(verified.st_mode) != FileType::RegularFile
        || verified.st_nlink != 1
        || verified.st_uid != expected_owner
        || Mode::from_raw_mode(verified.st_mode) != private_mode
    {
        bail!(
            "Log file '{}' changed while its security properties were being verified",
            path.display()
        );
    }

    Ok(fd.into())
}

fn writer_loop(receiver: Receiver<WriterCommand>, mut output: LogOutput, dropped: &AtomicU64) {
    let mut dirty = false;
    let mut next_flush = Instant::now() + LOG_FLUSH_INTERVAL;
    let mut last_drop_report = Instant::now()
        .checked_sub(LOG_DROP_REPORT_INTERVAL)
        .unwrap_or_else(Instant::now);
    loop {
        let wait = next_flush.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(wait) {
            Ok(WriterCommand::Entry(entry)) => {
                dirty |= report_dropped_if_due(
                    &mut output,
                    dropped,
                    &mut last_drop_report,
                    Instant::now(),
                    false,
                );
                if let Err(error) = output.write_line(&entry) {
                    eprintln!(
                        "{} ERROR log_writer_error={}",
                        timestamp(),
                        sanitize_log_line(&error.to_string())
                    );
                } else {
                    dirty = true;
                }
            }
            Ok(WriterCommand::Flush(done)) => {
                let _ = report_dropped_if_due(
                    &mut output,
                    dropped,
                    &mut last_drop_report,
                    Instant::now(),
                    true,
                );
                if let Err(error) = output.flush() {
                    eprintln!(
                        "{} ERROR log_flush_error={}",
                        timestamp(),
                        sanitize_log_line(&error.to_string())
                    );
                }
                dirty = false;
                next_flush = Instant::now() + LOG_FLUSH_INTERVAL;
                let _ = done.send(());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if Instant::now() >= next_flush {
            dirty |= report_dropped_if_due(
                &mut output,
                dropped,
                &mut last_drop_report,
                Instant::now(),
                false,
            );
            if dirty && let Err(error) = output.flush() {
                eprintln!(
                    "{} ERROR log_flush_error={}",
                    timestamp(),
                    sanitize_log_line(&error.to_string())
                );
            }
            dirty = false;
            next_flush = Instant::now() + LOG_FLUSH_INTERVAL;
        }
    }

    report_dropped(&mut output, dropped);
    let _ = output.flush();
}

fn report_dropped_if_due(
    output: &mut LogOutput,
    dropped: &AtomicU64,
    last_report: &mut Instant,
    now: Instant,
    force: bool,
) -> bool {
    if !force && now.saturating_duration_since(*last_report) < LOG_DROP_REPORT_INTERVAL {
        return false;
    }
    let reported = report_dropped(output, dropped);
    if reported {
        *last_report = now;
    }
    reported
}

fn report_dropped(output: &mut LogOutput, dropped: &AtomicU64) -> bool {
    let count = dropped.swap(0, Ordering::Relaxed);
    if count == 0 {
        return false;
    }
    let warning = LogEntry {
        text: format!(
            "{} WARN log_queue_overloaded dropped_newest={count} capacity={LOG_QUEUE_CAPACITY}",
            timestamp()
        ),
        stderr: true,
    };
    match output.write_line(&warning) {
        Ok(()) => true,
        Err(_) => {
            dropped.fetch_add(count, Ordering::Relaxed);
            false
        }
    }
}

fn timestamp() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn truncate_log_entry(mut value: String) -> String {
    if value.len() <= MAX_LOG_ENTRY_BYTES {
        return value;
    }

    let mut end = MAX_LOG_ENTRY_BYTES.saturating_sub(LOG_TRUNCATION_SUFFIX.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push_str(LOG_TRUNCATION_SUFFIX);
    value
}

/// Convert arbitrary text to one physical log line.
///
/// Structured access-log fields may add their own quoting, but every log
/// record passes through this final boundary so errors from external crates
/// and operating-system strings cannot inject extra records.
pub fn sanitize_log_line(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(output, "\\u{{{:x}}}", character as u32);
            }
            character => output.push(character),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log as _;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn arbitrary_values_are_reduced_to_one_physical_line() {
        assert_eq!(
            sanitize_log_line("first\r\nsecond\t\u{7f}"),
            "first\\r\\nsecond\\t\\u{7f}"
        );
    }

    #[test]
    fn queued_log_entries_have_a_utf8_safe_byte_limit() {
        let exact = "x".repeat(MAX_LOG_ENTRY_BYTES);
        assert_eq!(truncate_log_entry(exact.clone()), exact);

        let oversized = format!("prefix-{}", "中文".repeat(MAX_LOG_ENTRY_BYTES));
        let truncated = truncate_log_entry(oversized);
        assert!(truncated.len() <= MAX_LOG_ENTRY_BYTES);
        assert!(truncated.ends_with(LOG_TRUNCATION_SUFFIX));
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn log_files_are_private_and_existing_files_are_appended() {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let path = temporary.path().join("dufs.log");
        let mut file = open_log_file(&path).expect("securely create log");
        file.write_all(b"created\n").expect("write first log entry");
        file.flush().expect("flush first log entry");
        drop(file);
        assert_eq!(
            std::fs::metadata(&path)
                .expect("inspect created log")
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("set broad initial permissions");

        let mut file = open_log_file(&path).expect("securely open existing log");
        file.write_all(b"appended\n").expect("append log entry");
        file.flush().expect("flush log entry");
        drop(file);

        let metadata = std::fs::metadata(&path).expect("inspect log");
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read log"),
            "created\nappended\n"
        );
    }

    #[test]
    fn log_file_symlinks_are_rejected_without_touching_the_target() {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let target = temporary.path().join("target");
        let link = temporary.path().join("dufs.log");
        std::fs::write(&target, b"must remain unchanged").expect("create target");
        symlink(&target, &link).expect("create log symlink");

        let error = open_log_file(&link).expect_err("log symlink was accepted");
        assert!(
            format!("{error:#}").contains("securely open"),
            "unexpected error: {error:#}"
        );
        assert_eq!(
            std::fs::read(&target).expect("read target"),
            b"must remain unchanged"
        );
    }

    #[test]
    fn multiply_linked_and_unexpected_owner_log_files_are_rejected() {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let original = temporary.path().join("original");
        let linked = temporary.path().join("dufs.log");
        std::fs::write(&original, b"existing").expect("create original");
        std::fs::hard_link(&original, &linked).expect("create hard link");
        let error = open_log_file(&linked).expect_err("multiply linked log was accepted");
        assert!(
            error.to_string().contains("exactly one hard link"),
            "unexpected error: {error:#}"
        );

        let owner_path = temporary.path().join("owner.log");
        std::fs::write(&owner_path, b"existing").expect("create owner test log");
        let current_owner = geteuid().as_raw();
        let unexpected_owner = if current_owner == 0 { 1 } else { 0 };
        let error = open_log_file_for_owner(&owner_path, unexpected_owner)
            .expect_err("unexpected log owner was accepted");
        assert!(
            error.to_string().contains("effective service user"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn dropped_warning_is_rate_limited_but_force_flush_reports_pending_count() {
        let temporary = tempfile::NamedTempFile::new().expect("create temporary log");
        let mut output = LogOutput::File(BufWriter::new(
            temporary.reopen().expect("reopen temporary log"),
        ));
        let dropped = AtomicU64::new(3);
        let now = Instant::now();
        let mut last_report = now;

        assert!(!report_dropped_if_due(
            &mut output,
            &dropped,
            &mut last_report,
            now,
            false,
        ));
        assert_eq!(dropped.load(Ordering::Relaxed), 3);

        assert!(report_dropped_if_due(
            &mut output,
            &dropped,
            &mut last_report,
            now + LOG_DROP_REPORT_INTERVAL,
            false,
        ));
        dropped.store(2, Ordering::Relaxed);
        assert!(report_dropped_if_due(
            &mut output,
            &dropped,
            &mut last_report,
            now + LOG_DROP_REPORT_INTERVAL,
            true,
        ));
        output.flush().expect("flush temporary log");

        let contents = std::fs::read_to_string(temporary.path()).expect("read temporary log");
        assert_eq!(contents.matches("dropped_newest=").count(), 2);
        assert!(contents.contains("dropped_newest=3"));
        assert!(contents.contains("dropped_newest=2"));
    }

    #[test]
    fn full_queue_drops_the_newest_entry_without_blocking() {
        let (sender, receiver) = sync_channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let logger = AsyncLogger {
            sender,
            dropped: dropped.clone(),
        };

        logger.log(
            &Record::builder()
                .args(format_args!("first"))
                .level(Level::Info)
                .build(),
        );
        logger.log(
            &Record::builder()
                .args(format_args!("second"))
                .level(Level::Info)
                .build(),
        );

        assert!(matches!(
            receiver.try_recv(),
            Ok(WriterCommand::Entry(LogEntry { text, .. })) if text.ends_with(" first")
        ));
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn flush_waits_for_queued_entries_to_be_written() {
        let temporary = tempfile::NamedTempFile::new().expect("create temporary log");
        let output = LogOutput::File(BufWriter::new(
            temporary.reopen().expect("reopen temporary log"),
        ));
        let (sender, receiver) = sync_channel(2);
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_dropped = dropped.clone();
        let writer = thread::spawn(move || writer_loop(receiver, output, &writer_dropped));
        let logger = AsyncLogger { sender, dropped };

        logger.log(
            &Record::builder()
                .args(format_args!("durable line"))
                .level(Level::Info)
                .build(),
        );
        logger.flush();
        assert!(
            std::fs::read_to_string(temporary.path())
                .expect("read temporary log")
                .contains("durable line")
        );

        drop(logger);
        writer.join().expect("join log writer");
    }
}
