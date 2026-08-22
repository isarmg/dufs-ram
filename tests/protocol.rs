#[path = "support/fixtures.rs"]
mod fixtures;

use fixtures::{Error, TestServer, server};

use rstest::rstest;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const HTTP2_PRIOR_KNOWLEDGE_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

#[rstest]
fn cleartext_http2_prior_knowledge_is_rejected(server: TestServer) -> Result<(), Error> {
    let mut stream = TcpStream::connect(("127.0.0.1", server.port()))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(HTTP2_PRIOR_KNOWLEDGE_PREFACE)?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;

    assert!(
        response.is_empty() || response.starts_with(b"HTTP/1.1 400 "),
        "HTTP/2 prior knowledge unexpectedly produced a protocol response: {response:?}"
    );
    server.get(server.url())?.error_for_status()?;
    Ok(())
}

#[rstest]
fn cleartext_http2_upgrade_is_not_accepted(server: TestServer) -> Result<(), Error> {
    let mut stream = TcpStream::connect(("127.0.0.1", server.port()))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let request = format!(
        "GET /__dufs__/login HTTP/1.1\r\n\
         Host: 127.0.0.1:{}\r\n\
         Connection: Upgrade, HTTP2-Settings\r\n\
         Upgrade: h2c\r\n\
         HTTP2-Settings: AAMAAABkAAQAAP__\r\n\
         \r\n",
        server.port()
    );
    stream.write_all(request.as_bytes())?;

    let mut response = Vec::new();
    while !response.windows(4).any(|window| window == b"\r\n\r\n") {
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
    }

    assert!(
        response.starts_with(b"HTTP/1.1 200 ") || response.starts_with(b"HTTP/1.1 400 "),
        "h2c upgrade request produced an unexpected response: {response:?}"
    );
    assert!(
        !response.starts_with(b"HTTP/1.1 101 "),
        "h2c upgrade was unexpectedly accepted: {response:?}"
    );
    server.get(server.url())?.error_for_status()?;
    Ok(())
}
