use super::*;
use crate::{Args, server::ServerLifecycle};
use http_body_util::BodyExt as _;
use std::{
    collections::HashMap,
    ffi::OsString,
    os::unix::{ffi::OsStringExt, fs::PermissionsExt},
    sync::{
        Arc, Condvar, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
        mpsc,
    },
    time::Duration,
};
use tokio_util::task::TaskTracker;

const LIST_API_TEST_ACCOUNT: &str = "listing-test:$argon2id$v=19$m=19456,t=2,p=1$HdPI2G8k0h+yEgnqIt2rSw$P+MRyz7wH+b/iPY+He/9DApcy6yB9TAoo7j2JG1Smzs";

fn list_api_test_server(root: &assert_fs::TempDir) -> (Arc<Server>, assert_fs::TempDir) {
    let state_dir = assert_fs::TempDir::new().expect("create listing state directory");
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("restrict listing state directory");
    let mut args = Args {
        serve_path: root.path().to_path_buf(),
        state_dir: Some(state_dir.path().to_path_buf()),
        auth: crate::auth::AuthConfig::new(&[LIST_API_TEST_ACCOUNT])
            .expect("parse listing test account"),
        ..Args::default()
    };
    args.max_concurrent_searches = 1;
    let server = Server::init_with_list_snapshot_cache(
        args,
        ServerLifecycle::new(),
        ListSnapshotCache::isolated(),
    )
    .expect("initialize listing test server");
    (Arc::new(server), state_dir)
}

fn list_query(parameters: &[(&str, &str)]) -> HashMap<String, String> {
    parameters
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect()
}

async fn list_api_response(server: Arc<Server>, query: HashMap<String, String>) -> Response {
    let mut response = Response::default();
    server
        .handle_list_api("listing-test", &query, &mut response)
        .await
        .expect("handle list API request");
    response
}

async fn list_response_json(response: Response) -> serde_json::Value {
    let body = response
        .into_body()
        .collect()
        .await
        .expect("collect list API response")
        .to_bytes();
    serde_json::from_slice(&body).expect("decode list API response")
}

fn set_list_metadata_phase_hook(
    server: &Server,
    hook: Option<Arc<dyn Fn(ListMetadataPhase) + Send + Sync>>,
) {
    *server
        .admission
        .list_metadata_phase_hook
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
}

async fn wait_for_search_slot(server: &Server) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if server.admission.search_slots.available_permits() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("listing metadata worker did not release search admission");
}

#[tokio::test]
async fn first_page_admission_precedes_metadata_but_follows_parameter_validation() {
    let root = assert_fs::TempDir::new().expect("create listing root");
    let (server, _state_dir) = list_api_test_server(&root);
    let metadata_calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = metadata_calls.clone();
    set_list_metadata_phase_hook(
        &server,
        Some(Arc::new(move |_| {
            observed_calls.fetch_add(1, AtomicOrdering::SeqCst);
        })),
    );
    let held = server
        .admission
        .search_slots
        .clone()
        .try_acquire_owned()
        .expect("hold the only listing admission slot");

    let saturated = list_api_response(server.clone(), list_query(&[])).await;
    assert_eq!(saturated.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(saturated.headers()["retry-after"], "1");
    assert_eq!(
        list_response_json(saturated).await["code"],
        "directory_operation_limit"
    );
    assert_eq!(metadata_calls.load(AtomicOrdering::SeqCst), 0);

    let long_query = "x".repeat(129);
    for (invalid, expected_code) in [
        (list_query(&[("limit", "0")]), "invalid_list_limit"),
        (
            list_query(&[("cursor", "not-a-cursor")]),
            "invalid_list_cursor",
        ),
        (list_query(&[("q", &long_query)]), "search_query_too_long"),
    ] {
        let response = list_api_response(server.clone(), invalid).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(list_response_json(response).await["code"], expected_code);
    }
    assert_eq!(
        metadata_calls.load(AtomicOrdering::SeqCst),
        0,
        "saturated or invalid first-page requests reached metadata"
    );
    drop(held);
}

#[tokio::test]
async fn cursor_page_bypasses_saturated_search_admission_and_tracked_metadata_hook() {
    let root = assert_fs::TempDir::new().expect("create listing root");
    std::fs::write(root.path().join("one.txt"), "one").expect("create first listing entry");
    std::fs::write(root.path().join("two.txt"), "two").expect("create second listing entry");
    let (server, _state_dir) = list_api_test_server(&root);

    let first = list_api_response(server.clone(), list_query(&[("limit", "1")])).await;
    assert_eq!(first.status(), StatusCode::OK);
    let first = list_response_json(first).await;
    let cursor = first["next_cursor"]
        .as_str()
        .expect("first page has a continuation cursor")
        .to_string();

    let tracked_metadata_calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = tracked_metadata_calls.clone();
    set_list_metadata_phase_hook(
        &server,
        Some(Arc::new(move |_| {
            observed_calls.fetch_add(1, AtomicOrdering::SeqCst);
        })),
    );
    let held = server
        .admission
        .search_slots
        .clone()
        .try_acquire_owned()
        .expect("hold the only listing admission slot");

    let continuation = list_api_response(
        server.clone(),
        list_query(&[("limit", "1"), ("cursor", &cursor)]),
    )
    .await;
    assert_eq!(continuation.status(), StatusCode::OK);
    assert_eq!(
        tracked_metadata_calls.load(AtomicOrdering::SeqCst),
        0,
        "cursor lookup used first-page guarded metadata"
    );
    drop(held);
}

#[tokio::test]
async fn first_page_metadata_phases_hold_search_admission() {
    let root = assert_fs::TempDir::new().expect("create listing root");
    std::fs::write(root.path().join("entry.txt"), "entry").expect("create listing entry");
    let (server, _state_dir) = list_api_test_server(&root);
    let observations = Arc::new(StdMutex::new(Vec::new()));
    let observed = observations.clone();
    let search_slots = server.admission.search_slots.clone();
    set_list_metadata_phase_hook(
        &server,
        Some(Arc::new(move |phase| {
            observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((phase, search_slots.available_permits()));
        })),
    );

    let response = list_api_response(server.clone(), list_query(&[])).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        *observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        [
            (ListMetadataPhase::BeforeWalk, 0),
            (ListMetadataPhase::AfterWalk, 0),
        ]
    );
    assert_eq!(server.admission.search_slots.available_permits(), 1);
}

