use crate::http_logger::HttpLogger;
use hyper::{Method, Request};
use std::{collections::HashMap, net::SocketAddr};

/// Data parsed once at the HTTP boundary and shared by request handlers.
///
/// Normalized paths and authentication data can move here incrementally
/// without changing the transport entry point again.
#[derive(Debug)]
pub struct RequestContext {
    peer: SocketAddr,
    head_request: bool,
    access_log: HashMap<String, String>,
}

impl RequestContext {
    pub fn new<B>(request: &Request<B>, peer: SocketAddr, logger: &HttpLogger) -> Self {
        let mut access_log = logger.data(request);
        logger.set_runtime_value(&mut access_log, "remote_addr", || peer.ip().to_string());
        Self {
            peer,
            head_request: request.method() == Method::HEAD,
            access_log,
        }
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn is_head_request(&self) -> bool {
        self.head_request
    }

    pub fn access_log(&self) -> &HashMap<String, String> {
        &self.access_log
    }

    pub fn access_log_mut(&mut self) -> &mut HashMap<String, String> {
        &mut self.access_log
    }

    #[cfg(test)]
    pub(crate) fn for_test(peer: SocketAddr) -> Self {
        Self {
            peer,
            head_request: false,
            access_log: HashMap::new(),
        }
    }
}
