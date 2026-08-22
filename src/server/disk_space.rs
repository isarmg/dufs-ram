use super::blocking_io::blocking_io_gate;
use rustix::{
    fs::{fstat, fstatvfs},
    io::dup,
};
use std::{
    collections::HashMap,
    fs::File,
    io,
    os::fd::AsFd,
    sync::{Arc, Mutex},
};
use tokio::task::JoinHandle;

const MAX_CAPACITY_SNAPSHOT_RETRIES: usize = 8;
// Streaming writers commonly consume reservations in small chunks. Keep
// those logical updates local until they are large enough to justify taking
// the process-wide accounting mutex. Holding consumed bytes a little longer
// is conservative: it can temporarily reject another reservation, but can
// never expose protected capacity that has already been written.
const RESERVATION_RELEASE_BATCH_BYTES: u64 = 1024 * 1024;

#[derive(Clone)]
pub(super) struct DiskSpaceTracker {
    state: Arc<Mutex<DiskSpaceState>>,
}

#[derive(Default)]
struct DiskSpaceState {
    reserved: HashMap<u64, u64>,
    revisions: HashMap<u64, u64>,
}

pub(super) struct DiskSpaceReservation {
    tracker: DiskSpaceTracker,
    device: u64,
    remaining: u64,
    pending_release: u64,
}

impl DiskSpaceTracker {
    pub(super) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(DiskSpaceState::default())),
        }
    }

    pub(super) fn reserve<F: AsFd>(
        &self,
        file: &F,
        requested: u64,
        minimum_free: u64,
    ) -> io::Result<Option<DiskSpaceReservation>> {
        let device = filesystem_device(file)?;
        self.reserve_on_device(device, requested, minimum_free, || available_space(file))
    }

    pub(super) fn spawn_reservation<F: AsFd>(
        &self,
        file: &F,
        requested: u64,
        minimum_free: u64,
    ) -> io::Result<JoinHandle<io::Result<Option<DiskSpaceReservation>>>> {
        let file = File::from(dup(file).map_err(io::Error::from)?);
        let tracker = self.clone();
        Ok(blocking_io_gate().spawn_io(move || tracker.reserve(&file, requested, minimum_free)))
    }

    pub(super) async fn reserve_async<F: AsFd>(
        &self,
        file: &F,
        requested: u64,
        minimum_free: u64,
    ) -> io::Result<Option<DiskSpaceReservation>> {
        self.spawn_reservation(file, requested, minimum_free)?
            .await
            .map_err(io::Error::other)?
    }

    pub(super) fn spawn_allocated_reservation<F: AsFd>(
        &self,
        file: &F,
        logical_bytes: u64,
        metadata_bytes: u64,
        minimum_free: u64,
    ) -> io::Result<JoinHandle<io::Result<Option<DiskSpaceReservation>>>> {
        let file = File::from(dup(file).map_err(io::Error::from)?);
        let tracker = self.clone();
        Ok(blocking_io_gate().spawn_io(move || {
            let requested = allocated_reservation_bytes(&file, logical_bytes, metadata_bytes)?;
            tracker.reserve(&file, requested, minimum_free)
        }))
    }

    pub(super) async fn reserve_allocated_async<F: AsFd>(
        &self,
        file: &F,
        logical_bytes: u64,
        metadata_bytes: u64,
        minimum_free: u64,
    ) -> io::Result<Option<DiskSpaceReservation>> {
        self.spawn_allocated_reservation(file, logical_bytes, metadata_bytes, minimum_free)?
            .await
            .map_err(io::Error::other)?
    }

    fn reserve_locked(
        &self,
        state: &mut DiskSpaceState,
        device: u64,
        available: u64,
        requested: u64,
        minimum_free: u64,
    ) -> Option<DiskSpaceReservation> {
        let reserved = state.reserved.get(&device).copied().unwrap_or_default();
        if !space_budget_fits(available, reserved, requested, minimum_free) {
            return None;
        }
        if requested > 0 {
            state.reserved.insert(
                device,
                reserved
                    .checked_add(requested)
                    .expect("a checked disk-space reservation must not overflow"),
            );
            bump_revision(state, device);
        }
        Some(DiskSpaceReservation {
            tracker: self.clone(),
            device,
            remaining: requested,
            pending_release: 0,
        })
    }

    fn reserve_on_device<P>(
        &self,
        device: u64,
        requested: u64,
        minimum_free: u64,
        mut probe_available: P,
    ) -> io::Result<Option<DiskSpaceReservation>>
    where
        P: FnMut() -> io::Result<u64>,
    {
        for _ in 0..MAX_CAPACITY_SNAPSHOT_RETRIES {
            let observed_revision = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .revisions
                .get(&device)
                .copied()
                .unwrap_or_default();
            // Filesystem probes may block indefinitely on abnormal FUSE or
            // network mounts. Never hold the process-wide reservation mutex
            // while issuing them.
            let available = probe_available()?;
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.revisions.get(&device).copied().unwrap_or_default() != observed_revision {
                continue;
            }
            return Ok(self.reserve_locked(&mut state, device, available, requested, minimum_free));
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "disk-space reservations changed too frequently to obtain a stable capacity snapshot",
        ))
    }

    #[cfg(test)]
    fn reserve_with_snapshot(
        &self,
        device: u64,
        available: u64,
        requested: u64,
        minimum_free: u64,
    ) -> Option<DiskSpaceReservation> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.reserve_locked(&mut state, device, available, requested, minimum_free)
    }

    fn release(&self, device: u64, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut remove_bucket = false;
        if let Some(reserved) = state.reserved.get_mut(&device) {
            debug_assert!(bytes <= *reserved);
            *reserved = reserved.saturating_sub(bytes);
            remove_bucket = *reserved == 0;
        } else {
            debug_assert!(false, "disk-space reservation bucket disappeared");
        }
        if remove_bucket {
            state.reserved.remove(&device);
        }
        bump_revision(&mut state, device);
    }

    fn reserved_space_is_available_on_device<P>(
        &self,
        device: u64,
        minimum_free: u64,
        mut probe_available: P,
    ) -> io::Result<bool>
    where
        P: FnMut() -> io::Result<u64>,
    {
        for _ in 0..MAX_CAPACITY_SNAPSHOT_RETRIES {
            let observed_revision = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .revisions
                .get(&device)
                .copied()
                .unwrap_or_default();
            let available = probe_available()?;
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.revisions.get(&device).copied().unwrap_or_default() != observed_revision {
                continue;
            }
            let reserved = state.reserved.get(&device).copied().unwrap_or_default();
            return Ok(minimum_free
                .checked_add(reserved)
                .is_some_and(|required| available >= required));
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "disk-space reservations changed too frequently to verify the protected free space",
        ))
    }
}