async fn assert_aborted_metadata_retains_search_admission(target: ListMetadataPhase) {
    let root = assert_fs::TempDir::new().expect("create listing root");
    std::fs::write(root.path().join("entry.txt"), "entry").expect("create listing entry");
    let (server, _state_dir) = list_api_test_server(&root);
    let observations = Arc::new(StdMutex::new(Vec::new()));
    let observed = observations.clone();
    let (entered_sender, entered_receiver) = mpsc::channel();
    let entered_sender = StdMutex::new(Some(entered_sender));
    let (release_sender, release_receiver) = mpsc::channel();
    let release_receiver = StdMutex::new(Some(release_receiver));
    set_list_metadata_phase_hook(
        &server,
        Some(Arc::new(move |phase| {
            observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(phase);
            if phase == target {
                entered_sender
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                    .expect("target metadata phase ran once")
                    .send(())
                    .expect("report entered metadata phase");
                release_receiver
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                    .expect("target metadata phase waited once")
                    .recv()
                    .expect("release metadata phase");
            }
        })),
    );

    let request = tokio::spawn(list_api_response(server.clone(), list_query(&[])));
    tokio::task::spawn_blocking(move || {
        entered_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("target metadata phase did not start")
    })
    .await
    .expect("metadata-phase observer panicked");
    request.abort();
    assert!(
        request
            .await
            .expect_err("aborted request returned")
            .is_cancelled()
    );

    assert_eq!(
        server.admission.search_slots.available_permits(),
        0,
        "request cancellation released admission before blocking metadata exited"
    );
    let saturated = list_api_response(server.clone(), list_query(&[])).await;
    assert_eq!(saturated.status(), StatusCode::TOO_MANY_REQUESTS);

    release_sender
        .send(())
        .expect("release blocked metadata phase");
    wait_for_search_slot(&server).await;
    let observed_phases = observations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let expected = match target {
        ListMetadataPhase::BeforeWalk => vec![ListMetadataPhase::BeforeWalk],
        ListMetadataPhase::AfterWalk => {
            vec![ListMetadataPhase::BeforeWalk, ListMetadataPhase::AfterWalk]
        }
    };
    assert_eq!(
        observed_phases, expected,
        "an aborted request started a later metadata phase"
    );

    set_list_metadata_phase_hook(&server, None);
    let recovered = list_api_response(server.clone(), list_query(&[])).await;
    assert_eq!(recovered.status(), StatusCode::OK);
}

#[tokio::test]
async fn aborted_first_and_final_metadata_retain_search_admission_until_worker_exit() {
    assert_aborted_metadata_retains_search_admission(ListMetadataPhase::BeforeWalk).await;
    assert_aborted_metadata_retains_search_admission(ListMetadataPhase::AfterWalk).await;
}

#[tokio::test]
async fn listing_problem_protocol_ignores_diagnostic_reason_strings() {
    let root = Path::new("/srv/share");
    let error = ListingError::limit(
        "typed_protocol_test",
        root,
        root,
        // Diagnostic details must never override the typed public problem.
        "entry_budget",
        ListingProblem::DirectoryOperationTimeout,
    );
    let mut response = Response::default();

    respond_list_api_listing_error(&mut response, &error).unwrap();

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["code"], "directory_operation_timeout");
    assert_eq!(problem["detail"], "Directory operation timed out");
    assert_eq!(problem["recovery"], "retry");
}

#[tokio::test]
async fn list_snapshot_allocation_failure_uses_the_listing_error_code() {
    let root = Path::new("/srv/share");
    let error = ListingError::limit(
        "list_snapshot",
        root,
        root,
        "result_allocation_failed",
        ListingProblem::ListSnapshotAllocationFailed,
    );
    let mut response = Response::default();

    respond_list_api_listing_error(&mut response, &error).unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["code"], "list_snapshot_limit");
    assert_eq!(
        problem["detail"],
        "Directory listing exceeds the snapshot capacity"
    );
}

