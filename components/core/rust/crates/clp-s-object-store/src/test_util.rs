//! An in-process S3 server for tests: path-style `HeadObject` and `GetObject` (with the
//! `Range: bytes=a-b` semantics of S3) over plain HTTP/1.1, with fault injection and a request log.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use crate::S3Client;

/// How the server misbehaves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MockBehavior {
    /// Answer ranged GETs with the whole object (`200 OK`), like a server without range support.
    pub ignores_range: bool,

    /// Drop this many bytes from the end of every GET response body (and its length).
    pub truncated_size: u64,

    /// Wait this long before answering a GET.
    pub get_delay_ms: u64,

    /// Answer ranged GETs with the range of the same length starting this many bytes later, and a
    /// `Content-Range` that says so.
    pub range_shift: u64,
}

/// One request the server received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockRequest {
    pub method: String,
    pub bucket: String,
    pub key: String,
    /// The `Range` header, if any.
    pub range: Option<String>,
}

/// An S3 server on `127.0.0.1` serving the objects put into it. The server stops when dropped.
#[derive(Debug)]
pub struct MockS3Server {
    endpoint_url: String,
    state: Arc<Mutex<State>>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl MockS3Server {
    /// Factory function.
    ///
    /// Binds an ephemeral port on `127.0.0.1` and serves it from tasks on `runtime`.
    ///
    /// # Returns
    ///
    /// The running server on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`std::net::TcpListener::bind`]'s return values on failure.
    /// * Forwards [`std::net::TcpListener::local_addr`]'s return values on failure.
    /// * Forwards [`std::net::TcpListener::set_nonblocking`]'s return values on failure.
    /// * Forwards [`tokio::net::TcpListener::from_std`]'s return values on failure.
    pub fn start(runtime: &tokio::runtime::Handle) -> Result<Self, std::io::Error> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let listener = {
            let _guard = runtime.enter();
            tokio::net::TcpListener::from_std(listener)?
        };
        let state = Arc::new(Mutex::new(State::default()));
        let accept_state = Arc::clone(&state);
        let accept_task = runtime.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve_connection(stream, Arc::clone(&accept_state)));
            }
        });
        Ok(Self {
            endpoint_url: format!("http://{address}"),
            state,
            accept_task,
        })
    }

    /// # Returns
    ///
    /// The `http://127.0.0.1:port` endpoint of the server.
    #[must_use]
    pub fn endpoint_url(&self) -> &str {
        &self.endpoint_url
    }

    /// # Returns
    ///
    /// A client of this server: path-style, region `us-east-1`, static test credentials.
    #[must_use]
    pub fn client(&self) -> S3Client {
        use aws_sdk_s3::config::BehaviorVersion;
        use aws_sdk_s3::config::Credentials;
        use aws_sdk_s3::config::Region;

        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(&self.endpoint_url)
            .force_path_style(true)
            .credentials_provider(Credentials::new("test", "test", None, None, "mock"))
            .build();
        S3Client::from_conf(config)
    }

    /// Stores `bytes` as object `bucket`/`key`, replacing any previous object.
    pub fn put_object(&self, bucket: &str, key: &str, bytes: Vec<u8>) {
        self.lock()
            .objects
            .insert((bucket.to_owned(), key.to_owned()), Arc::from(bytes));
    }

    /// Makes the server answer every later request with `behavior`.
    pub fn set_behavior(&self, behavior: MockBehavior) {
        self.lock().behavior = behavior;
    }

    /// # Returns
    ///
    /// Every request received so far, in arrival order.
    #[must_use]
    pub fn requests(&self) -> Vec<MockRequest> {
        self.lock().requests.clone()
    }

    /// Forgets every request received so far.
    pub fn clear_requests(&self) {
        self.lock().requests.clear();
    }

    /// # Returns
    ///
    /// The locked server state.
    ///
    /// # Panics
    ///
    /// Panics if a connection task panicked while holding the lock.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("no connection task panics while holding the lock")
    }
}

impl Drop for MockS3Server {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// The objects, behaviour and request log shared by the connections.
#[derive(Debug, Default)]
struct State {
    objects: HashMap<(String, String), Arc<[u8]>>,
    behavior: MockBehavior,
    requests: Vec<MockRequest>,
}

/// An HTTP response.
struct Response {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,

    /// The `Content-Length` of a body-less (`HEAD`) response.
    head_length: Option<usize>,
}

impl Response {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// An S3 XML error response.
    fn error(status: &'static str, code: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type", "application/xml".to_owned())],
            body: format!(
                "<?xml version=\"1.0\" \
                 encoding=\"UTF-8\"?>\n<Error><Code>{code}</Code><Message>{code}</Message></Error>"
            )
            .into_bytes(),
            head_length: None,
        }
    }

