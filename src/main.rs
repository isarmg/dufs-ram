use dufs::args::{Args, build_cli};
use dufs::auth::hash_password;
use dufs::logger;
use dufs::server::Server;

use anyhow::{Context, Result, anyhow};

use hyper::{Request, body::Incoming, rt::Executor, service::service_fn};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    server::conn::auto::Builder,
};
use log::{error, info, warn};
use std::future::Future;
use std::net::{IpAddr, SocketAddr, TcpListener as StdTcpListener};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    time::{Instant, sleep, sleep_until},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

const ACCEPT_BACKOFF_INITIAL: Duration = Duration::from_millis(50);
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    let cmd = build_cli();
    let matches = cmd.get_matches();
    if matches.subcommand_matches("hash-password").is_some() {
        let password = rpassword::prompt_password("Password: ")?;
        if password.is_empty() {
            anyhow::bail!("Password must not be empty");
        }
        let confirmation = rpassword::prompt_password("Confirm password: ")?;
        if password != confirmation {
            anyhow::bail!("Passwords do not match");
        }
        println!("{}", hash_password(&password)?);
        return Ok(());
    }
    let args = Args::parse(matches)?;
    logger::init(args.log_file.clone()).map_err(|e| anyhow!("Failed to init logger, {e}"))?;
    let print_addrs = args.addrs.clone();
    let print_uri_prefix = args.uri_prefix.clone();
    let running = Arc::new(AtomicBool::new(true));
    let mut signals = ShutdownSignals::new()?;
    let serving = serve(args, running)?;
    let listening = print_listening(&print_addrs, serving.port, &print_uri_prefix);
    println!("{listening}");

    let reason = tokio::select! {
        biased;
        signal = signals.recv() => signal?,
        _ = serving.listener_tasks.wait() => {
            error!("All listener tasks exited unexpectedly");
            "listener-exit"
        }
    };
    serving.shutdown(reason, &mut signals).await?;
    Ok(())
}

struct Serving {
    port: u16,
    listener_tasks: TaskTracker,
    work_tasks: TaskTracker,
    commit_tasks: TaskTracker,
    shutdown: CancellationToken,
    force_shutdown: CancellationToken,
    running: Arc<AtomicBool>,
}

#[derive(Clone)]
struct ListenerRuntime {
    server: Arc<Server>,
    shutdown: CancellationToken,
    force_shutdown: CancellationToken,
    work_tasks: TaskTracker,
    connection_slots: Arc<Semaphore>,
}

impl Serving {
    async fn shutdown(self, reason: &str, signals: &mut ShutdownSignals) -> Result<()> {
        info!(
            "Graceful shutdown started reason={reason} grace_seconds={}",
            SHUTDOWN_GRACE_PERIOD.as_secs()
        );

        // Stop accepting first. Existing connections observe the same token and
        // ask Hyper to stop keep-alive/HTTP2 admission after their active work.
        self.shutdown.cancel();
        self.listener_tasks.wait().await;
        self.work_tasks.close();

        let deadline = Instant::now() + SHUTDOWN_GRACE_PERIOD;
        let work_drained = tokio::select! {
            biased;
            signal = signals.recv() => force_exit(signal?),
            _ = self.work_tasks.wait() => true,
            _ = sleep_until(deadline) => false,
        };

        if !work_drained {
            warn!(
                "Graceful shutdown deadline reached; cancelling ordinary work active_tasks={}",
                self.work_tasks.len()
            );
            self.running.store(false, Ordering::SeqCst);
            self.force_shutdown.cancel();
            tokio::select! {
                biased;
                signal = signals.recv() => force_exit(signal?),
                _ = self.work_tasks.wait() => {}
            }
        }

        // A request that can create a tracked filesystem mutation is itself a
        // work task. It registers the mutation before it can decrement the
        // work count, so once work is empty there can be no late registration.
        self.commit_tasks.close();
        if !self.commit_tasks.is_empty() {
            info!(
                "Waiting for durable filesystem mutations active_mutations={}",
                self.commit_tasks.len()
            );
        }

        if work_drained {
            let commits_drained = tokio::select! {
                biased;
                signal = signals.recv() => force_exit(signal?),
                _ = self.commit_tasks.wait() => true,
                _ = sleep_until(deadline) => false,
            };
            if !commits_drained {
                warn!(
                    "Graceful shutdown deadline reached during durable filesystem mutation; \
                     waiting for filesystem synchronization active_mutations={}",
                    self.commit_tasks.len()
                );
                self.running.store(false, Ordering::SeqCst);
                self.force_shutdown.cancel();
                tokio::select! {
                    biased;
                    signal = signals.recv() => force_exit(signal?),
                    _ = self.commit_tasks.wait() => {}
                }
            }
        } else {
            tokio::select! {
                biased;
                signal = signals.recv() => force_exit(signal?),
                _ = self.commit_tasks.wait() => {}
            }
        }

        self.running.store(false, Ordering::SeqCst);
        info!("Graceful shutdown complete");
        log::logger().flush();
        Ok(())
    }
}