#[test]
fn bounded_vector_growth_accounts_for_old_and_new_allocations() {
    let mut values = Vec::with_capacity(4);
    values.extend([1_u64, 2, 3, 4]);
    let non_vector_bytes = std::mem::size_of::<Vec<u64>>();
    let old_bytes = values.capacity() * std::mem::size_of::<u64>();
    let one_slot_growth = 5 * std::mem::size_of::<u64>();
    let maximum_bytes = non_vector_bytes + old_bytes + one_slot_growth;

    try_reserve_bounded_vec_slot(&mut values, non_vector_bytes, maximum_bytes)
        .expect("one bounded slot remains");
    assert_eq!(values.capacity(), 5);
    values.push(5);
    assert_eq!(
        try_reserve_bounded_vec_slot(&mut values, non_vector_bytes, maximum_bytes),
        Err(BoundedReserveError::Budget),
        "the next realloc would transiently retain both old and new buffers"
    );
}

#[test]
fn bounded_vector_growth_remains_geometric_with_available_budget() {
    let mut values = Vec::with_capacity(4);
    values.extend([1_u64, 2, 3, 4]);
    let non_vector_bytes = std::mem::size_of::<Vec<u64>>();
    let maximum_bytes = non_vector_bytes
        + values.capacity() * std::mem::size_of::<u64>()
        + 8 * std::mem::size_of::<u64>();

    try_reserve_bounded_vec_slot(&mut values, non_vector_bytes, maximum_bytes)
        .expect("geometric growth fits the transient budget");
    assert_eq!(values.capacity(), 8);
}

#[test]
fn non_utf8_and_control_bytes_are_safely_escaped_in_errors() {
    let root = Path::new("/srv/share");
    let name = OsString::from_vec(b"line\nquote\"slash\\bad\xff".to_vec());
    let path = root.join(name);
    let error = ListingError::unsupported_name("list_filename", &path, root);
    let rendered = error.to_string();

    assert_eq!(error.relative_path, "line\\x0aquote\\\"slash\\\\bad\\xff");
    assert!(!rendered.contains('\n'));
    assert!(!rendered.contains("/srv/share"));
    assert_eq!(error.problem.status(), StatusCode::CONFLICT);
    assert_eq!(error.problem.public_message(), UNSUPPORTED_FILENAME_MESSAGE);
}

#[test]
fn disappearing_walk_entries_are_reported_as_retryable_conflicts() {
    let root = Path::new("/srv/share");
    for kind in [
        std::io::ErrorKind::NotFound,
        std::io::ErrorKind::NotADirectory,
    ] {
        let io_error = std::io::Error::new(kind, "entry changed concurrently");
        let error = ListingError::io("walk_next", root, root, &io_error);

        assert_eq!(error.problem.status(), StatusCode::CONFLICT);
        assert_eq!(
            error.problem.public_message(),
            DIRECTORY_CHANGED_DURING_WALK_MESSAGE
        );
        assert!(error.reason.contains(&format!("io_kind={kind:?}")));
    }
}

#[test]
fn post_walk_snapshot_disappearance_or_type_change_is_a_retryable_conflict() {
    let root = assert_fs::TempDir::new().expect("create listing root");
    let file = root.path().join("replacement.txt");
    std::fs::write(&file, "content").expect("create non-directory replacement");

    for error in [
        directory_snapshot_after_walk(
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "directory disappeared",
            )),
            root.path(),
            root.path(),
        )
        .expect_err("disappeared directory must conflict"),
        directory_snapshot_after_walk(
            Ok(std::fs::metadata(&file).expect("read replacement metadata")),
            root.path(),
            root.path(),
        )
        .expect_err("non-directory replacement must conflict"),
    ] {
        assert_eq!(error.problem.status(), StatusCode::CONFLICT);
        assert_eq!(
            error.problem.public_message(),
            DIRECTORY_CHANGED_DURING_WALK_MESSAGE
        );
    }
}

