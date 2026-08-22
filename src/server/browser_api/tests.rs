use super::super::ServerLifecycle;
use super::*;
use crate::{Args, auth::AuthConfig};
use std::{os::unix::fs::PermissionsExt, time::Duration};

const TEST_ACCOUNT: &str = "user:$argon2id$v=19$m=19456,t=2,p=1$HdPI2G8k0h+yEgnqIt2rSw$P+MRyz7wH+b/iPY+He/9DApcy6yB9TAoo7j2JG1Smzs";

fn test_server(root: &Path) -> (Server, assert_fs::TempDir) {
    let state_dir = assert_fs::TempDir::new().unwrap();
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = Server::init_with_lifecycle(
        Args {
            serve_path: root.to_path_buf(),
            state_dir: Some(state_dir.path().to_path_buf()),
            auth: AuthConfig::new(&[TEST_ACCOUNT]).unwrap(),
            ..Args::default()
        },
        ServerLifecycle::new(),
    )
    .unwrap();
    (server, state_dir)
}

#[tokio::test]
async fn move_commit_rejects_target_created_after_precheck() {
    let temp = assert_fs::TempDir::new().unwrap();
    let source = temp.path().join("source.txt");
    let destination = temp.path().join("destination.txt");
    let rooted_fs = RootedFs::new(temp.path()).unwrap();
    std::fs::write(&source, "source-content").unwrap();
    let expected_source = rooted_fs.replacement_identity(&source).await.unwrap();

    std::fs::write(&destination, "competitor-content").unwrap();
    assert_eq!(
        commit_relocation(&rooted_fs, &source, &destination, expected_source, None)
            .await
            .unwrap(),
        RelocationCommitOutcome::DestinationExists
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), "source-content");
    assert_eq!(
        std::fs::read_to_string(&destination).unwrap(),
        "competitor-content"
    );
}

#[tokio::test]
async fn missing_relocation_noreplace_collision_matches_request_semantics() {
    for (expected_destination, expected_outcome) in [
        (None, RelocationCommitOutcome::DestinationExists),
        (
            Some(ReplacementTargetIdentity::Missing),
            RelocationCommitOutcome::DestinationChanged,
        ),
    ] {
        let temp = assert_fs::TempDir::new().unwrap();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("destination.txt");
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        std::fs::write(&source, "source-content").unwrap();
        let expected_source = rooted_fs.replacement_identity(&source).await.unwrap();

        let competing_destination = destination.clone();
        rooted_fs.inject_before_missing_rename_once(move || {
            std::fs::write(competing_destination, "competitor-content").unwrap();
        });

        assert_eq!(
            commit_relocation(
                &rooted_fs,
                &source,
                &destination,
                expected_source,
                expected_destination,
            )
            .await
            .unwrap(),
            expected_outcome
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "source-content");
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "competitor-content"
        );
    }
}

#[tokio::test]
async fn missing_relocation_is_unknown_when_the_renamed_source_misses_its_anchor() {
    let temp = assert_fs::TempDir::new().unwrap();
    let source = temp.path().join("source.txt");
    let displaced_source = temp.path().join("displaced-source.txt");
    let destination = temp.path().join("destination.txt");
    let rooted_fs = RootedFs::new(temp.path()).unwrap();
    std::fs::write(&source, "source-content").unwrap();
    let expected_source = rooted_fs.replacement_identity(&source).await.unwrap();

    let replaced_source = source.clone();
    let preserved_source = displaced_source.clone();
    rooted_fs.inject_before_missing_rename_once(move || {
        std::fs::rename(&replaced_source, &preserved_source).unwrap();
        std::fs::write(&replaced_source, "external-content").unwrap();
    });

    let error = commit_relocation(
        &rooted_fs,
        &source,
        &destination,
        expected_source,
        Some(ReplacementTargetIdentity::Missing),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert!(!source.exists());
    assert_eq!(
        std::fs::read_to_string(&displaced_source).unwrap(),
        "source-content"
    );
    assert_eq!(
        std::fs::read_to_string(&destination).unwrap(),
        "external-content"
    );
}

#[tokio::test]
async fn overwrite_commit_rejects_two_names_for_the_same_inode() {
    let temp = assert_fs::TempDir::new().unwrap();
    let source = temp.path().join("source.txt");
    let destination = temp.path().join("destination.txt");
    let rooted_fs = RootedFs::new(temp.path()).unwrap();
    std::fs::write(&source, "shared-content").unwrap();
    std::fs::hard_link(&source, &destination).unwrap();
    let expected_source = rooted_fs.replacement_identity(&source).await.unwrap();
    let expected_destination = rooted_fs.replacement_identity(&destination).await.unwrap();

    assert_eq!(
        commit_relocation(
            &rooted_fs,
            &source,
            &destination,
            expected_source,
            Some(expected_destination),
        )
        .await
        .unwrap(),
        RelocationCommitOutcome::SameFile
    );
    assert!(source.exists());
    assert!(destination.exists());
}

#[tokio::test]
async fn relocation_commit_rejects_a_source_replaced_after_revision_check() {
    let temp = assert_fs::TempDir::new().unwrap();
    let source = temp.path().join("source.txt");
    let replacement = temp.path().join("replacement.txt");
    let destination = temp.path().join("destination.txt");
    let rooted_fs = RootedFs::new(temp.path()).unwrap();
    std::fs::write(&source, "original-source").unwrap();
    let expected_source = rooted_fs.replacement_identity(&source).await.unwrap();
    std::fs::write(&replacement, "external-replacement").unwrap();
    std::fs::rename(&replacement, &source).unwrap();

    assert_eq!(
        commit_relocation(&rooted_fs, &source, &destination, expected_source, None)
            .await
            .unwrap(),
        RelocationCommitOutcome::SourceChanged
    );
    assert_eq!(
        std::fs::read_to_string(source).unwrap(),
        "external-replacement"
    );
    assert!(!destination.exists());
}

#[tokio::test]
async fn relocation_commit_reports_a_removed_destination_directory_as_changed() {
    for overwrite in [false, true] {
        let temp = assert_fs::TempDir::new().unwrap();
        let source = temp.path().join("source.txt");
        let destination_directory = temp.path().join("destination");
        let destination = destination_directory.join("source.txt");
        let rooted_fs = RootedFs::new(temp.path()).unwrap();
        std::fs::write(&source, "source-content").unwrap();
        std::fs::create_dir(&destination_directory).unwrap();
        let expected_source = rooted_fs.replacement_identity(&source).await.unwrap();
        let expected_destination = rooted_fs.replacement_identity(&destination).await.unwrap();

        // Simulate an external writer removing the directory after the
        // handler's existence precheck but before the atomic commit.
        std::fs::remove_dir(&destination_directory).unwrap();
        let outcome = commit_relocation(
            &rooted_fs,
            &source,
            &destination,
            expected_source,
            overwrite.then_some(expected_destination),
        )
        .await
        .unwrap();
        assert_eq!(outcome, RelocationCommitOutcome::DestinationChanged);
        assert!(source.is_file());
        assert!(!destination_directory.exists());
    }
}

#[tokio::test]
async fn relocation_state_unavailable_codes_are_endpoint_specific() {
    for (kind, expected) in [
        (RelocationKind::Move, "move_state_unavailable"),
        (RelocationKind::Rename, "rename_state_unavailable"),
    ] {
        let mut response = Response::default();
        render_relocation_state_unavailable(kind, None, &mut response).unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], expected);
        assert_eq!(problem["recovery"], "retry");
    }
}