    /// # Returns
    ///
    /// The response as HTTP/1.1 bytes, without the body for `HEAD`.
    fn into_bytes(self, is_head: bool) -> Vec<u8> {
        let content_length = self.head_length.unwrap_or(self.body.len());
        let mut bytes = format!(
            "HTTP/1.1 {}\r\nContent-Length: {content_length}\r\n",
            self.status
        )
        .into_bytes();
        for (name, value) in &self.headers {
            bytes.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        bytes.extend_from_slice(b"\r\n");
        if !is_head {
            bytes.extend_from_slice(&self.body);
        }
        bytes
    }
}

/// Serves the requests of one keep-alive connection until the peer closes it.
async fn serve_connection(mut stream: tokio::net::TcpStream, state: Arc<Mutex<State>>) {
    let mut pending = Vec::new();
    let mut chunk = vec![0; 8192];
    loop {
        let header_end = loop {
            if let Some(position) = pending.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(size) => pending.extend_from_slice(&chunk[..size]),
            }
        };
        let head = String::from_utf8_lossy(&pending[..header_end]).into_owned();
        pending.drain(..header_end);
        let request = parse_request(&head);
        let body_size = request
            .as_ref()
            .map_or(0, |(_, content_length)| *content_length);
        while pending.len() < body_size {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(size) => pending.extend_from_slice(&chunk[..size]),
            }
        }
        pending.drain(..body_size);
        let (is_head, response) = match request {
            Some((request, _)) => {
                let is_head = "HEAD" == request.method;
                (is_head, respond(request, &state).await)
            }
            None => (false, Response::error("400 Bad Request", "BadRequest")),
        };
        if stream
            .write_all(&response.into_bytes(is_head))
            .await
            .is_err()
        {
            return;
        }
    }
}

/// # Returns
///
/// The request in `head` (path-style, key percent-decoded) and its `Content-Length`, or `None` if
/// it is malformed.
fn parse_request(head: &str) -> Option<(MockRequest, usize)> {
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_owned();
    let target = request_line.next()?;
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    let (bucket, key) = path.strip_prefix('/')?.split_once('/')?;
    let mut range = None;
    let mut content_length = 0;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("range") {
            range = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().ok()?;
        }
    }
    Some((
        MockRequest {
            method,
            bucket: percent_decode(bucket)?,
            key: percent_decode(key)?,
            range,
        },
        content_length,
    ))
}

/// Logs `request` and answers it.
///
/// # Returns
///
/// The response.
async fn respond(request: MockRequest, state: &Mutex<State>) -> Response {
    let (object, behavior) = {
        let mut state = state
            .lock()
            .expect("no connection task panics while holding the lock");
        state.requests.push(request.clone());
        let object = state
            .objects
            .get(&(request.bucket.clone(), request.key.clone()))
            .cloned();
        (object, state.behavior)
    };
    let Some(object) = object else {
        return Response::error("404 Not Found", "NoSuchKey");
    };
    let size = object.len();
    match request.method.as_str() {
        "HEAD" => Response {
            status: "200 OK",
            headers: vec![("Accept-Ranges", "bytes".to_owned())],
            body: Vec::new(),
            head_length: Some(size),
        },
        "GET" => {
            if 0 != behavior.get_delay_ms {
                tokio::time::sleep(std::time::Duration::from_millis(behavior.get_delay_ms)).await;
            }
            let range_shift = usize::try_from(behavior.range_shift).unwrap_or(usize::MAX);
            let range = match request.range.as_deref() {
                Some(range) if !behavior.ignores_range => {
                    match parse_range(range, size).and_then(|(first, last)| {
                        let last = last.checked_add(range_shift)?;
                        (last < size).then_some((first + range_shift, last))
                    }) {
                        Some(range) => Some(range),
                        None => {
                            return Response::error("416 Range Not Satisfiable", "InvalidRange");
                        }
                    }
                }
                _ => None,
            };
            let truncated_size = usize::try_from(behavior.truncated_size).unwrap_or(usize::MAX);
            let (status, headers, body) = match range {
                Some((first, last)) => (
                    "206 Partial Content",
                    vec![("Content-Range", format!("bytes {first}-{last}/{size}"))],
                    &object[first..=last],
                ),
                None => ("200 OK", Vec::new(), &object[..]),
            };
            Response {
                status,
                headers,
                body: body[..body.len().saturating_sub(truncated_size)].to_vec(),
                head_length: None,
            }
        }
        _ => Response::error("405 Method Not Allowed", "MethodNotAllowed"),
    }
}

/// Parses a `bytes=first-[last]` range of an object of `size` bytes, clamping `last` like S3.
///
/// # Returns
///
/// The first and last byte offsets, or `None` if the range is malformed or not satisfiable.
fn parse_range(range: &str, size: usize) -> Option<(usize, usize)> {
    let (first, last) = range.strip_prefix("bytes=")?.split_once('-')?;
    let first: usize = first.parse().ok()?;
    let last = if last.is_empty() {
        size.checked_sub(1)?
    } else {
        last.parse::<usize>().ok()?.min(size.checked_sub(1)?)
    };
    (first <= last).then_some((first, last))
}

/// # Returns
///
/// `text` with its `%XX` escapes decoded, or `None` if an escape or the result is invalid.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if b'%' == bytes[index] {
            let escape = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(escape, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}
