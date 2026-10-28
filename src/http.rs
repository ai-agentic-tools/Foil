//! A small HTTP/1.1 server toolkit for `foil serve`: read one request,
//! write one response, close. Every response says `Connection: close`, so
//! there is no keep-alive bookkeeping, and a streamed body (server-sent
//! events, a file) simply ends when the connection does.

use std::io::{self, BufRead, BufReader, Read, Write};

use serde_json::Value;

/// Longest request line or header line accepted.
const MAX_LINE: usize = 64 * 1024;
/// Most headers accepted in one request.
const MAX_HEADERS: usize = 100;

pub struct Request {
    pub method: String,
    /// The path, percent-decoded, without the query.
    pub path: String,
    /// The path exactly as sent, with the query.
    pub target: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
}

impl Request {
    /// Read a request's head (request line and headers) from `reader`.
    pub fn read(reader: &mut impl BufRead) -> io::Result<Request> {
        let line = read_line(reader)?;
        let mut parts = line.split(' ');
        let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(bad("malformed request line"));
        };
        if !version.starts_with("HTTP/1.") {
            return Err(bad("only HTTP/1.x is supported"));
        }
        let mut headers = Vec::new();
        loop {
            let line = read_line(reader)?;
            if line.is_empty() {
                break;
            }
            if headers.len() == MAX_HEADERS {
                return Err(bad("too many headers"));
            }
            let Some((name, value)) = line.split_once(':') else {
                return Err(bad("malformed header"));
            };
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
        let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
        Ok(Request {
            method: method.to_owned(),
            path: percent_decode(raw_path),
            target: target.to_owned(),
            query: parse_query(raw_query),
            headers,
        })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The first value of query parameter `name`.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// Every value of query parameter `name` (`?path=a&path=b`).
    pub fn params(&self, name: &str) -> Vec<&str> {
        self.query.iter().filter(|(n, _)| n == name).map(|(_, v)| v.as_str()).collect()
    }

    /// Read the body (`Content-Length` or chunked), refusing more than `limit` bytes.
    pub fn body(&self, reader: &mut impl BufRead, limit: usize) -> io::Result<Vec<u8>> {
        let mut body = Vec::new();
        if self.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
            loop {
                let size_line = read_line(reader)?;
                let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
                    .map_err(|_| bad("malformed chunk size"))?;
                if size == 0 {
                    // Trailers, then the blank line that ends them.
                    while !read_line(reader)?.is_empty() {}
                    return Ok(body);
                }
                if body.len() + size > limit {
                    return Err(too_large());
                }
                let start = body.len();
                body.resize(start + size, 0);
                reader.read_exact(&mut body[start..])?;
                read_line(reader)?;
            }
        }
        let length = match self.header("content-length") {
            Some(v) => v.parse::<usize>().map_err(|_| bad("malformed Content-Length"))?,
            None => 0,
        };
        if length > limit {
            return Err(too_large());
        }
        body.resize(length, 0);
        reader.read_exact(&mut body)?;
        Ok(body)
    }

    /// The body as JSON; an empty body is `null`.
    pub fn json(&self, reader: &mut impl BufRead, limit: usize) -> io::Result<Value> {
        let body = self.body(reader, limit)?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&body).map_err(|e| bad(&format!("invalid JSON body: {e}")))
    }
}

/// An error that becomes a response: status, a code for programs, a message for people.
#[derive(Debug)]
pub struct Error {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl Error {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Error {
        Error { status, code, message: message.into() }
    }
    pub fn bad(message: impl Into<String>) -> Error {
        Error::new(400, "INVALID_REQUEST", message)
    }
    pub fn not_found(message: impl Into<String>) -> Error {
        Error::new(404, "NOT_FOUND", message)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        match err.kind() {
            io::ErrorKind::NotFound => Error::not_found(err.to_string()),
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => Error::bad(err.to_string()),
            io::ErrorKind::PermissionDenied => Error::new(403, "PERMISSION_DENIED", err.to_string()),
            io::ErrorKind::AlreadyExists => Error::new(409, "ALREADY_EXISTS", err.to_string()),
            _ if err.to_string() == TOO_LARGE => Error::new(413, "TOO_LARGE", TOO_LARGE),
            _ => Error::new(500, "INTERNAL_ERROR", err.to_string()),
        }
    }
}

const TOO_LARGE: &str = "request body too large";

fn too_large() -> io::Error {
    io::Error::other(TOO_LARGE)
}

pub fn bad(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

fn read_line(reader: &mut impl BufRead) -> io::Result<String> {
    let mut line = Vec::new();
    let read = reader.take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line)?;
    if read == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"));
    }
    if line.len() > MAX_LINE {
        return Err(bad("line too long"));
    }
    while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        line.pop();
    }
    String::from_utf8(line).map_err(|_| bad("request head is not UTF-8"))
}