fn serve(args: Args, running: Arc<AtomicBool>) -> Result<Serving> {
    let addrs = args.addrs.clone();
    let mut port = args.port;
    let connection_slots = Arc::new(Semaphore::new(args.max_connections));
    let listener_tasks = TaskTracker::new();
    let work_tasks = TaskTracker::new();
    let commit_tasks = TaskTracker::new();
    let shutdown = CancellationToken::new();
    let force_shutdown = CancellationToken::new();
    let server_handle = Arc::new(Server::init(
        args,
        running.clone(),
        work_tasks.clone(),
        commit_tasks.clone(),
        shutdown.clone(),
        force_shutdown.clone(),
    )?);
    server_handle.start_maintenance();

    for ip in addrs {
        let listener = create_listener(SocketAddr::new(ip, port))
            .with_context(|| format!("Failed to bind `{ip}:{port}`"))?;
        if port == 0 {
            port = listener
                .local_addr()
                .context("Failed to inspect the dynamically assigned listen port")?
                .port();
        }

        let runtime = ListenerRuntime {
            server: server_handle.clone(),
            shutdown: shutdown.clone(),
            force_shutdown: force_shutdown.clone(),
            work_tasks: work_tasks.clone(),
            connection_slots: connection_slots.clone(),
        };
        drop(listener_tasks.spawn(async move {
            serve_tcp_listener(listener, runtime).await;
        }));
    }
    listener_tasks.close();
    Ok(Serving {
        port,
        listener_tasks,
        work_tasks,
        commit_tasks,
        shutdown,
        force_shutdown,
        running,
    })
}

async fn serve_tcp_listener(listener: TcpListener, runtime: ListenerRuntime) {
    let ListenerRuntime {
        server,
        shutdown,
        force_shutdown,
        work_tasks,
        connection_slots,
    } = runtime;
    let listener_addr = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    let mut backoff = AcceptBackoff::default();

    loop {
        let connection_permit = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            permit = connection_slots.clone().acquire_owned() => {
                permit.expect("the connection semaphore is never closed")
            }
        };
        let Some((stream, addr)) =
            accept_with_backoff(&listener, &listener_addr, &shutdown, &mut backoff).await
        else {
            break;
        };
        let server = server.clone();
        let connection_shutdown = shutdown.clone();
        let connection_force_shutdown = force_shutdown.clone();
        let connection_work_tasks = work_tasks.clone();
        drop(work_tasks.spawn(async move {
            let _connection_permit = connection_permit;
            handle_stream(
                server,
                TokioIo::new(stream),
                addr,
                connection_shutdown,
                connection_force_shutdown,
                connection_work_tasks,
            )
            .await;
        }));
    }
}

async fn accept_with_backoff(
    listener: &TcpListener,
    listener_addr: &str,
    shutdown: &CancellationToken,
    backoff: &mut AcceptBackoff,
) -> Option<(TcpStream, SocketAddr)> {
    loop {
        let result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return None,
            result = listener.accept() => result,
        };

        match result {
            Ok(connection) => {
                backoff.reset();
                if shutdown.is_cancelled() {
                    return None;
                }
                return Some(connection);
            }
            Err(err) => {
                let retry_delay = backoff.failure_delay();
                log_accept_error(listener_addr, &err, retry_delay);
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return None,
                    _ = sleep(retry_delay) => {}
                }
            }
        }
    }
}

#[derive(Debug)]
struct AcceptBackoff {
    next_delay: Duration,
}

impl Default for AcceptBackoff {
    fn default() -> Self {
        Self {
            next_delay: ACCEPT_BACKOFF_INITIAL,
        }
    }
}

impl AcceptBackoff {
    fn failure_delay(&mut self) -> Duration {
        let delay = self.next_delay;
        self.next_delay = self.next_delay.saturating_mul(2).min(ACCEPT_BACKOFF_MAX);
        delay
    }

