use crate::http_logger::HttpLogger;
use hyper::{Request, body::Incoming};
use std::{collections::HashMap, net::SocketAddr};

/// Data parsed once at the HTTP boundary and shared by request handlers.
///
/// Normalized paths and authentication data can move here incrementally
/// without changing the transport entry point again.
#[derive(Debug)]
pub struct RequestContext {
    peer: SocketAddr,
    access_log: HashMap<String, String>,
}

impl RequestContext {
    pub fn new(request: &Request<Incoming>, peer: SocketAddr, logger: &HttpLogger) -> Self {
        let mut access_log = logger.data(request);
        access_log.insert("remote_addr".to_string(), peer.ip().to_string());
        Self { peer, access_log }
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn access_log(&self) -> &HashMap<String, String> {
        &self.access_log
    }

    pub fn access_log_mut(&mut self) -> &mut HashMap<String, String> {
        &mut self.access_log
    }
}
