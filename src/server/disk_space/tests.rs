use super::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

#[test]
fn same_device_reservations_share_budget_but_other_devices_are_isolated() {
    let tracker = DiskSpaceTracker::new();
    let upload = tracker
        .reserve_with_snapshot(1, 100, 30, 50)
        .expect("upload reservation should fit");

    assert!(
        tracker.reserve_with_snapshot(1, 100, 21, 50).is_none(),
        "same-device reservation must include the existing reservation"
    );
    let other_device_reservation = tracker
        .reserve_with_snapshot(2, 100, 50, 50)
        .expect("another filesystem must have an independent bucket");

    drop(upload);
    let same_device_reservation = tracker
        .reserve_with_snapshot(1, 100, 50, 50)
        .expect("dropping the upload must release its reservation");
    drop(same_device_reservation);
    drop(other_device_reservation);
    assert!(
        tracker
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reserved
            .is_empty()
    );
}

#[test]
fn reservation_arithmetic_is_checked_and_drop_releases_exact_bytes() {
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
            .reserved
            .get(&7),
        Some(&30),
        "a sub-threshold consume stays conservatively reserved"
    );
    drop(reservation);
    assert!(
        tracker
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reserved
            .is_empty()
    );
}

#[test]
fn small_consumption_is_released_in_batches_without_capacity_overcommit() {
    let tracker = DiskSpaceTracker::new();
    let requested = RESERVATION_RELEASE_BATCH_BYTES * 2 + 17;
    let mut reservation = tracker
        .reserve_with_snapshot(11, requested, requested, 0)
        .expect("reservation should fit");
    let initial_revision = tracker
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .revisions[&11];

    for _ in 0..1024 {
        reservation.consume(512);
    }
    let state = tracker
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(state.reserved[&11], requested);
    assert_eq!(state.revisions[&11], initial_revision);
    drop(state);

    reservation.consume(RESERVATION_RELEASE_BATCH_BYTES - 1024 * 512);
    let state = tracker
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        state.reserved[&11],
        requested - RESERVATION_RELEASE_BATCH_BYTES
    );
    assert_eq!(state.revisions[&11], initial_revision.wrapping_add(1));
    drop(state);

    reservation.consume(17);
    assert!(
        tracker
            .reserve_with_snapshot(11, requested, RESERVATION_RELEASE_BATCH_BYTES + 1, 0,)
            .is_none(),
        "pending releases must remain unavailable to concurrent reservations"
    );
    reservation.flush_pending_release();
    let concurrent = tracker
        .reserve_with_snapshot(11, requested, RESERVATION_RELEASE_BATCH_BYTES + 1, 0)
        .expect("flushed consumed bytes should become reservable");

    drop(concurrent);
    drop(reservation);
    assert_eq!(tracker.total_reserved_for_tests(), 0);
}

#[test]
fn allocation_budget_includes_rounding_and_metadata_without_overflow() {
    assert_eq!(allocation_budget(0, 4096, 4097), Some(8192));
    assert_eq!(allocation_budget(1, 4096, 4097), Some(12_288));
    assert_eq!(allocation_budget(4097, 4096, 4097), Some(16_384));
    assert_eq!(allocation_budget(u64::MAX, 4096, 4097), None);
    assert_eq!(allocation_budget(1, 4096, u64::MAX), None);
}

#[test]
fn available_space_overflow_fails_closed() {
    assert_eq!(available_bytes(7, 4096).unwrap(), 28_672);
    assert_eq!(
        available_bytes(u64::MAX, 2).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn another_device_does_not_lock_or_invalidate_a_slow_filesystem_probe() {
    let tracker = DiskSpaceTracker::new();
    let slow_tracker = tracker.clone();
    let probe_calls = Arc::new(AtomicUsize::new(0));
    let slow_probe_calls = probe_calls.clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();

    let slow = std::thread::spawn(move || {
        slow_tracker
            .reserve_on_device(1, 10, 0, || {
                if slow_probe_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
                Ok(100)
            })
            .unwrap()
    });
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("slow probe did not start");

    let quick_tracker = tracker.clone();
    let (quick_tx, quick_rx) = mpsc::channel();
    let quick = std::thread::spawn(move || {
        let reservation = quick_tracker.reserve_with_snapshot(2, 100, 1, 0);
        quick_tx.send(reservation.is_some()).unwrap();
    });
    assert!(
        quick_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the reservation mutex was held across the slow probe")
    );

    release_tx.send(()).unwrap();
    quick.join().unwrap();
    drop(slow.join().unwrap());
    assert_eq!(
        probe_calls.load(Ordering::SeqCst),
        1,
        "another device must not invalidate this device's capacity snapshot"
    );
}

#[test]
fn same_device_revision_change_retries_a_slow_filesystem_probe() {
    let tracker = DiskSpaceTracker::new();
    let slow_tracker = tracker.clone();
    let probe_calls = Arc::new(AtomicUsize::new(0));
    let slow_probe_calls = probe_calls.clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();

    let slow = std::thread::spawn(move || {
        slow_tracker
            .reserve_on_device(1, 10, 0, || {
                if slow_probe_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
                Ok(100)
            })
            .unwrap()
    });
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("slow probe did not start");

    let quick_tracker = tracker.clone();
    let quick = std::thread::spawn(move || {
        drop(
            quick_tracker
                .reserve_with_snapshot(1, 100, 1, 0)
                .expect("same-device reservation should fit"),
        );
    });
    quick.join().unwrap();
    release_tx.send(()).unwrap();
    drop(slow.join().unwrap());
    assert_eq!(
        probe_calls.load(Ordering::SeqCst),
        2,
        "same-device accounting changes must invalidate a stale capacity snapshot"
    );
}

#[test]
fn capacity_snapshot_churn_fails_closed_after_a_finite_number_of_retries() {
    let tracker = DiskSpaceTracker::new();
    let churning_tracker = tracker.clone();
    let calls = AtomicUsize::new(0);
    let error = tracker
        .reserve_on_device(9, 1, 0, || {
            calls.fetch_add(1, Ordering::SeqCst);
            drop(
                churning_tracker
                    .reserve_with_snapshot(9, 100, 1, 0)
                    .expect("churn reservation should fit"),
            );
            Ok(100)
        })
        .err()
        .expect("permanent revision churn must not spin forever");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(calls.load(Ordering::SeqCst), MAX_CAPACITY_SNAPSHOT_RETRIES);

    let verification_tracker = DiskSpaceTracker::new();
    let verification_churn = verification_tracker.clone();
    let calls = AtomicUsize::new(0);
    let error = verification_tracker
        .reserved_space_is_available_on_device(10, 0, || {
            calls.fetch_add(1, Ordering::SeqCst);
            drop(
                verification_churn
                    .reserve_with_snapshot(10, 100, 1, 0)
                    .expect("churn reservation should fit"),
            );
            Ok(100)
        })
        .expect_err("permanent verification churn must not spin forever");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(calls.load(Ordering::SeqCst), MAX_CAPACITY_SNAPSHOT_RETRIES);
}
