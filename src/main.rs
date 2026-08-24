use dufs::args::{Args, build_cli};
use dufs::auth::{MAX_PASSWORD_BYTES, hash_password};
use dufs::logger;
use dufs::server::{Server, ServerRuntime};

use anyhow::{Context, Result, anyhow};

use hyper::{Request, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use log::{error, info, warn};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use std::{
    future::Future,
    io::{self, IoSlice},
    net::{IpAddr, SocketAddr, TcpListener as StdTcpListener},
    pin::Pin,
    task::{Context as TaskContext, Poll},
};
use tokio::{
    io::unix::AsyncFd,
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Instant, sleep, sleep_until},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

const ACCEPT_BACKOFF_INITIAL: Duration = Duration::from_millis(50);
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(30);
const FORCED_SHUTDOWN_PERIOD: Duration = Duration::from_secs(10);
const RESPONSE_WRITE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    let cmd = build_cli();
    let matches = cmd.get_matches();
    if matches.subcommand_matches("hash-password").is_some() {
        let password = rpassword::prompt_password("Password: ")?;
        validate_cli_password(&password)?;
        let confirmation = rpassword::prompt_password("Confirm password: ")?;
        if password != confirmation {
            anyhow::bail!("Passwords do not match");
        }
        println!("{}", hash_password(&password)?);
        return Ok(());
    }
    let args = Args::parse(matches)?;
    logger::init(args.log_file.clone()).map_err(|e| anyhow!("Failed to init logger, {e}"))?;
    let result = run_server(args).await;
    if let Err(error) = &result {
        error!("Server failed: {error:#}");
        log::logger().flush();
    }
    result
}

async fn run_server(args: Args) -> Result<()> {
    let print_addrs = args.addrs.clone();
    let mut signals = ShutdownSignals::new()?;
    let serving = serve(args)?;
    let listening = print_listening(&print_addrs, serving.port);
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
    graceful_exit(&mut signals).await
}

fn validate_cli_password(password: &str) -> Result<()> {
    if password.is_empty() {
        anyhow::bail!("Password must not be empty");
    }
    if password.len() > MAX_PASSWORD_BYTES {
        anyhow::bail!("Password exceeds the {MAX_PASSWORD_BYTES}-byte limit");
    }
    Ok(())
}

struct Serving {
    port: u16,
    listener_tasks: TaskTracker,
    connection_tasks: TaskTracker,
    shutdown: CancellationToken,
    runtime: ServerRuntime,
}

#[derive(Clone)]
struct ListenerRuntime {
    server: Arc<Server>,
    shutdown: CancellationToken,
    force_shutdown: CancellationToken,
    connection_tasks: TaskTracker,
    connection_slots: Arc<Semaphore>,
}