#[test]
fn list_snapshot_cursor_is_authenticated_and_expires() {
    let now = Instant::now();
    let id = [7u8; 32];
    let secret = [11u8; 32];
    let directory = DirectorySnapshot {
        device: 1,
        inode: 2,
        mtime: 3,
        mtime_nanoseconds: 4,
        ctime: 5,
        ctime_nanoseconds: 6,
    };
    let path = PathBuf::from("/srv/share");
    let owner = list_snapshot_owner("alice");
    let binding = ListSnapshotBinding {
        owner,
        path: path.clone(),
        directory,
        sort: "name".to_string(),
        order: "asc".to_string(),
        query: "match".to_string(),
        limit: 1,
    };
    let item = |name: &str| PathItem {
        path_type: PathType::File,
        sort_name: name.to_string(),
        name: name.to_string(),
        mtime: 0,
        size: 0,
        revision: TargetRevision::from_bytes([0; 32]),
    };
    let paths = vec![item("a"), item("b"), item("c")];
    let weight = list_snapshot_weight(&binding, &paths, paths.capacity());
    let mut store = ListSnapshotStore::default();
    store.insertion_order.push_back(id);
    store.total_weight = weight;
    store.snapshots.insert(
        id,
        ListSnapshotRecord {
            secret,
            binding,
            paths: paths.into(),
            expires_at: now + LIST_SNAPSHOT_TTL,
            weight,
        },
    );

    let cursor =
        decode_list_cursor(&encode_list_cursor(id, &secret, 1)).expect("decode valid cursor");
    let request = |owner| ListSnapshotRequest {
        owner,
        path: &path,
        directory,
        sort: "name",
        order: "asc",
        query: "match",
        limit: 1,
    };
    let page = store
        .page(&cursor, request(owner), now)
        .expect("read cached page");
    assert_eq!(
        page.paths()
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        ["b"]
    );

    let mut tampered = cursor;
    tampered.offset = 2;
    assert!(matches!(
        store.page(&tampered, request(owner), now),
        Err(ListSnapshotLookupError::Unavailable)
    ));
    assert!(matches!(
        store.page(&cursor, request(list_snapshot_owner("bob")), now),
        Err(ListSnapshotLookupError::InvalidBinding)
    ));
    assert!(matches!(
        store.page(&cursor, request(owner), now + LIST_SNAPSHOT_TTL),
        Err(ListSnapshotLookupError::Unavailable)
    ));
    assert!(store.snapshots.is_empty());
    assert_eq!(store.total_weight, 0);
}

#[test]
fn list_snapshot_store_has_count_and_memory_caps() {
    let now = Instant::now();
    let record = |owner, weight| ListSnapshotRecord {
        secret: [0u8; 32],
        binding: ListSnapshotBinding {
            owner,
            path: PathBuf::from("/srv/share"),
            directory: DirectorySnapshot {
                device: 1,
                inode: 2,
                mtime: 3,
                mtime_nanoseconds: 4,
                ctime: 5,
                ctime_nanoseconds: 6,
            },
            sort: "name".to_string(),
            order: "asc".to_string(),
            query: String::new(),
            limit: 1,
        },
        paths: Vec::new().into(),
        expires_at: now + LIST_SNAPSHOT_TTL,
        weight,
    };

    let mut count_bounded = ListSnapshotStore::default();
    for value in 0..MAX_CACHED_LIST_SNAPSHOTS {
        let mut id = [0u8; 32];
        id[0] = u8::try_from(value).expect("test snapshot count fits in u8");
        let mut owner = [0u8; 32];
        owner[0] = u8::try_from(value).expect("test owner count fits in u8");
        count_bounded.insertion_order.push_back(id);
        count_bounded.snapshots.insert(id, record(owner, 1));
        count_bounded.total_weight += 1;
    }
    count_bounded.make_room(&[0xff; 32], 1);
    assert_eq!(count_bounded.snapshots.len(), MAX_CACHED_LIST_SNAPSHOTS - 1);
    assert!(!count_bounded.snapshots.contains_key(&[0u8; 32]));

    let id = [1u8; 32];
    let mut memory_bounded = ListSnapshotStore::default();
    memory_bounded.insertion_order.push_back(id);
    memory_bounded
        .snapshots
        .insert(id, record([1u8; 32], MAX_CACHED_LIST_SNAPSHOT_BYTES));
    memory_bounded.total_weight = MAX_CACHED_LIST_SNAPSHOT_BYTES;
    memory_bounded.make_room(&[2u8; 32], 1);
    assert!(memory_bounded.snapshots.is_empty());
    assert_eq!(memory_bounded.total_weight, 0);

    let protected_owner = [3u8; 32];
    let busy_owner = [4u8; 32];
    let mut fair = ListSnapshotStore::default();
    let protected_id = [0x80; 32];
    fair.insertion_order.push_back(protected_id);
    fair.snapshots
        .insert(protected_id, record(protected_owner, 1));
    fair.total_weight = 1;
    for value in 0..MAX_CACHED_LIST_SNAPSHOTS_PER_OWNER {
        let mut id = [0u8; 32];
        id[0] = u8::try_from(value + 1).expect("test snapshot count fits in u8");
        fair.insertion_order.push_back(id);
        fair.snapshots.insert(id, record(busy_owner, 1));
        fair.total_weight += 1;
    }
    fair.make_room(&busy_owner, 1);
    assert!(fair.snapshots.contains_key(&protected_id));
    assert_eq!(
        fair.owner_usage(&busy_owner).0,
        MAX_CACHED_LIST_SNAPSHOTS_PER_OWNER - 1
    );
}

