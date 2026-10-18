//! Helpers shared by tests.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use reqwest::blocking::Client;

/// Minimal HTTP server: answers 200 for known paths, 404 for the rest.
/// The key is the request path, query string included. In text bodies
/// `{BASE}` becomes the server URL (the port is known only after bind).
pub struct TestServer {
    pub url: String,
    pub hits: Arc<AtomicUsize>,
}

impl TestServer {
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

pub fn serve(files: &[(&str, &[u8])]) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let files: HashMap<String, Vec<u8>> = files
        .iter()
        .map(|(p, b)| {
            let body = match std::str::from_utf8(b) {
                Ok(text) if text.contains("{BASE}") => text.replace("{BASE}", &url).into_bytes(),
                _ => b.to_vec(),
            };
            (p.to_string(), body)
        })
        .collect();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut header = String::new();
            while reader.read_line(&mut header).unwrap() > 2 {
                header.clear();
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let path = request.split_whitespace().nth(1).unwrap_or("");
            let (status, body) = match files.get(path) {
                Some(b) => ("200 OK", b.as_slice()),
                None => ("404 Not Found", &b""[..]),
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
        }
    });
    TestServer { url, hits }
}

/// Client without proxy: requests to 127.0.0.1 must not go through a system proxy.
pub fn client() -> Client {
    Client::builder().no_proxy().build().unwrap()
}
