//! Fake browser used by the launcher integration tests.
//!
//! Parses `--remote-debugging-port=<port>` (Chromium-style) and
//! `--remote-debugging-port <port>` (Firefox-style), binds 127.0.0.1:<port>,
//! and replies to HTTP GET `/json/version` with a JSON body containing
//! `webSocketDebuggerUrl`. Sleeps forever after that.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

fn parse_port(args: &[String]) -> Option<u16> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(rest) = a.strip_prefix("--remote-debugging-port=") {
            return rest.parse().ok();
        }
        if a == "--remote-debugging-port" {
            if let Some(v) = it.next() {
                return v.parse().ok();
            }
        }
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let port = parse_port(&args).expect("missing --remote-debugging-port");

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind fake-browser TCP listener");

    let body = format!(
        "{{\"Browser\":\"FakeBrowser/1.0\",\"webSocketDebuggerUrl\":\"ws://127.0.0.1:{port}/devtools/browser/fake\"}}"
    );

    loop {
        let (mut stream, _) = match listener.accept() {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Read just the request line + headers (we don't care about contents).
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        // Consume headers
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                break;
            }
            if h == "\r\n" || h == "\n" || h.is_empty() {
                break;
            }
        }

        let response = if line.starts_with("GET /json/version") {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
        } else {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        };
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }
}