fn bump_revision(state: &mut DiskSpaceState, device: u64) {
    let revision = state.revisions.entry(device).or_default();
    *revision = revision.wrapping_add(1);
}

impl DiskSpaceReservation {
    pub(super) fn remaining(&self) -> u64 {
        self.remaining
    }

    pub(super) fn consume(&mut self, bytes: u64) {
        debug_assert!(bytes <= self.remaining);
        let consumed = bytes.min(self.remaining);
        self.remaining -= consumed;
        self.pending_release = self
            .pending_release
            .checked_add(consumed)
            .expect("consumed disk-space reservation bytes must not overflow");
        if self.pending_release >= RESERVATION_RELEASE_BATCH_BYTES {
            self.flush_pending_release();
        }
    }

    pub(super) async fn reserved_space_is_available_async<F: AsFd>(
        &mut self,
        file: &F,
        minimum_free: u64,
    ) -> io::Result<bool> {
        // The filesystem probe sees writes that have already reached the
        // filesystem, so first make the in-memory reservation view match the
        // logically consumed bytes. Otherwise this safety check could reject
        // a healthy upload solely because a conservative local batch remains.
        self.flush_pending_release();
        let file = File::from(dup(file).map_err(io::Error::from)?);
        let tracker = self.tracker.clone();
        let device = self.device;
        blocking_io_gate()
            .run_io(move || {
                let actual_device = filesystem_device(&file)?;
                if actual_device != device {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "disk-space reservation used with a different filesystem",
                    ));
                }
                tracker.reserved_space_is_available_on_device(device, minimum_free, || {
                    available_space(&file)
                })
            })
            .await
    }

    fn flush_pending_release(&mut self) {
        let released = std::mem::take(&mut self.pending_release);
        self.tracker.release(self.device, released);
    }
}

impl Drop for DiskSpaceReservation {
    fn drop(&mut self) {
        let tracked = self
            .remaining
            .checked_add(self.pending_release)
            .expect("tracked disk-space reservation bytes must not overflow");
        self.tracker.release(self.device, tracked);
        self.remaining = 0;
        self.pending_release = 0;
    }
}

fn filesystem_device<F: AsFd>(file: &F) -> io::Result<u64> {
    Ok(fstat(file).map_err(io::Error::from)?.st_dev)
}

fn available_space<F: AsFd>(file: &F) -> io::Result<u64> {
    let stats = fstatvfs(file).map_err(io::Error::from)?;
    available_bytes(stats.f_bavail, stats.f_frsize)
}

fn available_bytes(available_blocks: u64, fragment_size: u64) -> io::Result<u64> {
    available_blocks.checked_mul(fragment_size).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "filesystem available-space report exceeds u64",
        )
    })
}

fn allocated_reservation_bytes<F: AsFd>(
    file: &F,
    logical_bytes: u64,
    metadata_bytes: u64,
) -> io::Result<u64> {
    let stats = fstatvfs(file).map_err(io::Error::from)?;
    allocation_budget(logical_bytes, stats.f_frsize.max(1), metadata_bytes).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "disk-space reservation including allocation overhead exceeds u64",
        )
    })
}

fn allocation_budget(logical_bytes: u64, allocation_unit: u64, metadata_bytes: u64) -> Option<u64> {
    fn round_up(value: u64, unit: u64) -> Option<u64> {
        if value == 0 {
            return Some(0);
        }
        Some(value.checked_add(unit.checked_sub(1)?)? / unit * unit)
    }

    round_up(logical_bytes, allocation_unit)?
        .checked_add(round_up(metadata_bytes, allocation_unit)?)
}

fn space_budget_fits(
    available: u64,
    already_reserved: u64,
    requested: u64,
    minimum_free: u64,
) -> bool {
    minimum_free
        .checked_add(already_reserved)
        .and_then(|required| required.checked_add(requested))
        .is_some_and(|required| available >= required)
}

#[cfg(test)]
impl DiskSpaceTracker {
    pub(super) fn total_reserved_for_tests(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reserved
            .values()
            .copied()
            .sum()
    }
}

#[cfg(test)]
mod tests;