#[test]
fn isolated_list_snapshot_caches_do_not_share_entries_or_capacity() {
    let now = Instant::now();
    let path = PathBuf::from("/srv/share");
    let directory = DirectorySnapshot {
        device: 1,
        inode: 2,
        mtime: 3,
        mtime_nanoseconds: 4,
        ctime: 5,
        ctime_nanoseconds: 6,
    };
    let item = |name: &str| PathItem {
        path_type: PathType::File,
        sort_name: name.to_string(),
        name: name.to_string(),
        mtime: 0,
        size: 0,
        revision: TargetRevision::from_bytes([0; 32]),
    };
    let binding = |owner: [u8; 32]| ListSnapshotBinding {
        owner,
        path: path.clone(),
        directory,
        sort: "name".to_string(),
        order: "asc".to_string(),
        query: String::new(),
        limit: 1,
    };
    let cache_snapshot = |cache: &ListSnapshotCache, owner: [u8; 32]| {
        cache
            .cache(binding(owner), vec![item("a"), item("b")], &path, now)
            .expect("cache listing snapshot")
    };

    let first = ListSnapshotCache::isolated();
    let second = ListSnapshotCache::isolated();
    let owner = [0u8; 32];
    let first_page = cache_snapshot(&first, owner);
    let cursor = decode_list_cursor(
        first_page
            .next_cursor
            .as_deref()
            .expect("multi-page snapshot has a cursor"),
    )
    .expect("decode generated cursor");
    let request = ListSnapshotRequest {
        owner,
        path: &path,
        directory,
        sort: "name",
        order: "asc",
        query: "",
        limit: 1,
    };
    assert!(matches!(
        second.page(&cursor, request, now),
        Err(ListSnapshotLookupError::Unavailable)
    ));

    for value in 1..=MAX_CACHED_LIST_SNAPSHOTS {
        let owner = [u8::try_from(value).expect("cache count fits in u8"); 32];
        cache_snapshot(&first, owner);
    }
    assert_eq!(first.snapshot_count(), MAX_CACHED_LIST_SNAPSHOTS);
    assert_eq!(second.snapshot_count(), 0);

    for value in 0..=MAX_CACHED_LIST_SNAPSHOTS {
        let owner = [u8::try_from(value + MAX_CACHED_LIST_SNAPSHOTS + 1)
            .expect("cache count fits in u8"); 32];
        cache_snapshot(&second, owner);
    }
    assert_eq!(first.snapshot_count(), MAX_CACHED_LIST_SNAPSHOTS);
    assert_eq!(second.snapshot_count(), MAX_CACHED_LIST_SNAPSHOTS);
}

#[test]
fn direct_list_snapshot_scan_enforces_entry_and_time_budgets() {
    let root = assert_fs::TempDir::new().expect("create listing root");
    std::fs::write(root.path().join("one.txt"), "one").expect("create first entry");
    std::fs::write(root.path().join("two.txt"), "two").expect("create second entry");
    let rooted_fs = RootedFs::new(root.path()).expect("open rooted filesystem");
    let running = AtomicBool::new(true);

    let entry_error = collect_list_snapshot_blocking(
        &rooted_fs,
        root.path(),
        ListSnapshotOptions {
            serve_path: root.path(),
            revision_owner: OwnerId::persistent("alice"),
            max_entries: 1,
            max_bytes: MAX_CACHED_LIST_SNAPSHOT_BYTES_PER_OWNER,
            sort: "name",
            order: "asc",
            running: &running,
            cancellation: &CancellationToken::new(),
            max_duration: Duration::from_secs(5),
        },
    )
    .expect_err("the direct listing entry budget must stop the scan");
    assert_eq!(entry_error.problem.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        entry_error.problem.public_message(),
        "Directory listing exceeded its entry limit"
    );

    let time_error = collect_list_snapshot_blocking(
        &rooted_fs,
        root.path(),
        ListSnapshotOptions {
            serve_path: root.path(),
            revision_owner: OwnerId::persistent("alice"),
            max_entries: 8,
            max_bytes: MAX_CACHED_LIST_SNAPSHOT_BYTES_PER_OWNER,
            sort: "name",
            order: "asc",
            running: &running,
            cancellation: &CancellationToken::new(),
            max_duration: Duration::ZERO,
        },
    )
    .expect_err("the direct listing time budget must stop the scan");
    assert_eq!(time_error.problem.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        time_error.problem.public_message(),
        "Directory operation timed out"
    );
}

#[test]
fn stable_sort_can_be_interrupted_during_merge_work() {
    #[derive(Debug, Eq, PartialEq)]
    struct Tagged {
        key: usize,
        original_position: usize,
    }

    let mut stable = vec![
        Tagged {
            key: 2,
            original_position: 0,
        },
        Tagged {
            key: 1,
            original_position: 1,
        },
        Tagged {
            key: 2,
            original_position: 2,
        },
        Tagged {
            key: 1,
            original_position: 3,
        },
    ];
    stable_sort_by_interruptible(&mut stable, |left, right| left.key.cmp(&right.key), || true)
        .expect("active stable sort");
    assert_eq!(
        stable,
        [
            Tagged {
                key: 1,
                original_position: 1,
            },
            Tagged {
                key: 1,
                original_position: 3,
            },
            Tagged {
                key: 2,
                original_position: 0,
            },
            Tagged {
                key: 2,
                original_position: 2,
            },
        ]
    );

    let mut interrupted: Vec<_> = (0..4_096usize).rev().collect();
    let checks = std::cell::Cell::new(0usize);
    let stop_after = 4;
    let result = stable_sort_by_interruptible(&mut interrupted, Ord::cmp, || {
        let next = checks.get() + 1;
        checks.set(next);
        next < stop_after
    });
    assert_eq!(result, Err(InterruptibleSortFailure::Interrupted));
    assert_eq!(checks.get(), stop_after);
}

