//! The transport: the system curl, as for the model files
//! ([`crate::models`]), so no HTTP or TLS crate is added.
//!
//! The credential never reaches curl's command line (a process listing
//! would show it): curl reads its whole request, header included, as a
//! config from its stdin (`-K -`), and `-q` comes first so a user's
//! `.curlrc` cannot turn on tracing. curl never follows a redirect itself:
//! [`super::api::Client`] does, and drops the header when the redirect
//! leaves the origin. curl's own error text is never read; a failure is its
//! exit code, mapped to a [`Failure`].

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::Secret;

/// The request methods the trackers send. One: the client is read-only by
/// construction (the JQL and CQL searches are GETs too).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
}

impl Method {
    /// The method's name on the wire.
    pub fn name(self) -> &'static str {
        match self {
            Method::Get => "GET",
        }
    }
}

/// The methods of a write (project tracking writes, [`super::write`]). Apart
/// from [`Method`] so the read client stays GET-only by construction: a read
/// builds a [`Request`] with `write: None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMethod {
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
}

impl WriteMethod {
    /// The method's name on the wire.
    pub fn name(self) -> &'static str {
        match self {
            WriteMethod::Post => "POST",
            WriteMethod::Put => "PUT",
        }
    }
}

/// A write's method and JSON body.
#[derive(Debug, Clone, Copy)]
pub struct WriteBody<'a> {
    /// `POST` or `PUT`.
    pub method: WriteMethod,
    /// The JSON text: compact, so it holds no raw line break, and starting
    /// with `{` so curl never reads it as a file name.
    pub body: &'a str,
}

/// One request.
pub struct Request<'a> {
    /// Its method for a read; a write's is in `write`.
    pub method: Method,
    /// The write this request carries, when it is one.
    pub write: Option<WriteBody<'a>>,
    /// The whole URL.
    pub url: &'a str,
    /// The `Authorization` header value, or none (after a redirect to
    /// another origin).
    pub auth: Option<&'a Secret>,
    /// The time it may take.
    pub timeout: Duration,
    /// The largest response accepted, in bytes.
    pub max_bytes: u64,
}

/// One response.
#[derive(Debug, Clone, Default)]
pub struct Response {
    /// The HTTP status.
    pub status: u16,
    /// The headers, names lower case.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Response {
    /// A header's value by its lower case name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Why no response came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The host name did not resolve.
    Dns,
    /// The connection was refused or failed.
    Connect,
    /// The TLS handshake or certificate check failed.
    Tls,
    /// The request took longer than its timeout.
    Timeout,
    /// The response was larger than the cap.
    TooLarge,
    /// curl could not be started.
    Spawn,
    /// Any other curl failure, by exit code.
    Other(i32),
}

impl Failure {
    /// The failure for the user, without anything from the request.
    pub fn describe(self, timeout: Duration) -> String {
        match self {
            Failure::Dns => "the host name did not resolve (DNS)".into(),
            Failure::Connect => "could not connect".into(),
            Failure::Tls => "the TLS handshake or certificate check failed".into(),
            Failure::Timeout => format!("timed out after {} s", timeout.as_secs()),
            Failure::TooLarge => "the response is over the size cap".into(),
            Failure::Spawn => "curl could not be started (is it on PATH?)".into(),
            Failure::Other(c) => format!("curl failed (exit {c})"),
        }
    }
}