    fn reset(&mut self) {
        self.next_delay = ACCEPT_BACKOFF_INITIAL;
    }
}

fn log_accept_error(listener_addr: &str, err: &std::io::Error, retry_delay: Duration) {
    let category = classify_accept_error(err);
    warn!(
        "TCP accept error listener={listener_addr} category={category} io_kind={:?} \
         os_error={:?} retry_ms={}",
        err.kind(),
        err.raw_os_error(),
        retry_delay.as_millis()
    );
}

fn classify_accept_error(err: &std::io::Error) -> &'static str {
    if is_resource_exhaustion(err) {
        "resource"
    } else {
        match err.kind() {
            std::io::ErrorKind::Interrupted => "interrupted",
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "transient",
            std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset => {
                "connection"
            }
            std::io::ErrorKind::PermissionDenied => "permission",
            std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::NotConnected => "listener",
            _ => "io",
        }
    }
}

fn is_resource_exhaustion(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::OutOfMemory {
        return true;
    }
    // Linux ENOMEM, ENFILE, EMFILE and ENOBUFS.
    matches!(err.raw_os_error(), Some(12 | 23 | 24 | 105))
}

async fn handle_stream<T>(
    handle: Arc<Server>,
    stream: TokioIo<T>,
    addr: SocketAddr,
    shutdown: CancellationToken,
    force_shutdown: CancellationToken,
    work_tasks: TaskTracker,
) where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let request_seen = Arc::new(AtomicBool::new(false));
    let service_request_seen = request_seen.clone();
    let hyper_service = service_fn(move |request: Request<Incoming>| {
        service_request_seen.store(true, Ordering::Relaxed);
        handle.clone().call(request, addr)
    });

    let mut builder = Builder::new(TrackedExecutor {
        work_tasks,
        force_shutdown: force_shutdown.clone(),
    });
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10))
        .max_buf_size(64 * 1024);
    let connection = builder.serve_connection_with_upgrades(stream, hyper_service);
    tokio::pin!(connection);
    let result = tokio::select! {
        biased;
        _ = force_shutdown.cancelled() => return,
        _ = shutdown.cancelled() => {
            connection.as_mut().graceful_shutdown();
            tokio::select! {
                biased;
                _ = force_shutdown.cancelled() => return,
                result = &mut connection => result,
            }
        }
        result = &mut connection => result,
    };
    if let Err(err) = result {
        log_connection_error(addr, request_seen.load(Ordering::Relaxed), err.as_ref());
    }
}

#[derive(Clone)]
struct TrackedExecutor {
    work_tasks: TaskTracker,
    force_shutdown: CancellationToken,
}

impl<Fut> Executor<Fut> for TrackedExecutor
where
    Fut: Future + Send + 'static,
    Fut::Output: Send + 'static,
{
    fn execute(&self, future: Fut) {
        let force_shutdown = self.force_shutdown.clone();
        drop(self.work_tasks.spawn(async move {
            tokio::select! {
                biased;
                _ = force_shutdown.cancelled() => {}
                _ = future => {}
            }
        }));
    }
}

fn log_connection_error(
    addr: SocketAddr,
    request_seen: bool,
    err: &(dyn std::error::Error + Send + Sync + 'static),
) {
    let hyper_error = err.downcast_ref::<hyper::Error>();
    let io_error = find_io_error(err);
    let io_kind = io_error.map(std::io::Error::kind);
    let io_disconnect = matches!(
        io_kind,
        Some(
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::NotConnected
        )
    );
    let benign_probe_close =
        hyper_error.is_some_and(hyper::Error::is_incomplete_message) || io_disconnect;
    if !request_seen && benign_probe_close {
        return;
    }

    let category = if hyper_error.is_some_and(hyper::Error::is_parse) {
        "protocol"
    } else if hyper_error.is_some_and(hyper::Error::is_timeout)
        || io_kind == Some(std::io::ErrorKind::TimedOut)
    {
        "timeout"
    } else if hyper_error.is_some_and(|err| err.is_user() || err.is_body_write_aborted()) {
        "service"
    } else if hyper_error
        .is_some_and(|err| err.is_incomplete_message() || err.is_canceled() || err.is_closed())
        || io_disconnect
    {
        "disconnect"
    } else if io_error.is_some() {
        "io"
    } else if hyper_error.is_some() {
        "hyper"
    } else {
        "unknown"
    };
    let io_kind = io_kind
        .map(|kind| format!("{kind:?}"))
        .unwrap_or_else(|| "-".to_string());
    let message = format!(
        "HTTP connection error peer={addr} category={category} request_seen={request_seen} io_kind={io_kind} error={err:?}"
    );
    if category == "disconnect" {
        info!("{message}");
    } else {
        warn!("{message}");
    }
}

fn find_io_error<'a>(
    err: &'a (dyn std::error::Error + Send + Sync + 'static),
) -> Option<&'a std::io::Error> {
    let mut current: Option<&'a (dyn std::error::Error + 'static)> = Some(err);
    while let Some(error) = current {
        if let Some(io_error) = error.downcast_ref::<std::io::Error>() {
            return Some(io_error);
        }
        current = error.source();
    }
    None
}