impl Serving {
    async fn shutdown(self, reason: &str, signals: &mut ShutdownSignals) -> Result<()> {
        info!(
            "Graceful shutdown started reason={reason} grace_seconds={}",
            SHUTDOWN_GRACE_PERIOD.as_secs()
        );

        // Stop accepting first. Existing connections observe the same token and
        // ask Hyper to stop HTTP/1 keep-alive admission after their active work.
        self.shutdown.cancel();
        self.listener_tasks.wait().await;
        self.connection_tasks.close();

        let deadline = Instant::now() + SHUTDOWN_GRACE_PERIOD;
        let connections_drained = tokio::select! {
            biased;
            signal = signals.recv() => force_exit(signal?),
            _ = self.connection_tasks.wait() => true,
            _ = sleep_until(deadline) => false,
        };
        let mut forced_deadline = None;

        if !connections_drained {
            let (active_work, active_mutations) = self.runtime.active_task_counts();
            warn!(
                "Graceful shutdown deadline reached; cancelling ordinary work \
                 active_connections={} active_tasks={} active_mutations={}",
                self.connection_tasks.len(),
                active_work,
                active_mutations,
            );
            self.runtime.request_force_shutdown();
            let deadline = Instant::now() + FORCED_SHUTDOWN_PERIOD;
            forced_deadline = Some(deadline);
            tokio::select! {
                biased;
                signal = signals.recv() => force_exit(signal?),
                _ = self.connection_tasks.wait() => {}
                _ = sleep_until(deadline) => hard_deadline_exit(
                    self.connection_tasks.len() + self.runtime.active_task_counts().0,
                    self.runtime.active_task_counts().1,
                ),
            }
        }

        // A request can register a durable filesystem mutation only while its
        // connection task is alive. Once all connection tasks have drained,
        // ServerRuntime can close its own work and mutation trackers without
        // racing a late registration.
        let runtime_shutdown = self.runtime.shutdown();
        tokio::pin!(runtime_shutdown);
        if connections_drained {
            let runtime_drained = tokio::select! {
                biased;
                signal = signals.recv() => force_exit(signal?),
                _ = &mut runtime_shutdown => true,
                _ = sleep_until(deadline) => false,
            };
            if !runtime_drained {
                let (active_work, active_mutations) = self.runtime.active_task_counts();
                warn!(
                    "Graceful shutdown deadline reached while draining server runtime; \
                     cancelling ordinary work active_tasks={} active_mutations={}",
                    active_work, active_mutations,
                );
                self.runtime.request_force_shutdown();
                let forced_deadline = Instant::now() + FORCED_SHUTDOWN_PERIOD;
                tokio::select! {
                    biased;
                    signal = signals.recv() => force_exit(signal?),
                    _ = &mut runtime_shutdown => {}
                    _ = sleep_until(forced_deadline) => hard_deadline_exit(
                        self.runtime.active_task_counts().0,
                        self.runtime.active_task_counts().1,
                    ),
                }
            }
        } else {
            let forced_deadline =
                forced_deadline.expect("forced work cancellation established a hard deadline");
            tokio::select! {
                biased;
                signal = signals.recv() => force_exit(signal?),
                _ = &mut runtime_shutdown => {}
                _ = sleep_until(forced_deadline) => hard_deadline_exit(
                    self.runtime.active_task_counts().0,
                    self.runtime.active_task_counts().1,
                ),
            }
        }

        info!("Graceful shutdown complete");
        Ok(())
    }
}

fn serve(args: Args) -> Result<Serving> {
    let addrs = args.addrs.clone();
    let mut port = args.port;
    let mut listeners = Vec::with_capacity(addrs.len());

    for ip in addrs {
        let listener = create_listener(SocketAddr::new(ip, port))
            .with_context(|| format!("Failed to bind `{ip}:{port}`"))?;
        if port == 0 {
            port = listener
                .get_ref()
                .local_addr()
                .context("Failed to inspect the dynamically assigned listen port")?
                .port();
        }
        listeners.push(listener);
    }

    let connection_slots = Arc::new(Semaphore::new(args.max_connections));
    let listener_tasks = TaskTracker::new();
    let connection_tasks = TaskTracker::new();
    let runtime = Server::builder(args).build()?;
    let server_handle = runtime.server().clone();
    let shutdown = runtime.shutdown_token();
    let force_shutdown = runtime.force_shutdown_token();

    for listener in listeners {
        let runtime = ListenerRuntime {
            server: server_handle.clone(),
            shutdown: shutdown.clone(),
            force_shutdown: force_shutdown.clone(),
            connection_tasks: connection_tasks.clone(),
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
        connection_tasks,
        shutdown,
        runtime,
    })
}

async fn serve_tcp_listener(listener: AsyncFd<StdTcpListener>, runtime: ListenerRuntime) {
    let ListenerRuntime {
        server,
        shutdown,
        force_shutdown,
        connection_tasks,
        connection_slots,
    } = runtime;
    let listener_addr = listener
        .get_ref()
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    let mut backoff = AcceptBackoff::default();

    loop {
        let Some((stream, addr, connection_permit)) = accept_with_backoff(
            &listener,
            &listener_addr,
            &shutdown,
            &connection_slots,
            &mut backoff,
        )
        .await
        else {
            break;
        };
        let server = server.clone();
        let connection_shutdown = shutdown.clone();
        let connection_force_shutdown = force_shutdown.clone();
        drop(connection_tasks.spawn(async move {
            let _connection_permit = connection_permit;
            handle_stream(
                server,
                TokioIo::new(WriteIdleTimeout::new(stream, RESPONSE_WRITE_IDLE_TIMEOUT)),
                addr,
                connection_shutdown,
                connection_force_shutdown,
            )
            .await;
        }));
    }
}