/// Decode `%XX` escapes (and `+` as space when `plus`), keeping bad escapes as they are.
fn decode(raw: &str, plus: bool) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                        continue;
                    }
                    _ => out.push(b'%'),
                }
            }
            b'+' if plus => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn percent_decode(raw: &str) -> String {
    decode(raw, false)
}

pub fn parse_query(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k, true), decode(v, true))
        })
        .collect()
}

/// Percent-encode a query value.
pub fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Write a status line and headers; the body follows (and ends at close).
pub fn head(out: &mut impl Write, status: u16, headers: &[(&str, &str)]) -> io::Result<()> {
    let mut text = format!("HTTP/1.1 {status} {}\r\nConnection: close\r\n", reason(status));
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str("\r\n");
    out.write_all(text.as_bytes())
}

pub fn send(out: &mut impl Write, status: u16, content_type: &str, body: &[u8], extra: &[(&str, &str)]) -> io::Result<()> {
    let length = body.len().to_string();
    let mut headers = vec![("Content-Type", content_type), ("Content-Length", length.as_str())];
    headers.extend_from_slice(extra);
    head(out, status, &headers)?;
    out.write_all(body)?;
    out.flush()
}

pub fn json(out: &mut impl Write, status: u16, value: &Value) -> io::Result<()> {
    send(out, status, "application/json", value.to_string().as_bytes(), &[])
}

pub fn empty(out: &mut impl Write, status: u16) -> io::Result<()> {
    send(out, status, "text/plain", b"", &[])
}

/// The error body both OpenSandbox APIs use: `{"code", "message"}`.
pub fn error(out: &mut impl Write, err: &Error) -> io::Result<()> {
    json(out, err.status, &serde_json::json!({ "code": err.code, "message": err.message }))
}

/// One request on `conn`: parse it, let `handle` answer, and turn an
/// `Err` from `handle` into an error response.
pub fn serve_one<C: Read + Write>(
    conn: C,
    handle: impl FnOnce(&Request, &mut BufReader<C>) -> Result<(), Error>,
) -> io::Result<()> {
    let mut reader = BufReader::new(conn);
    let request = match Request::read(&mut reader) {
        Ok(r) => r,
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
        Err(err) => return error(reader.get_mut(), &Error::bad(err.to_string())),
    };
    match handle(&request, &mut reader) {
        Ok(()) => Ok(()),
        Err(err) => error(reader.get_mut(), &err),
    }
}

/// RFC 3339 time (UTC, seconds) for a Unix time.
pub fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Parse the `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)` form of RFC 3339.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b't' || b[10] == b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| text.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, s) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &text[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        rest = frac.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 && (rest.starts_with('+') || rest.starts_with('-')) => {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            sign * (rest[1..3].parse::<i64>().ok()? * 3600 + rest[4..6].parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s - offset)
}

// Howard Hinnant's algorithms between days since 1970-01-01 and dates.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_parsed() {
        let raw = b"POST /files/info?path=%2Fa%20b&path=c+d&x HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\n\r\nhello";
        let mut reader = BufReader::new(&raw[..]);
        let req = Request::read(&mut reader).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/files/info");
        assert_eq!(req.params("path"), ["/a b", "c d"]);
        assert_eq!(req.param("x"), Some(""));
        assert_eq!(req.header("content-length"), Some("5"));
        assert_eq!(req.body(&mut reader, 10).unwrap(), b"hello");
    }

    #[test]
    fn chunked_bodies_and_limits() {
        let raw = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n";
        let mut reader = BufReader::new(&raw[..]);
        let req = Request::read(&mut reader).unwrap();
        assert_eq!(req.body(&mut reader, 10).unwrap(), b"abcde");
        let raw = b"POST / HTTP/1.1\r\nContent-Length: 50\r\n\r\n";
        let mut reader = BufReader::new(&raw[..]);
        let req = Request::read(&mut reader).unwrap();
        assert!(req.body(&mut reader, 10).is_err());
    }

    #[test]
    fn odd_escapes_are_kept() {
        assert_eq!(percent_decode("/a%2"), "/a%2");
        assert_eq!(percent_decode("%zz%41"), "%zzA");
        assert_eq!(encode("/a b&c"), "/a%20b%26c");
    }

    #[test]
    fn times_round_trip() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(parse_rfc3339("2023-11-14T22:13:20Z"), Some(1_700_000_000));
        assert_eq!(parse_rfc3339("2023-11-14T23:13:20.123+01:00"), Some(1_700_000_000));
        assert_eq!(parse_rfc3339("2023-11-14"), None);
        for t in [-86_400 * 400, 951_782_400, 4_102_444_800] {
            assert_eq!(parse_rfc3339(&rfc3339(t)), Some(t));
        }
    }
}