#[test]
fn search_case_folding_avoids_ascii_allocations_without_changing_unicode_matching() {
    assert!(case_folded_contains("Quarterly-REPORT.txt", "report"));
    assert!(!case_folded_contains("Quarterly-REPORT.txt", "invoice"));
    assert!(case_folded_contains("CAFÉ.txt", "café"));
}

#[test]
fn path_sort_maps_cancellation_and_deadline_to_existing_statuses() {
    let item = |name: &str| PathItem {
        path_type: PathType::File,
        sort_name: name.to_owned(),
        name: name.to_owned(),
        mtime: 0,
        size: 0,
        revision: TargetRevision::from_bytes([0; 32]),
    };
    let path = Path::new("/srv/share");

    let mut cancelled_items = vec![item("b"), item("a")];
    let cancelled = AtomicBool::new(false);
    let error = sort_path_items_interruptibly(
        &mut cancelled_items,
        "name",
        "asc",
        &cancelled,
        &CancellationToken::new(),
        Instant::now(),
        Duration::from_secs(5),
        "search_sort",
        path,
        path,
    )
    .expect_err("cancelled sorting must stop");
    assert_eq!(error.problem.status(), StatusCode::SERVICE_UNAVAILABLE);

    let mut expired_items = vec![item("b"), item("a")];
    let running = AtomicBool::new(true);
    let error = sort_path_items_interruptibly(
        &mut expired_items,
        "name",
        "asc",
        &running,
        &CancellationToken::new(),
        Instant::now(),
        Duration::ZERO,
        "search_sort",
        path,
        path,
    )
    .expect_err("expired sorting must stop");
    assert_eq!(error.problem.status(), StatusCode::GATEWAY_TIMEOUT);
}

#[test]
fn direct_list_snapshot_rejects_long_names_before_inserting_over_budget_items() {
    let root = assert_fs::TempDir::new().expect("create listing root");
    let name = format!("{}.txt", "x".repeat(200));
    std::fs::write(root.path().join(name), "content").expect("create long-name entry");
    let rooted_fs = RootedFs::new(root.path()).expect("open rooted filesystem");
    let running = AtomicBool::new(true);

    let error = collect_list_snapshot_blocking(
        &rooted_fs,
        root.path(),
        ListSnapshotOptions {
            serve_path: root.path(),
            revision_owner: OwnerId::persistent("alice"),
            max_entries: 8,
            max_bytes: 1,
            sort: "name",
            order: "asc",
            running: &running,
            cancellation: &CancellationToken::new(),
            max_duration: Duration::from_secs(5),
        },
    )
    .expect_err("the direct listing allocation must be checked before insertion");

    assert_eq!(error.problem.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error.reason, "snapshot_memory_budget");
    assert_eq!(
        error.problem.public_message(),
        "Directory listing exceeds the snapshot capacity"
    );
}

#[tokio::test]
async fn recursive_search_collection_enforces_allocated_byte_budget() {
    let root = assert_fs::TempDir::new().expect("create search root");
    std::fs::write(root.path().join("match.txt"), "match").expect("create matching entry");
    let rooted_fs = RootedFs::new(root.path()).expect("open rooted filesystem");
    let base_path = root.path().to_path_buf();
    let serve_path = base_path.clone();

    let error = collect_dir_items(
        DirectoryWalk {
            work_tasks: TaskTracker::new(),
            running: Arc::new(AtomicBool::new(true)),
            cancellation: CancellationToken::new(),
            path: base_path.clone(),
            serve_path: serve_path.clone(),
            rooted_fs,
            max_entries: 8,
            max_depth: MAX_DIRECTORY_WALK_DEPTH,
            max_working_bytes: MAX_DIRECTORY_WALK_WORKING_BYTES,
            max_duration: Duration::from_secs(5),
            permit: None,
        },
        |_, _| Ok(true),
        move |entry| {
            pathitem_from_rooted_entry(entry, &base_path, &serve_path, OwnerId::persistent("alice"))
        },
        path_item_heap_bytes,
        Some(CollectionByteBudget {
            max_bytes: 1,
            operation: "search_result",
            reason: "result_memory_budget",
            problem: ListingProblem::SearchResultLimit,
            allocation_problem: ListingProblem::SearchResultLimit,
        }),
    )
    .await
    .expect_err("the allocated result vector must be byte-bounded");

    assert_eq!(error.problem.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error.reason, "result_memory_budget");
    assert_eq!(
        error.problem.public_message(),
        "Search results exceed the memory limit"
    );
}

