use dufs::{Args, auth::AuthConfig, server::Server};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const TEST_ACCOUNT: &str = "user:$argon2id$v=19$m=19456,t=2,p=1$HdPI2G8k0h+yEgnqIt2rSw$P+MRyz7wH+b/iPY+He/9DApcy6yB9TAoo7j2JG1Smzs";

fn private_state_dir() -> assert_fs::TempDir {
    let state_dir = assert_fs::TempDir::new().expect("create temporary state directory");
    std::fs::set_permissions(state_dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("make state directory private");
    state_dir
}

fn authenticated_args(root: &Path, state_dir: &Path) -> Args {
    Args {
        serve_path: root
            .canonicalize()
            .expect("canonicalize temporary shared root"),
        state_dir: Some(state_dir.to_path_buf()),
        auth: AuthConfig::new(&[TEST_ACCOUNT]).expect("create test account"),
        ..Args::default()
    }
}

#[tokio::test]
async fn reusable_server_layer_can_be_constructed_without_starting_a_process() {
    let root = assert_fs::TempDir::new().expect("create temporary shared root");
    let state_dir = private_state_dir();
    let args = authenticated_args(root.path(), state_dir.path());

    let runtime = Server::builder(args)
        .build()
        .expect("construct reusable server layer");
    runtime.shutdown().await;
}

#[tokio::test]
async fn reusable_server_can_own_an_isolated_list_snapshot_cache() {
    let root = assert_fs::TempDir::new().expect("create temporary shared root");
    let state_dir = private_state_dir();
    let runtime = Server::builder(authenticated_args(root.path(), state_dir.path()))
        .with_isolated_list_snapshot_cache()
        .build()
        .expect("construct server with an isolated listing cache");

    runtime.shutdown().await;
}

#[tokio::test]
async fn runtime_shutdown_is_observable_and_idempotent() {
    let root = assert_fs::TempDir::new().expect("create temporary shared root");
    let state_dir = private_state_dir();
    let runtime = Server::builder(authenticated_args(root.path(), state_dir.path()))
        .build()
        .expect("construct reusable server layer");

    assert!(runtime.active_task_counts().0 > 0);
    runtime.shutdown().await;
    assert_eq!(runtime.active_task_counts(), (0, 0));

    runtime.shutdown().await;
    assert_eq!(runtime.active_task_counts(), (0, 0));
}

#[tokio::test]
async fn configured_state_database_is_created_privately() {
    let root = assert_fs::TempDir::new().expect("create temporary shared root");
    let state_dir = private_state_dir();
    let state_db = state_dir.path().join("state.sqlite3");
    let args = authenticated_args(root.path(), state_dir.path());

    let runtime = Server::builder(args)
        .build()
        .expect("construct server with persistent state");
    runtime.shutdown().await;

    let metadata = std::fs::metadata(&state_db).expect("inspect SQLite state database");
    assert!(metadata.is_file());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    let header = std::fs::read(&state_db).expect("read SQLite state database");
    assert!(header.starts_with(b"SQLite format 3\0"));
}

#[tokio::test]
async fn dropping_runtime_cancels_its_lifecycle() {
    let root = assert_fs::TempDir::new().expect("create temporary shared root");
    let state_dir = private_state_dir();
    let runtime = Server::builder(authenticated_args(root.path(), state_dir.path()))
        .build()
        .expect("construct reusable server layer");
    let shutdown = runtime.shutdown_token();
    let force_shutdown = runtime.force_shutdown_token();

    drop(runtime);

    assert!(shutdown.is_cancelled());
    assert!(force_shutdown.is_cancelled());
}

#[test]
fn builder_without_tokio_runtime_returns_an_error() {
    let root = assert_fs::TempDir::new().expect("create temporary shared root");
    let state_dir = private_state_dir();
    let result = Server::builder(authenticated_args(root.path(), state_dir.path())).build();

    let error = result.err().expect("construction should require Tokio");
    assert!(error.to_string().contains("active Tokio runtime"));
}
