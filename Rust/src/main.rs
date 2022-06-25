use async_std::fs::File;
use async_std::io::BufReader;
use async_std::io::Error;
use async_std::io::ReadExt;
use async_std::io::Result;
use async_std::io::Stderr;
use async_std::io::prelude::BufReadExt;
use async_std::io::stderr;
use async_std::net::Ipv6Addr;
use async_std::net::Shutdown;
use async_std::net::SocketAddr;
use async_std::net::SocketAddrV6;
use async_std::net::TcpListener;
use async_std::net::TcpStream;
use async_std::sync::Mutex;
use futures_lite::AsyncWriteExt;
use lazy_static::lazy_static;
use regex::bytes;
use std::collections::HashMap;
use std::fmt::Display;
use std::io::ErrorKind;
use std::ops::DerefMut;
use std::process::exit;

#[tokio::main]
async fn main() {
    let mut local = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0);

    match TcpListener::bind(local).await {
        Err(err) => {
            complain("listen", local, &err).await;
            exit(1);
        }
        Ok(listener) => {
            if let Ok(addr) = listener.local_addr() {
                local.set_port(addr.port());
            }

            loop {
                tokio::spawn(handle_accept(listener.accept().await, local));
            }
        }
    }
}

async fn handle_accept(accepted: Result<(TcpStream, SocketAddr)>, local: SocketAddrV6) {
    match accepted {
        Err(err) => { complain("accept", local, &err).await; }
        Ok(client) => {
            let (stream, remote) = client;
            let mut buf = BufReader::new(stream);

            match read_req(&mut buf).await {
                Err(err) => { complain("read", remote, &err).await; }
                Ok(or) => {
                    match or {
                        None => { respond(&mut buf, remote, 400, "Bad Request", None, false).await; }
                        Some(req) => { handle_request(&mut buf, remote, req).await; }
                    }
                }
            }

            if let Err(err) = buf.get_mut().close().await {
                complain("close", remote, &err).await;
            }
        }
    }
}

struct Request {
    method: String,
    uri: String,
    http09: bool,
    headers: HashMap<String, Vec<String>>,
}

lazy_static! {
    static ref HTTP_HEADLINE: bytes::Regex = bytes::Regex::new(r"\A(\S+) (\S+) HTTP/\d+\.\d+\r\n\z").unwrap();
    static ref HTTP_HEADER: bytes::Regex = bytes::Regex::new(r"\A(\S+?): *(.*)\r\n\z").unwrap();
    static ref HTTP09: bytes::Regex = bytes::Regex::new(r"\A(GET) (\S+)\r\n\z").unwrap();
}