/// Sends a request.
pub trait Transport {
    /// Sends `r` and returns the response, whatever its status.
    ///
    /// # Errors
    ///
    /// A [`Failure`] when no response came back.
    fn send(&self, r: &Request<'_>) -> Result<Response, Failure>;
}

/// The curl transport.
pub struct Curl {
    /// The curl binary.
    pub exe: PathBuf,
}

impl Default for Curl {
    fn default() -> Curl {
        Curl { exe: crate::models::default_curl() }
    }
}

/// A value quoted for a curl config file.
fn quoted(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' | '\r' | '\t' => o.push(' '),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The curl config a request is sent as (stdin, never argv or a file).
pub(crate) fn curl_config(r: &Request<'_>) -> String {
    let proto = if r.url.starts_with("https://") { "=https" } else { "=http" };
    let mut c = String::new();
    c.push_str("silent\nshow-error\ninclude\ngloboff\n");
    c.push_str(&format!("request = {}\n", r.write.map_or(r.method.name(), |w| w.method.name())));
    c.push_str(&format!("proto = {}\n", quoted(proto)));
    c.push_str(&format!("connect-timeout = {}\n", r.timeout.as_secs().clamp(1, 30)));
    c.push_str(&format!("max-time = {}\n", r.timeout.as_secs().max(1)));
    c.push_str(&format!("max-filesize = {}\n", r.max_bytes));
    c.push_str(&format!("url = {}\n", quoted(r.url)));
    c.push_str("header = \"Accept: application/json\"\n");
    c.push_str("header = \"User-Agent: atlas-track\"\n");
    if let Some(w) = r.write {
        // The body rides in the same stdin config, never argv or a file; a
        // leading `@` would make curl read a file, so only JSON objects go.
        if w.body.starts_with('{') {
            c.push_str("header = \"Content-Type: application/json\"\n");
            c.push_str(&format!("data-binary = {}\n", quoted(w.body)));
        }
    }
    if let Some(a) = r.auth {
        c.push_str(&format!("header = {}\n", quoted(&format!("Authorization: {}", a.expose()))));
    }
    c
}

/// Maps curl's exit code.
fn failure(code: i32) -> Failure {
    match code {
        6 | 5 => Failure::Dns,
        7 | 52 | 55 | 56 => Failure::Connect,
        28 => Failure::Timeout,
        35 | 51 | 53 | 54 | 58 | 59 | 60 | 64 | 66 | 77 | 80 | 82 | 83 | 90 | 91 => Failure::Tls,
        63 => Failure::TooLarge,
        c => Failure::Other(c),
    }
}

/// Splits curl's `--include` output into the final status, headers and
/// body (an interim `100 Continue` or a proxy's `Connection established`
/// block is skipped).
pub(crate) fn parse_included(out: &[u8]) -> Option<Response> {
    let mut rest = out;
    loop {
        let end = rest
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| (p, 4))
            .or_else(|| rest.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2)))?;
        let head = String::from_utf8_lossy(&rest[..end.0]).into_owned();
        let body = &rest[end.0 + end.1..];
        let mut lines = head.lines();
        let status_line = lines.next()?;
        if !status_line.starts_with("HTTP/") {
            return None;
        }
        let status: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
        if body.starts_with(b"HTTP/") && (status < 200 || status_line.to_ascii_lowercase().contains("established")) {
            rest = body;
            continue;
        }
        let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
        return Some(Response { status, headers, body: body.to_vec() });
    }
}

impl Transport for Curl {
    fn send(&self, r: &Request<'_>) -> Result<Response, Failure> {
        let mut child = Command::new(&self.exe)
            .args(["-q", "-K", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| Failure::Spawn)?;
        {
            let config = Secret::new(curl_config(r));
            let mut stdin = child.stdin.take().ok_or(Failure::Spawn)?;
            // A write error shows as curl's own failure below.
            let _ = stdin.write_all(config.expose().as_bytes());
        }
        let out = child.wait_with_output().map_err(|_| Failure::Spawn)?;
        match out.status.code() {
            Some(0) => parse_included(&out.stdout).ok_or(Failure::Other(0)),
            Some(c) => Err(failure(c)),
            None => Err(Failure::Other(-1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_goes_to_stdin_quoted_and_the_method_is_get() {
        let s = Secret::new("Basic a\"b\\c".into());
        let r = Request {
            method: Method::Get,
            write: None,
            url: "https://x.atlassian.net/rest/api/3/myself",
            auth: Some(&s),
            timeout: Duration::from_secs(5),
            max_bytes: 10,
        };
        let c = curl_config(&r);
        assert!(c.contains("request = GET\n"));
        assert!(c.contains("header = \"Authorization: Basic a\\\"b\\\\c\"\n"), "{c}");
        assert!(c.contains("proto = \"=https\"\n"));
        assert!(!c.contains("location"), "curl must not follow redirects itself");
    }

    #[test]
    fn included_output_parses_past_interim_blocks() {
        let out = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 429 Too Many Requests\r\nRetry-After: 3\r\nContent-Type: application/json\r\n\r\n{\"a\":1}";
        let r = parse_included(out).unwrap();
        assert_eq!((r.status, r.header("retry-after"), r.body.as_slice()), (429, Some("3"), &b"{\"a\":1}"[..]));
        assert!(parse_included(b"garbage").is_none());
    }
}