fn create_listener(addr: SocketAddr) -> Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024 /* Default backlog */)?;
    let std_listener = StdTcpListener::from(socket);
    std_listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std_listener)?;
    Ok(listener)
}

fn print_listening(print_addrs: &[IpAddr], port: u16, uri_prefix: &str) -> String {
    let mut output = String::new();
    let urls = print_addrs
        .iter()
        .map(|addr| {
            let addr = match addr {
                IpAddr::V4(_) => format!("{addr}:{port}"),
                IpAddr::V6(_) => format!("[{addr}]:{port}"),
            };
            format!("http://{addr}{uri_prefix}")
        })
        .collect::<Vec<_>>();

    if urls.len() == 1 {
        output.push_str(&format!("Listening on {}", urls[0]))
    } else {
        let info = urls
            .iter()
            .map(|v| format!("  {v}"))
            .collect::<Vec<String>>()
            .join("\n");
        output.push_str(&format!("Listening on:\n{info}\n"))
    }

    output
}

struct ShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            interrupt: signal(SignalKind::interrupt())
                .context("Failed to install SIGINT signal handler")?,
            terminate: signal(SignalKind::terminate())
                .context("Failed to install SIGTERM signal handler")?,
        })
    }

    async fn recv(&mut self) -> Result<&'static str> {
        tokio::select! {
            biased;
            value = self.interrupt.recv() => {
                value.map(|()| "SIGINT").ok_or_else(|| anyhow!("SIGINT signal stream closed"))
            }
            value = self.terminate.recv() => {
                value.map(|()| "SIGTERM").ok_or_else(|| anyhow!("SIGTERM signal stream closed"))
            }
        }
    }
}

fn force_exit(signal: &str) -> ! {
    warn!(
        "Second shutdown signal received signal={signal}; forcing exit without waiting for cleanup"
    );
    log::logger().flush();
    let exit_code = match signal {
        "SIGTERM" => 143,
        _ => 130,
    };
    std::process::exit(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_backoff_is_bounded_and_resets_after_success() {
        let mut backoff = AcceptBackoff::default();
        assert_eq!(backoff.failure_delay(), Duration::from_millis(50));
        assert_eq!(backoff.failure_delay(), Duration::from_millis(100));
        assert_eq!(backoff.failure_delay(), Duration::from_millis(200));
        assert_eq!(backoff.failure_delay(), Duration::from_millis(400));
        assert_eq!(backoff.failure_delay(), Duration::from_millis(800));
        assert_eq!(backoff.failure_delay(), ACCEPT_BACKOFF_MAX);
        assert_eq!(backoff.failure_delay(), ACCEPT_BACKOFF_MAX);

        backoff.reset();
        assert_eq!(backoff.failure_delay(), ACCEPT_BACKOFF_INITIAL);
    }

    #[test]
    fn accept_errors_are_classified_for_diagnostics() {
        assert_eq!(
            classify_accept_error(&std::io::Error::from(std::io::ErrorKind::Interrupted)),
            "interrupted"
        );
        assert_eq!(
            classify_accept_error(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            "permission"
        );
        assert_eq!(
            classify_accept_error(&std::io::Error::from(std::io::ErrorKind::OutOfMemory)),
            "resource"
        );
    }

    #[tokio::test]
    async fn tracked_executor_tasks_are_counted_and_force_cancelled() {
        let work_tasks = TaskTracker::new();
        let force_shutdown = CancellationToken::new();
        let executor = TrackedExecutor {
            work_tasks: work_tasks.clone(),
            force_shutdown: force_shutdown.clone(),
        };
        executor.execute(std::future::pending::<()>());
        assert_eq!(work_tasks.len(), 1);

        work_tasks.close();
        force_shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), work_tasks.wait())
            .await
            .expect("tracked executor task did not observe forced shutdown");
    }
}