async fn read_req(stream: &mut BufReader<TcpStream>) -> Result<Option<Request>> {
    let mut buf = Vec::<u8>::new();

    stream.read_until(b'\n', &mut buf).await?;

    match HTTP_HEADLINE.captures(&*buf) {
        Some(captures) => {
            let mut req = Request {
                method: String::new(),
                uri: String::new(),
                http09: false,
                headers: Default::default(),
            };

            match match2str(captures.get(1)) {
                None => { return Ok(None); }
                Some(s) => { req.method = s; }
            }

            match match2str(captures.get(2)) {
                None => { return Ok(None); }
                Some(s) => { req.uri = s; }
            }

            loop {
                buf = Vec::<u8>::new();
                stream.read_until(b'\n', &mut buf).await?;

                match HTTP_HEADER.captures(&*buf) {
                    Some(captures) => {
                        match match2str(captures.get(1)) {
                            None => { return Ok(None); }
                            Some(k) => {
                                match match2str(captures.get(2)) {
                                    None => { return Ok(None); }
                                    Some(v) => {
                                        let lk = k.to_lowercase();

                                        match req.headers.get_mut(&*lk) {
                                            None => { req.headers.insert(lk, vec![v]); }
                                            Some(vv) => { vv.push(v); }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        if buf == b"\r\n" {
                            break;
                        } else {
                            return Ok(None);
                        }
                    }
                }
            }

            Ok(Some(req))
        }
        None => match HTTP09.captures(&*buf) {
            Some(captures) => match match2str(captures.get(2)) {
                None => Ok(None),
                Some(s) => Ok(Some(Request {
                    method: String::from_utf8(captures.get(1).unwrap().as_bytes().to_vec()).unwrap(),
                    uri: s,
                    http09: true,
                    headers: Default::default(),
                }))
            }
            None => Ok(None)
        }
    }
}

fn match2str(om: Option<bytes::Match>) -> Option<String> {
    match om {
        None => None,
        Some(m) => match String::from_utf8(m.as_bytes().to_vec()) {
            Err(_) => None,
            Ok(s) => Some(s),
        }
    }
}

async fn handle_request(from: &mut BufReader<TcpStream>, remote: SocketAddr, req: Request) {
    let serve_body;

    match req.method.as_str() {
        "HEAD" => { serve_body = false; }
        "GET" => { serve_body = true; }
        _ => {
            respond(from, remote, 501, "Not Implemented", None, req.http09).await;
            return;
        }
    }

    let mut uri = req.uri.as_str();

    if !uri.starts_with("/") {
        respond(from, remote, 501, "Not Implemented", None, req.http09).await;
        return;
    }

    let mut depth = 0;

    loop {
        loop {
            match uri.strip_prefix("/") {
                Some(u) => { uri = u; }
                None => { break; }
            }
        }

        if uri.is_empty() {
            break;
        }

        let next = match uri.find('/') {
            None => uri.len(),
            Some(i) => i
        };

        let step = &uri[..next];
        uri = &uri[next..];

        match step {
            "." => {}
            ".." => {
                if depth < 1 {
                    respond(from, remote, 403, "Forbidden", None, req.http09).await;
                    return;
                }

                depth -= 1;
            }
            _ => { depth += 1; }
        }
    }

    let path = ".".to_owned() + &req.uri;

    match File::open(&path).await {
        Ok(mut f) => {
            if serve_body {
                let mut buf = Vec::<u8>::new();

                match f.read_to_end(&mut buf).await {
                    Ok(_) => { respond(from, remote, 200, "OK", Some(buf), req.http09).await; }
                    Err(err) => {
                        complain("read", &path, &err).await;
                        respond(from, remote, 500, "Internal Server Error", None, req.http09).await;
                    }
                }
            } else {
                respond(from, remote, 200, "OK", None, req.http09).await;
            }

            if let Err(err) = f.close().await {
                complain("close", path, &err).await;
            }
        }
        Err(err) => {
            complain("open", path, &err).await;

            match err.kind() {
                ErrorKind::NotFound => respond(from, remote, 404, "Not Found", None, req.http09),
                ErrorKind::PermissionDenied => respond(from, remote, 403, "Forbidden", None, req.http09),
                _ => respond(from, remote, 500, "Internal Server Error", None, req.http09)
            }.await;
        }
    }
}

async fn respond(to: &mut BufReader<TcpStream>, remote: SocketAddr, status: u16, reason: &'static str, body: Option<Vec<u8>>, http09: bool) {
    if http09 && status > 299 && body == None {
        return;
    }

    let stream = to.get_mut();

    if let Err(err) = stream.shutdown(Shutdown::Read) {
        complain("shutdown", remote, &err).await;
    }

    let mut buf = if http09 {
        Vec::<u8>::new()
    } else {
        format!("HTTP/1.0 {} {}\r\n\r\n", status, reason).into_bytes()
    };

    if let Some(mut b) = body {
        buf.append(&mut b);
    }

    match AsyncWriteExt::write_all(stream, &*buf).await {
        Err(err) => { complain("write", remote, &err).await; }
        Ok(_) => {
            match AsyncWriteExt::flush(stream).await {
                Err(err) => { complain("flush", remote, &err).await; }
                Ok(_) => {
                    if let Err(err) = stream.shutdown(Shutdown::Write) {
                        complain("shutdown", remote, &err).await;
                    }
                }
            }
        }
    }
}

lazy_static! {
    static ref STDERR: Mutex<Stderr> = Mutex::new(stderr());
}

async fn complain(op: &'static str, ctx: impl Display, err: &Error) {
    let buf = format!("{} {}: {}\n", op, ctx, err).into_bytes();

    let mut lock = STDERR.lock().await;
    let stderr = lock.deref_mut();
    let _ = AsyncWriteExt::write_all(stderr, &*buf).await;
    let _ = AsyncWriteExt::flush(stderr).await;
}
