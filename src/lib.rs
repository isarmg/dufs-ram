#[macro_use]
extern crate log;

// Depending on the shared gate makes unsupported server targets fail during
// compilation, while this public identity prevents release metadata drift.
pub const SERVER_TARGET: &str = sarmg_server_target::SERVER_TARGET_TRIPLE;

pub mod app_error;
pub mod args;
pub mod auth;
pub mod http_logger;
pub mod http_utils;
pub mod logger;
pub mod request_context;
pub mod server;
pub mod utils;

pub use args::Args;
