use std::{io, sync::Arc, sync::OnceLock};
use tokio::{sync::Semaphore, task};

// Tokio's blocking pool has a deliberately high default ceiling because it
// serves many workloads. Filesystem calls against FUSE, network mounts, or a
// failing device can remain uninterruptible after their request times out, so
// admit a much smaller bounded set for this server's disk work.
const BLOCKING_IO_CAPACITY: usize = 64;

static BLOCKING_IO_GATE: OnceLock<BlockingIoGate> = OnceLock::new();

#[derive(Clone)]
pub(super) struct BlockingIoGate {
    slots: Arc<Semaphore>,
}

pub(super) fn blocking_io_gate() -> &'static BlockingIoGate {
    BLOCKING_IO_GATE.get_or_init(|| BlockingIoGate::new(BLOCKING_IO_CAPACITY))
}

impl BlockingIoGate {
    fn new(capacity: usize) -> Self {
        debug_assert!(capacity > 0);
        Self {
            slots: Arc::new(Semaphore::new(capacity)),
        }
    }

    #[cfg(test)]
    pub(super) fn with_capacity_for_test(capacity: usize) -> Self {
        Self::new(capacity)
    }

    /// Run blocking work after bounded asynchronous admission.
    ///
    /// The owned permit deliberately lives inside the blocking closure. A
    /// caller may stop awaiting this future, but Tokio cannot cancel a
    /// blocking syscall that has started, so capacity is released only when
    /// the actual worker exits.
    pub(super) async fn run<T, F>(&self, work: F) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| io::Error::other("blocking I/O admission was closed"))?;
        task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .map_err(io::Error::other)
    }

    pub(super) async fn run_io<T, F>(&self, work: F) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> io::Result<T> + Send + 'static,
    {
        self.run(work).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[tokio::test]
    async fn detached_waiter_keeps_capacity_until_the_blocking_worker_exits() {
        let gate = BlockingIoGate::new(1);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_gate = gate.clone();
        let first = tokio::spawn(async move {
            first_gate
                .run_io(move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .await
        });
        tokio::task::spawn_blocking(move || {
            entered_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("blocking worker did not start")
        })
        .await
        .unwrap();

        first.abort();
        let second_gate = gate.clone();
        let mut second = tokio::spawn(async move { second_gate.run_io(|| Ok(7_u8)).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut second)
                .await
                .is_err(),
            "aborting the waiter released capacity before its syscall exited"
        );

        release_tx.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), second)
                .await
                .expect("second worker remained blocked")
                .expect("second admission task failed")
                .expect("second blocking worker failed"),
            7
        );
    }

    #[tokio::test]
    async fn dropping_a_queued_future_prevents_its_blocking_work_from_starting() {
        let gate = BlockingIoGate::new(1);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_gate = gate.clone();
        let first = tokio::spawn(async move {
            first_gate
                .run_io(move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .await
        });
        tokio::task::spawn_blocking(move || {
            entered_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("blocking worker did not start")
        })
        .await
        .unwrap();

        let (queued_tx, queued_rx) = mpsc::channel();
        let mut queued = Box::pin(gate.run_io(move || {
            queued_tx.send(()).unwrap();
            Ok(())
        }));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), queued.as_mut())
                .await
                .is_err(),
            "queued work unexpectedly acquired the occupied slot"
        );
        drop(queued);

        release_tx.send(()).unwrap();
        first
            .await
            .expect("first admission task failed")
            .expect("first blocking worker failed");
        gate.run_io(|| Ok(())).await.unwrap();
        assert!(
            queued_rx.try_recv().is_err(),
            "work from a dropped queued future still started later"
        );
    }
}