#[tokio::test]
async fn unavailable_state_admission_rejects_move_before_commit_and_allows_same_id_retry() {
    let temp = assert_fs::TempDir::new().unwrap();
    let source = temp.path().join("source.txt");
    let destination_directory = temp.path().join("target");
    let destination = destination_directory.join("source.txt");
    std::fs::write(&source, "source-content").unwrap();
    std::fs::create_dir(&destination_directory).unwrap();
    let (server, _state_dir) = test_server(temp.path());
    let source_revision = target_revision(
        OwnerId::persistent("owner"),
        Path::new("source.txt"),
        server
            .content
            .rooted_fs
            .replacement_identity(&source)
            .await
            .unwrap(),
    )
    .unwrap()
    .encode();
    let operation_id = Uuid::new_v4();
    let fingerprint = OperationFingerprint::new(&[b"state-admission-test"]);
    let operation = match server
        .state
        .operation_registry
        .begin("owner", operation_id, fingerprint)
        .await
        .unwrap()
    {
        BeginOperation::Started(operation) => operation,
        _ => panic!("a fresh operation id must start"),
    };
    let release = server
        .state
        .state_store
        .saturate_command_queue_for_test()
        .unwrap();

    let mut unavailable = Response::default();
    server
        .handle_api_move(
            "owner",
            MoveRequest {
                source: "/source.txt".to_string(),
                directory: "/target".to_string(),
                source_revision: Some(source_revision.clone()),
                destination_revision: None,
                overwrite: false,
            },
            Some((operation_id, operation)),
            MutationProgress::default(),
            &mut unavailable,
        )
        .await
        .unwrap();
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        unavailable.headers().get("x-dufs-operation-state").unwrap(),
        "rejected"
    );
    let body = unavailable.into_body().collect().await.unwrap().to_bytes();
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["code"], "move_state_unavailable");
    assert_eq!(problem["recovery"], "retry");
    assert!(source.is_file());
    assert!(!destination.exists());

    release.send(()).unwrap();
    let retry = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match server
                .state
                .operation_registry
                .begin("owner", operation_id, fingerprint)
                .await
                .unwrap()
            {
                BeginOperation::Started(operation) => return operation,
                BeginOperation::Running | BeginOperation::Unavailable => {
                    tokio::task::yield_now().await;
                }
                BeginOperation::Replay(_) | BeginOperation::Conflict | BeginOperation::Full => {
                    panic!("a safely rejected move became terminal or conflicted")
                }
            }
        }
    })
    .await
    .expect("the abandoned operation id did not become retryable");

    let mut completed = Response::default();
    server
        .handle_api_move(
            "owner",
            MoveRequest {
                source: "/source.txt".to_string(),
                directory: "/target".to_string(),
                source_revision: Some(source_revision),
                destination_revision: None,
                overwrite: false,
            },
            Some((operation_id, retry)),
            MutationProgress::default(),
            &mut completed,
        )
        .await
        .unwrap();
    assert_eq!(completed.status(), StatusCode::NO_CONTENT);
    assert!(!source.exists());
    assert_eq!(
        std::fs::read_to_string(destination).unwrap(),
        "source-content"
    );
}
