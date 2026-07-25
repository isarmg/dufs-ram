use rustix::fs::{fstat, fstatvfs};
use std::{
    collections::HashMap,
    io,
    os::fd::AsFd,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub(super) struct DiskSpaceTracker {
    state: Arc<Mutex<HashMap<u64, u64>>>,
}

pub(super) struct DiskSpaceReservation {
    tracker: DiskSpaceTracker,
    device: u64,
    remaining: u64,
}

impl DiskSpaceTracker {
    pub(super) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(super) fn reserve<F: AsFd>(
        &self,
        file: &F,
        requested: u64,
        minimum_free: u64,
    ) -> io::Result<Option<DiskSpaceReservation>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let device = filesystem_device(file)?;
        let available = available_space(file)?;
        Ok(self.reserve_locked(&mut state, device, available, requested, minimum_free))
    }

    fn reserve_locked(
        &self,
        state: &mut HashMap<u64, u64>,
        device: u64,
        available: u64,
        requested: u64,
        minimum_free: u64,
    ) -> Option<DiskSpaceReservation> {
        let reserved = state.get(&device).copied().unwrap_or_default();
        if !space_budget_fits(available, reserved, requested, minimum_free) {
            return None;
        }
        if requested > 0 {
            state.insert(
                device,
                reserved
                    .checked_add(requested)
                    .expect("a checked disk-space reservation must not overflow"),
            );
        }
        Some(DiskSpaceReservation {
            tracker: self.clone(),
            device,
            remaining: requested,
        })
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
        if let Some(reserved) = state.get_mut(&device) {
            debug_assert!(bytes <= *reserved);
            *reserved = reserved.saturating_sub(bytes);
            remove_bucket = *reserved == 0;
        } else {
            debug_assert!(false, "disk-space reservation bucket disappeared");
        }
        if remove_bucket {
            state.remove(&device);
        }
    }
}

impl DiskSpaceReservation {
    pub(super) fn remaining(&self) -> u64 {
        self.remaining
    }

    pub(super) fn consume(&mut self, bytes: u64) {
        debug_assert!(bytes <= self.remaining);
        let released = bytes.min(self.remaining);
        self.remaining -= released;
        self.tracker.release(self.device, released);
    }

    pub(super) fn reserved_space_is_available<F: AsFd>(
        &self,
        file: &F,
        minimum_free: u64,
    ) -> io::Result<bool> {
        let state = self
            .tracker
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let device = filesystem_device(file)?;
        if device != self.device {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "disk-space reservation used with a different filesystem",
            ));
        }
        let available = available_space(file)?;
        let reserved = state.get(&device).copied().unwrap_or_default();
        Ok(minimum_free
            .checked_add(reserved)
            .is_some_and(|required| available >= required))
    }
}

impl Drop for DiskSpaceReservation {
    fn drop(&mut self) {
        self.tracker.release(self.device, self.remaining);
        self.remaining = 0;
    }
}

fn filesystem_device<F: AsFd>(file: &F) -> io::Result<u64> {
    Ok(fstat(file).map_err(io::Error::from)?.st_dev)
}

fn available_space<F: AsFd>(file: &F) -> io::Result<u64> {
    let stats = fstatvfs(file).map_err(io::Error::from)?;
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
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
            .values()
            .copied()
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_device_upload_and_zip_share_budget_but_other_devices_are_isolated() {
        let tracker = DiskSpaceTracker::new();
        let upload = tracker
            .reserve_with_snapshot(1, 100, 30, 50)
            .expect("upload reservation should fit");

        assert!(
            tracker.reserve_with_snapshot(1, 100, 21, 50).is_none(),
            "same-device ZIP reservation must include the upload reservation"
        );
        let other_device_zip = tracker
            .reserve_with_snapshot(2, 100, 50, 50)
            .expect("another filesystem must have an independent bucket");

        drop(upload);
        let same_device_zip = tracker
            .reserve_with_snapshot(1, 100, 50, 50)
            .expect("dropping the upload must release its reservation");
        drop(same_device_zip);
        drop(other_device_zip);
        assert!(
            tracker
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[test]
    fn reservation_arithmetic_is_checked_and_consumption_releases_exact_bytes() {
        assert!(space_budget_fits(100, 20, 30, 50));
        assert!(!space_budget_fits(99, 20, 30, 50));
        assert!(!space_budget_fits(u64::MAX, u64::MAX, 1, 0));
        assert!(!space_budget_fits(u64::MAX, 0, 1, u64::MAX));

        let tracker = DiskSpaceTracker::new();
        let mut reservation = tracker
            .reserve_with_snapshot(7, 100, 30, 0)
            .expect("reservation should fit");
        reservation.consume(12);
        assert_eq!(reservation.remaining(), 18);
        assert_eq!(
            tracker
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&7),
            Some(&18)
        );
        drop(reservation);
        assert!(
            tracker
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }
}
