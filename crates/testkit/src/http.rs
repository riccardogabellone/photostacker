//! A local HTTP/1.1 server for the model downloader's tests: it serves one
//! body on `127.0.0.1` from a std `TcpListener`, behaving as a test asks,
//! and logs every request. Tests never touch the network.

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// How the server answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behaviour {
    /// Serves the body, honouring `Range: bytes=<start>-`: 206 with the
    /// rest, 416 for a start at or past the end, 200 without a range.
    Normal,
    /// As `Normal`, but the first response closes the connection after this
    /// many bytes of its body, although its `Content-Length` promised them
    /// all. Later responses are whole.
    DropAfter(usize),
    /// Answers 404 to every request.
    NotFound,
    /// As `Normal`, but every byte of the body is inverted: the length is
    /// right and the hash is wrong.
    WrongBytes,
    /// Ignores `Range` and answers 200 with the whole body.
    IgnoreRange,
    /// As `Normal`, but every response sends the body only up to this byte
    /// of the whole body, then nothing more, and keeps its connection open
    /// until the test process ends: a server that stalls. A request for the
    /// range from that byte on gets its headers and no byte of the body.
    StallAt(usize),
}

/// One request the server received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The request target, such as `/model.onnx`.
    pub path: String,
    /// The headers, in order, as sent.
    pub headers: Vec<(String, String)>,
}

impl Request {
    /// The value of the header `name`, compared without case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A running server. Its thread serves until the test process ends.
#[derive(Debug)]
pub struct Server {
    url: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Server {
    /// The URL of the served body.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The requests received so far, in order. A request is logged before
    /// it is answered, so a client that got its answer finds it here.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

/// Starts a server of `body` on a free port of `127.0.0.1`. It answers one
/// request per connection, then closes it (`Connection: close`), unless it
/// stalls ([`Behaviour::StallAt`]).
pub fn serve(body: Vec<u8>, behaviour: Behaviour) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
    let url = format!(
        "http://{}/model.onnx",
        listener.local_addr().expect("the bound address")
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requests);
    let body = match behaviour {
        Behaviour::WrongBytes => body.iter().map(|byte| !byte).collect(),
        _ => body,
    };
    thread::spawn(move || {
        let mut drop_pending = matches!(behaviour, Behaviour::DropAfter(_));
        // The stalled connections, held open while the server serves on.
        let mut stalled = Vec::new();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&stream) else {
                continue;
            };
            log.lock().unwrap().push(request.clone());
            let cut = match behaviour {
                Behaviour::DropAfter(n) if drop_pending => {
                    drop_pending = false;
                    Some(n)
                }
                _ => None,
            };
            // A client that went away is not the server's failure.
            let _ = respond(&mut stream, &request, &body, behaviour, cut);
            if matches!(behaviour, Behaviour::StallAt(_)) {
                stalled.push(stream);
            } else {
                // The whole request was read, so closing sends a FIN, not a
                // reset: the client reads every byte sent before it sees the
                // connection end.
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    });
    Server { url, requests }
}

/// Reads the request line and headers; `None` for a malformed request.
fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).ok()?;
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            return Some(Request { path, headers });
        }
        let (key, value) = header.split_once(':')?;
        headers.push((key.trim().to_string(), value.trim().to_string()));
    }
}

/// Answers `request` per `behaviour`, sending only `cut` bytes of the body
/// when set. The caller closes the connection, or holds it open.
fn respond(
    stream: &mut TcpStream,
    request: &Request,
    body: &[u8],
    behaviour: Behaviour,
    cut: Option<usize>,
) -> std::io::Result<()> {
    let len = body.len();
    let start = match behaviour {
        Behaviour::IgnoreRange => None,
        _ => request.header("range").and_then(range_start),
    };
    // `from`: where the payload starts in the body; `len` for none.
    let (status, content_range, from) = match (behaviour, start) {
        (Behaviour::NotFound, _) => ("404 Not Found", None, len),
        (_, Some(start)) if start >= len => (
            "416 Range Not Satisfiable",
            Some(format!("bytes */{len}")),
            len,
        ),
        (_, Some(start)) => (
            "206 Partial Content",
            Some(format!("bytes {start}-{}/{len}", len - 1)),
            start,
        ),
        (_, None) => ("200 OK", None, 0),
    };
    let payload = &body[from..];
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        payload.len()
    );
    if let Some(content_range) = content_range {
        head.push_str(&format!("Content-Range: {content_range}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    let sent = match behaviour {
        Behaviour::StallAt(at) => &body[from..at.clamp(from, len)],
        _ => cut.map_or(payload, |n| &payload[..n.min(payload.len())]),
    };
    stream.write_all(sent)?;
    stream.flush()
}

/// The start of a `bytes=<start>-` range.
fn range_start(range: &str) -> Option<usize> {
    range
        .strip_prefix("bytes=")?
        .strip_suffix('-')?
        .parse()
        .ok()
}