struct WriteIdleTimeout<T> {
    inner: T,
    timeout: Duration,
    timer: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<T> WriteIdleTimeout<T> {
    fn new(inner: T, timeout: Duration) -> Self {
        debug_assert!(!timeout.is_zero());
        Self {
            inner,
            timeout,
            timer: None,
        }
    }

    fn clear_timer(&mut self) {
        self.timer = None;
    }

    fn write_is_timed_out(&mut self, context: &mut TaskContext<'_>) -> bool {
        let timer = self
            .timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(self.timeout)));
        timer.as_mut().poll(context).is_ready()
    }

    fn timeout_error() -> io::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "HTTP response write made no progress before the idle deadline",
        )
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for WriteIdleTimeout<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for WriteIdleTimeout<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.as_mut().get_mut();
        match Pin::new(&mut this.inner).poll_write(context, buffer) {
            Poll::Ready(result) => {
                this.clear_timer();
                Poll::Ready(result)
            }
            Poll::Pending if this.write_is_timed_out(context) => {
                Poll::Ready(Err(Self::timeout_error()))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.as_mut().get_mut();
        match Pin::new(&mut this.inner).poll_write_vectored(context, buffers) {
            Poll::Ready(result) => {
                this.clear_timer();
                Poll::Ready(result)
            }
            Poll::Pending if this.write_is_timed_out(context) => {
                Poll::Ready(Err(Self::timeout_error()))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        match Pin::new(&mut this.inner).poll_flush(context) {
            Poll::Ready(result) => {
                this.clear_timer();
                Poll::Ready(result)
            }
            Poll::Pending if this.write_is_timed_out(context) => {
                Poll::Ready(Err(Self::timeout_error()))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        match Pin::new(&mut this.inner).poll_shutdown(context) {
            Poll::Ready(result) => {
                this.clear_timer();
                Poll::Ready(result)
            }
            Poll::Pending if this.write_is_timed_out(context) => {
                Poll::Ready(Err(Self::timeout_error()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

async fn accept_with_backoff(
    listener: &AsyncFd<StdTcpListener>,
    listener_addr: &str,
    shutdown: &CancellationToken,
    connection_slots: &Arc<Semaphore>,
    backoff: &mut AcceptBackoff,
) -> Option<(TcpStream, SocketAddr, OwnedSemaphorePermit)> {
    loop {
        let readiness = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return None,
            result = listener.readable() => result,
        };
        let mut readiness = match readiness {
            Ok(readiness) => readiness,
            Err(err) => {
                if !wait_after_accept_error(listener_addr, shutdown, backoff, &err).await {
                    return None;
                }
                continue;
            }
        };

        // AsyncFd deliberately keeps a successful readiness observation cached
        // so callers can drain an fd. We cannot drain a listener before owning
        // capacity, however: waiting for a permit on that stale cache would let
        // an idle listener starve another bind. Recheck the kernel's level state
        // without accepting; clear only the stale observation.
        match listener_is_readable(listener.get_ref()) {
            Ok(true) => {}
            Ok(false) => {
                readiness.clear_ready();
                continue;
            }
            Err(err) => {
                drop(readiness);
                if !wait_after_accept_error(listener_addr, shutdown, backoff, &err).await {
                    return None;
                }
                continue;
            }
        }

        // Wait for a global slot only after this listener has a connection
        // ready. An idle bind therefore cannot reserve capacity from another
        // address, while every socket accepted into userspace already owns the
        // permit that bounds its lifetime.
        let connection_permit = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return None,
            permit = connection_slots.clone().acquire_owned() => {
                permit.expect("the connection semaphore is never closed")
            }
        };
        if shutdown.is_cancelled() {
            return None;
        }

        let accepted = match readiness.try_io(|listener| listener.get_ref().accept()) {
            Ok(result) => result,
            Err(_) => continue,
        };
        drop(readiness);
        match accepted {
            Ok((stream, addr)) => {
                if let Err(err) = stream.set_nonblocking(true) {
                    drop(connection_permit);
                    if !wait_after_accept_error(listener_addr, shutdown, backoff, &err).await {
                        return None;
                    }
                    continue;
                }
                let stream = match TcpStream::from_std(stream) {
                    Ok(stream) => stream,
                    Err(err) => {
                        drop(connection_permit);
                        if !wait_after_accept_error(listener_addr, shutdown, backoff, &err).await {
                            return None;
                        }
                        continue;
                    }
                };
                backoff.reset();
                if shutdown.is_cancelled() {
                    return None;
                }
                return Some((stream, addr, connection_permit));
            }
            Err(err) => {
                drop(connection_permit);
                if !wait_after_accept_error(listener_addr, shutdown, backoff, &err).await {
                    return None;
                }
            }
        }
    }
}

fn listener_is_readable(listener: &StdTcpListener) -> io::Result<bool> {
    let mut descriptors = [PollFd::new(listener, PollFlags::IN)];
    let no_wait = Timespec::default();
    let ready = poll(&mut descriptors, Some(&no_wait)).map_err(io::Error::from)?;
    Ok(ready > 0 && !descriptors[0].revents().is_empty())
}

async fn wait_after_accept_error(
    listener_addr: &str,
    shutdown: &CancellationToken,
    backoff: &mut AcceptBackoff,
    err: &io::Error,
) -> bool {
    let retry_delay = backoff.failure_delay();
    log_accept_error(listener_addr, err, retry_delay);
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => false,
        _ = sleep(retry_delay) => true,
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
) where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let request_seen = Arc::new(AtomicBool::new(false));
    let service_request_seen = request_seen.clone();
    let hyper_service = service_fn(move |request: Request<Incoming>| {
        service_request_seen.store(true, Ordering::Relaxed);
        handle.clone().call(request, addr)
    });

    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10))
        .max_buf_size(64 * 1024);
    let connection = builder.serve_connection(stream, hyper_service);
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
        log_connection_error(addr, request_seen.load(Ordering::Relaxed), &err);
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

fn create_listener(addr: SocketAddr) -> Result<AsyncFd<StdTcpListener>> {
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
    let listener = AsyncFd::new(std_listener)?;
    Ok(listener)
}

fn print_listening(print_addrs: &[IpAddr], port: u16) -> String {
    let mut output = String::new();
    let urls = print_addrs
        .iter()
        .map(|addr| {
            let addr = match addr {
                IpAddr::V4(_) => format!("{addr}:{port}"),
                IpAddr::V6(_) => format!("[{addr}]:{port}"),
            };
            format!("http://{addr}/")
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
    let exit_code = match signal {
        "SIGTERM" => 143,
        _ => 130,
    };
    std::process::exit(exit_code)
}

async fn graceful_exit(signals: &mut ShutdownSignals) -> Result<()> {
    // Tokio waits for every spawn_blocking worker when its runtime is dropped.
    // A cancelled filesystem request can still be inside an uninterruptible
    // kernel/FUSE call after all tracked request and commit guards are gone.
    // Use a dedicated OS thread rather than Tokio's blocking pool: abnormal
    // filesystems may already have saturated that pool with uninterruptible
    // calls. Keep polling the installed signal streams while the logger waits
    // for its own bounded acknowledgement.
    let (flush_complete, mut flush_waiter) = tokio::sync::oneshot::channel();
    if let Err(error) = std::thread::Builder::new()
        .name("dufs-final-log-flush".to_string())
        .spawn(move || {
            log::logger().flush();
            let _ = flush_complete.send(());
        })
    {
        eprintln!("Failed to start final log flush thread: {error}");
        std::process::exit(1);
    }
    tokio::select! {
        biased;
        signal = signals.recv() => force_exit(signal?),
        result = &mut flush_waiter => {
            if result.is_err() {
                eprintln!("Final log flush thread ended without acknowledgement");
            }
            // Cleanup and logging are complete at this point, so terminate
            // explicitly instead of allowing runtime teardown to defeat the
            // advertised deadline.
            std::process::exit(0)
        }
    }
}

fn hard_deadline_exit(active_work: usize, active_mutations: usize) -> ! {
    error!(
        "Forced shutdown deadline reached; exiting despite stuck tasks active_tasks={active_work} active_mutations={active_mutations}"
    );
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt as _;

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
    fn cli_password_validation_uses_the_login_byte_limit() {
        assert!(validate_cli_password("").is_err());
        for password in [
            "p".repeat(MAX_PASSWORD_BYTES),
            "é".repeat(MAX_PASSWORD_BYTES / "é".len()),
        ] {
            assert!(validate_cli_password(&password).is_ok());
        }
        for password in [
            "p".repeat(MAX_PASSWORD_BYTES + 1),
            format!("a{}", "é".repeat(MAX_PASSWORD_BYTES / "é".len())),
        ] {
            assert!(validate_cli_password(&password).is_err());
        }
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
    async fn response_write_idle_timeout_interrupts_a_stalled_peer() {
        let (stream, _peer) = tokio::io::duplex(1);
        let mut stream = WriteIdleTimeout::new(stream, Duration::from_millis(20));
        let error = tokio::time::timeout(Duration::from_secs(1), stream.write_all(b"ab"))
            .await
            .expect("the write timeout itself did not fire")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