#[tokio::test]
async fn deep_comb_walk_uses_bounded_linear_working_memory() {
    let root = assert_fs::TempDir::new().expect("create walk root");
    let walk_root = root.path().join("walk");
    std::fs::create_dir(&walk_root).expect("create walk directory");
    let mut continuation = walk_root.clone();
    const DEPTH: usize = 80;
    for depth in 0..DEPTH {
        std::fs::create_dir(continuation.join(format!("s{depth:02}")))
            .expect("create comb side directory");
        let next = continuation.join(format!("d{depth:02}"));
        std::fs::create_dir(&next).expect("create comb continuation");
        continuation = next;
    }
    let rooted_fs = RootedFs::new(root.path()).expect("open rooted filesystem");

    let entries = collect_dir_entries(
        DirectoryWalk {
            work_tasks: TaskTracker::new(),
            running: Arc::new(AtomicBool::new(true)),
            cancellation: CancellationToken::new(),
            path: walk_root,
            serve_path: root.path().to_path_buf(),
            rooted_fs,
            max_entries: DEPTH * 2,
            max_depth: DEPTH,
            max_working_bytes: 256 * 1024,
            max_duration: Duration::from_secs(5),
            permit: None,
        },
        |_, _| Ok(false),
    )
    .await
    .expect("the explicit DFS stack must fit a linear working-set budget");

    assert!(entries.is_empty());
}

#[tokio::test]
async fn recursive_walk_enforces_depth_and_working_set_budgets() {
    let root = assert_fs::TempDir::new().expect("create walk root");
    let walk_root = root.path().join("walk");
    std::fs::create_dir_all(walk_root.join("one/two")).expect("create nested directories");
    let rooted_fs = RootedFs::new(root.path()).expect("open rooted filesystem");

    for (max_depth, max_working_bytes, expected_reason) in [
        (1, MAX_DIRECTORY_WALK_WORKING_BYTES, "depth_budget"),
        (MAX_DIRECTORY_WALK_DEPTH, 1, "working_memory_budget"),
    ] {
        let error = collect_dir_entries(
            DirectoryWalk {
                work_tasks: TaskTracker::new(),
                running: Arc::new(AtomicBool::new(true)),
                cancellation: CancellationToken::new(),
                path: walk_root.clone(),
                serve_path: root.path().to_path_buf(),
                rooted_fs: rooted_fs.clone(),
                max_entries: 8,
                max_depth,
                max_working_bytes,
                max_duration: Duration::from_secs(5),
                permit: None,
            },
            |_, _| Ok(false),
        )
        .await
        .expect_err("the recursive walk budget must reject the tree");

        assert_eq!(error.problem.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error.reason, expected_reason);
    }
}

