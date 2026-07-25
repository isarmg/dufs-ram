use dufs::{Args, server::Server};
use std::sync::{Arc, atomic::AtomicBool};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[test]
fn reusable_server_layer_can_be_constructed_without_starting_a_process() {
    let root = assert_fs::TempDir::new().expect("create temporary shared root");
    let args = Args {
        serve_path: root
            .path()
            .canonicalize()
            .expect("canonicalize temporary shared root"),
        ..Args::default()
    };

    Server::init(
        args,
        Arc::new(AtomicBool::new(true)),
        TaskTracker::new(),
        TaskTracker::new(),
        CancellationToken::new(),
        CancellationToken::new(),
    )
    .expect("construct reusable server layer");
}
