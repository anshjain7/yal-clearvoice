//! One HTTP(S) request at a time, for the after-call pass (aftercall.rs):
//! Sarvam's batch speech-to-text API and the storage links it hands out.
//! TLS goes through Windows' own schannel (native-tls), as for the live
//! transcript. Each request opens its own connection and closes it.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Longest a connection may sit without data before the request fails. Long
/// enough for a whole call's audio going up on a slow office line.
const TIMEOUT: Duration = Duration::from_secs(120);

/// A reply: its status and body.
#[derive(Debug, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Sends `method url` with `headers` and `body` (Content-Length is added),
/// and returns the reply.
pub fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<Reply, String> {
    let (tls, host, port, target) = split(url)?;
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host} not found"))?;
    let tcp = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(TIMEOUT))
        .map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(TIMEOUT))
        .map_err(|e| e.to_string())?;
    let mut reply = Vec::new();
    let mut talk = |s: &mut dyn ReadWrite| -> Result<(), String> {
        s.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
        s.write_all(body).map_err(|e| e.to_string())?;
        s.flush().map_err(|e| e.to_string())?;
        s.read_to_end(&mut reply).map_err(|e| e.to_string())?;
        Ok(())
    };
    if tls {
        let connector = native_tls::TlsConnector::new().map_err(|e| e.to_string())?;
        let mut s = connector.connect(&host, tcp).map_err(|e| e.to_string())?;
        talk(&mut s)?;
    } else {
        let mut s = tcp;
        talk(&mut s)?;
    }
    parse(&reply)
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// An http:// or https:// address as (tls, host, port, path with its query).
fn split(url: &str) -> Result<(bool, String, u16, String), String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("not an http(s) address: {url}"));
    };
    let cut = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, target) = rest.split_at(cut);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| format!("bad port in {url}"))?),
        None => (authority, if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(format!("no host in {url}"));
    }
    let target = match target {
        "" => "/".to_string(),
        t if t.starts_with('?') => format!("/{t}"),
        t => t.to_string(),
    };
    Ok((tls, host.to_string(), port, target))
}

/// An HTTP/1.1 reply: its status, and its body by Content-Length, chunked
/// transfer encoding, or to the end of the connection.
fn parse(reply: &[u8]) -> Result<Reply, String> {
    let end = reply
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no reply")?;
    let head = String::from_utf8_lossy(&reply[..end]);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("bad reply")?;
    let header = |name: &str| {
        head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    };
    let rest = &reply[end + 4..];
    let body = if header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        dechunk(rest)?
    } else if let Some(len) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        rest[..len.min(rest.len())].to_vec()
    } else {
        rest.to_vec()
    };
    Ok(Reply { status, body })
}

/// A chunked body put back together.
fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let line_end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("cut-off chunked reply")?;
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| "bad chunk size")?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        if rest.len() < size {
            return Err("cut-off chunked reply".into());
        }
        body.extend_from_slice(&rest[..size]);
        rest = rest.get(size + 2..).ok_or("cut-off chunked reply")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn addresses_keep_their_path_and_query() {
        let s = |u| split(u).unwrap();
        assert_eq!(
            s("https://api.sarvam.ai/speech-to-text/job/v1"),
            (
                true,
                "api.sarvam.ai".into(),
                443,
                "/speech-to-text/job/v1".into()
            )
        );
        assert_eq!(
            s("https://x.blob.core.windows.net/c/0.wav?sv=1&sig=a%2Fb"),
            (
                true,
                "x.blob.core.windows.net".into(),
                443,
                "/c/0.wav?sv=1&sig=a%2Fb".into()
            )
        );
        assert_eq!(s("http://h:8080"), (false, "h".into(), 8080, "/".into()));
        assert_eq!(s("http://h?q=1"), (false, "h".into(), 80, "/?q=1".into()));
        assert!(split("ftp://h").is_err() && split("https:///x").is_err());
    }

    #[test]
    fn replies_are_read_by_length_chunks_or_to_the_end() {
        let r = parse(b"HTTP/1.1 202 Accepted\r\nContent-Length: 3\r\n\r\nabcdef").unwrap();
        assert_eq!(
            r,
            Reply {
                status: 202,
                body: b"abc".to_vec()
            }
        );
        let chunked = b"HTTP/1.1 200 OK\r\ntransfer-encoding: Chunked\r\n\r\n\
                        4\r\n{\"a\"\r\n6;ext=1\r\n: 12}\n\r\n0\r\n\r\n";
        assert_eq!(parse(chunked).unwrap().body, b"{\"a\": 12}\n");
        assert_eq!(parse(b"HTTP/1.0 404 x\r\n\r\nnope").unwrap().body, b"nope");
        assert!(parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nab").is_err());
    }

    #[test]
    fn request_sends_method_headers_and_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/up?sig=1", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while !got.ends_with(b"hello") {
                let n = s.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            s.write_all(
                b"HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n",
            )
            .unwrap();
            String::from_utf8(got).unwrap()
        });
        let reply = request("PUT", &url, &[("x-ms-blob-type", "BlockBlob")], b"hello").unwrap();
        assert_eq!(
            reply,
            Reply {
                status: 201,
                body: b"ok".to_vec()
            }
        );
        let sent = server.join().unwrap();
        assert!(sent.starts_with("PUT /up?sig=1 HTTP/1.1\r\n"), "{sent}");
        assert!(
            sent.contains("Content-Length: 5\r\n")
                && sent.contains("x-ms-blob-type: BlockBlob\r\n")
        );
    }
}