#[tokio::test]
async fn recursive_walk_rechecks_every_visited_directory_snapshot() {
    let root = assert_fs::TempDir::new().expect("create walk root");
    let walk_root = root.path().join("walk");
    for directory in ["a", "b"] {
        let directory = walk_root.join(directory);
        std::fs::create_dir_all(&directory).expect("create nested directory");
        std::fs::write(directory.join("marker.txt"), "content").expect("create marker");
    }
    let rooted_fs = RootedFs::new(root.path()).expect("open rooted filesystem");
    let first_scanned_directory = Arc::new(StdMutex::new(None::<PathBuf>));
    let scan_state = first_scanned_directory.clone();

    let error = collect_dir_entries(
        DirectoryWalk {
            work_tasks: TaskTracker::new(),
            running: Arc::new(AtomicBool::new(true)),
            cancellation: CancellationToken::new(),
            path: walk_root,
            serve_path: root.path().to_path_buf(),
            rooted_fs,
            max_entries: 16,
            max_depth: MAX_DIRECTORY_WALK_DEPTH,
            max_working_bytes: MAX_DIRECTORY_WALK_WORKING_BYTES,
            max_duration: Duration::from_secs(5),
            permit: None,
        },
        move |entry, _| {
            if entry.metadata.is_file() {
                let parent = entry
                    .path
                    .parent()
                    .expect("a nested marker has a parent")
                    .to_path_buf();
                let mut first = scan_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match first.as_ref() {
                    Some(first) if first != &parent => {
                        std::fs::write(first.join("changed-after-scan.txt"), "new")
                            .expect("mutate an already scanned sibling directory");
                    }
                    None => *first = Some(parent),
                    Some(_) => {}
                }
            }
            Ok(false)
        },
    )
    .await
    .expect_err("a child directory changed after its scan must invalidate the result");

    assert_eq!(error.problem.status(), StatusCode::CONFLICT);
    assert_eq!(error.reason, "directory_snapshot_changed");
    assert_eq!(
        error.problem.public_message(),
        DIRECTORY_CHANGED_DURING_WALK_MESSAGE
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directory_worker_keeps_permit_after_waiter_timeout() {
    let work_tasks = TaskTracker::new();
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = Arc::new(
        slots
            .clone()
            .try_acquire_owned()
            .expect("acquire directory slot"),
    );
    let gate = Arc::new((StdMutex::new(false), Condvar::new()));
    let worker_gate = gate.clone();
    let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
    let worker = spawn_directory_blocking(&work_tasks, Some(permit.clone()), move || {
        let _ = started_sender.send(());
        let (released, condition) = &*worker_gate;
        let mut released = released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*released {
            released = condition
                .wait(released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    });
    drop(permit);
    started_receiver.await.expect("worker must start");

    assert!(
        tokio::time::timeout(Duration::ZERO, worker).await.is_err(),
        "the request-side waiter should time out"
    );
    assert_eq!(slots.available_permits(), 0);

    let (released, condition) = &*gate;
    *released
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    condition.notify_one();
    work_tasks.close();
    work_tasks.wait().await;
    assert_eq!(slots.available_permits(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_request_cancels_its_detached_directory_worker() {
    const ENTRY_COUNT: usize = 128;

    let root = assert_fs::TempDir::new().expect("create cancellation test root");
    for index in 0..ENTRY_COUNT {
        std::fs::write(root.path().join(format!("entry-{index:03}")), "content")
            .expect("create cancellation test entry");
    }
    let rooted_fs = RootedFs::new(root.path()).expect("open rooted filesystem");
    let work_tasks = TaskTracker::new();
    let request_tasks = work_tasks.clone();
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let visited = Arc::new(AtomicUsize::new(0));
    let worker_visited = visited.clone();
    let walk_path = root.path().to_path_buf();
    let serve_path = walk_path.clone();

    let request = tokio::spawn(async move {
        let _cancel_on_drop = CancelOnDrop::new(cancellation);
        collect_dir_entries(
            DirectoryWalk {
                work_tasks: request_tasks,
                running: Arc::new(AtomicBool::new(true)),
                cancellation: worker_cancellation,
                path: walk_path,
                serve_path,
                rooted_fs,
                max_entries: ENTRY_COUNT,
                max_depth: MAX_DIRECTORY_WALK_DEPTH,
                max_working_bytes: MAX_DIRECTORY_WALK_WORKING_BYTES,
                max_duration: Duration::from_secs(5),
                permit: None,
            },
            move |_, _| {
                worker_visited.fetch_add(1, AtomicOrdering::SeqCst);
                std::thread::sleep(Duration::from_millis(2));
                Ok(true)
            },
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        while visited.load(AtomicOrdering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("directory worker did not begin");
    request.abort();
    let _ = request.await;

    work_tasks.close();
    tokio::time::timeout(Duration::from_secs(1), work_tasks.wait())
        .await
        .expect("cancelled directory worker did not stop");
    assert!(
        visited.load(AtomicOrdering::SeqCst) < ENTRY_COUNT,
        "the detached worker ignored request cancellation"
    );
}

#[tokio::test]
async fn missing_walk_root_is_a_retryable_conflict() {
    let root = assert_fs::TempDir::new().expect("create rooted filesystem");
    let missing = root.path().join("removed-before-walk");
    std::fs::create_dir(&missing).expect("create directory to remove");
    let rooted_fs = RootedFs::new(root.path()).expect("open rooted filesystem");
    std::fs::remove_dir(&missing).expect("remove directory before walk");

    let error = collect_dir_entries(
        DirectoryWalk {
            work_tasks: TaskTracker::new(),
            running: Arc::new(AtomicBool::new(true)),
            cancellation: CancellationToken::new(),
            path: missing,
            serve_path: root.path().to_path_buf(),
            rooted_fs,
            max_entries: 16,
            max_depth: MAX_DIRECTORY_WALK_DEPTH,
            max_working_bytes: MAX_DIRECTORY_WALK_WORKING_BYTES,
            max_duration: Duration::from_secs(5),
            permit: None,
        },
        |_, _| Ok(true),
    )
    .await
    .expect_err("a disappeared walk root must not produce a partial result");

    assert_eq!(error.problem.status(), StatusCode::CONFLICT);
    assert_eq!(
        error.problem.public_message(),
        DIRECTORY_CHANGED_DURING_WALK_MESSAGE
    );
}

#[tokio::test]
async fn nested_directory_disappearance_is_a_retryable_conflict() {
    let root = assert_fs::TempDir::new().expect("create rooted filesystem");
    let walk_root = root.path().join("walk");
    let disappearing = walk_root.join("disappearing");
    std::fs::create_dir_all(&disappearing).expect("create nested directory");
    std::fs::write(walk_root.join("possible-partial-result.txt"), "content")
        .expect("create possible partial result");
    let rooted_fs = RootedFs::new(root.path()).expect("open rooted filesystem");
    let remove_path = disappearing.clone();

    let error = collect_dir_entries(
        DirectoryWalk {
            work_tasks: TaskTracker::new(),
            running: Arc::new(AtomicBool::new(true)),
            cancellation: CancellationToken::new(),
            path: walk_root,
            serve_path: root.path().to_path_buf(),
            rooted_fs,
            max_entries: 16,
            max_depth: MAX_DIRECTORY_WALK_DEPTH,
            max_working_bytes: MAX_DIRECTORY_WALK_WORKING_BYTES,
            max_duration: Duration::from_secs(5),
            permit: None,
        },
        move |entry, _| {
            if entry.path == remove_path {
                std::fs::remove_dir(&remove_path)
                    .expect("remove nested directory during traversal");
            }
            Ok(entry.metadata.is_file())
        },
    )
    .await
    .expect_err("a disappeared nested directory must discard partial results");

    assert_eq!(error.problem.status(), StatusCode::CONFLICT);
    assert_eq!(
        error.problem.public_message(),
        DIRECTORY_CHANGED_DURING_WALK_MESSAGE
    );
}
