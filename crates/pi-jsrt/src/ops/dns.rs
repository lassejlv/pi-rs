//! A small DNS stub resolver (RFC 1035 wire format over UDP, TCP when truncated) for
//! `node:dns` resolve*() and reverse(), like Node's c-ares: the servers come from
//! `/etc/resolv.conf` unless `setServers` set others; `options timeout:`/`attempts:` apply.
//! Also `getnameinfo` for `lookupService`.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use boa_engine::object::builtins::JsArray;
use boa_engine::{Context, JsArgs, JsResult, JsValue};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::engine::{self, OpFn, arg_f64, arg_string};
use crate::error::{Error, JsException};

fn rrtype_code(name: &str) -> Option<u16> {
    Some(match name {
        "A" => 1,
        "NS" => 2,
        "CNAME" => 5,
        "SOA" => 6,
        "PTR" => 12,
        "MX" => 15,
        "TXT" => 16,
        "AAAA" => 28,
        "SRV" => 33,
        "NAPTR" => 35,
        "TLSA" => 52,
        "ANY" => 255,
        "CAA" => 257,
        _ => return None,
    })
}

fn rrtype_name(code: u16) -> &'static str {
    match code {
        1 => "A",
        2 => "NS",
        5 => "CNAME",
        6 => "SOA",
        12 => "PTR",
        15 => "MX",
        16 => "TXT",
        28 => "AAAA",
        33 => "SRV",
        35 => "NAPTR",
        52 => "TLSA",
        257 => "CAA",
        _ => "",
    }
}

fn encode_query(id: u16, name: &str, qtype: u16) -> Result<Vec<u8>, &'static str> {
    let mut q = Vec::with_capacity(32 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00]); // RD
    q.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
    let trimmed = name.trim_end_matches('.');
    if !trimmed.is_empty() {
        for label in trimmed.split('.') {
            if label.is_empty() || label.len() > 63 {
                return Err("EBADNAME");
            }
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
    }
    q.push(0);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes());
    Ok(q)
}

struct Reader<'a> {
    msg: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Option<u8> {
        let b = *self.msg.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes([self.u8()?, self.u8()?]))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes([self.u8()?, self.u8()?, self.u8()?, self.u8()?]))
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.msg.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(s)
    }
    /// A (possibly compressed) domain name.
    fn name(&mut self) -> Option<String> {
        let mut labels: Vec<String> = Vec::new();
        let mut pos = self.pos;
        let mut jumped = false;
        let mut hops = 0;
        loop {
            let len = *self.msg.get(pos)? as usize;
            if len & 0xc0 == 0xc0 {
                let ptr = ((len & 0x3f) << 8) | *self.msg.get(pos + 1)? as usize;
                if !jumped {
                    self.pos = pos + 2;
                }
                jumped = true;
                pos = ptr;
                hops += 1;
                if hops > 64 {
                    return None;
                }
                continue;
            }
            if len == 0 {
                if !jumped {
                    self.pos = pos + 1;
                }
                break;
            }
            let label = self.msg.get(pos + 1..pos + 1 + len)?;
            labels.push(String::from_utf8_lossy(label).into_owned());
            pos += 1 + len;
        }
        Some(labels.join("."))
    }
    /// Character-strings filling `end` (TXT).
    fn strings(&mut self, end: usize) -> Option<Vec<String>> {
        let mut out = Vec::new();
        while self.pos < end {
            let n = self.u8()? as usize;
            out.push(String::from_utf8_lossy(self.bytes(n)?).into_owned());
        }
        Some(out)
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        Some(String::from_utf8_lossy(self.bytes(n)?).into_owned())
    }
}

/// Parse a response: `(rcode, truncated, answers)`; answers as Node-shaped JSON with `type`/`ttl`.
fn parse_response(msg: &[u8], id: u16) -> Option<(u8, bool, Vec<Value>)> {
    let mut r = Reader { msg, pos: 0 };
    if r.u16()? != id {
        return None;
    }
    let flags = r.u16()?;
    let truncated = flags & 0x0200 != 0;
    let rcode = (flags & 0x000f) as u8;
    let qd = r.u16()?;
    let an = r.u16()?;
    r.u16()?;
    r.u16()?;
    for _ in 0..qd {
        r.name()?;
        r.u16()?;
        r.u16()?;
    }
    let mut answers = Vec::new();
    for _ in 0..an {
        let _owner = r.name()?;
        let rtype = r.u16()?;
        let _class = r.u16()?;
        let ttl = r.u32()?;
        let len = r.u16()? as usize;
        let start = r.pos;
        let end = start + len;
        let v = match rtype {
            1 if len == 4 => {
                let b = r.bytes(4)?;
                json!({"type": "A", "address": IpAddr::from([b[0], b[1], b[2], b[3]]).to_string(), "ttl": ttl})
            }
            28 if len == 16 => {
                let b: [u8; 16] = r.bytes(16)?.try_into().ok()?;
                json!({"type": "AAAA", "address": IpAddr::from(b).to_string(), "ttl": ttl})
            }
            2 | 5 | 12 => json!({"type": rrtype_name(rtype), "value": r.name()?, "ttl": ttl}),
            15 => {
                let priority = r.u16()?;
                json!({"type": "MX", "priority": priority, "exchange": r.name()?, "ttl": ttl})
            }
            16 => json!({"type": "TXT", "entries": r.strings(end)?, "ttl": ttl}),
            33 => {
                let priority = r.u16()?;
                let weight = r.u16()?;
                let port = r.u16()?;
                json!({"type": "SRV", "priority": priority, "weight": weight, "port": port, "name": r.name()?, "ttl": ttl})
            }
            6 => {
                let nsname = r.name()?;
                let hostmaster = r.name()?;
                json!({"type": "SOA", "nsname": nsname, "hostmaster": hostmaster, "serial": r.u32()?, "refresh": r.u32()?,
                       "retry": r.u32()?, "expire": r.u32()?, "minttl": r.u32()?, "ttl": ttl})
            }
            35 => {
                let order = r.u16()?;
                let preference = r.u16()?;
                let flags = r.string()?;
                let service = r.string()?;
                let regexp = r.string()?;
                let replacement = r.name()?;
                json!({"type": "NAPTR", "order": order, "preference": preference, "flags": flags, "service": service,
                       "regexp": regexp, "replacement": replacement, "ttl": ttl})
            }
            257 => {
                let critical = r.u8()?;
                let tag = r.string()?;
                let value = String::from_utf8_lossy(r.bytes(end.saturating_sub(r.pos))?).into_owned();
                json!({"type": "CAA", "critical": critical, "tag": tag, "value": value, "ttl": ttl})
            }
            52 => {
                let usage = r.u8()?;
                let selector = r.u8()?;
                let mtype = r.u8()?;
                let data = r.bytes(end.saturating_sub(r.pos))?.to_vec();
                json!({"type": "TLSA", "certUsage": usage, "selector": selector, "match": mtype, "data": data, "ttl": ttl})
            }
            _ => Value::Null,
        };
        r.pos = end;
        if !v.is_null() {
            answers.push(v);
        }
    }
    Some((rcode, truncated, answers))
}

fn dns_error(code: &str, syscall: &str, hostname: &str) -> Error {
    Error::Js(JsException {
        name: "Error".into(),
        message: format!("{syscall} {code} {hostname}"),
        stack: None,
        code: Some(code.into()),
    })
}

async fn exchange_udp(server: SocketAddr, query: &[u8], timeout: Duration) -> std::io::Result<Vec<u8>> {
    let bind: SocketAddr = if server.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
    let sock = tokio::net::UdpSocket::bind(bind).await?;
    sock.connect(server).await?;
    sock.send(query).await?;
    let mut buf = vec![0u8; 65535];
    let n = tokio::time::timeout(timeout, sock.recv(&mut buf)).await.map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
    buf.truncate(n);
    Ok(buf)
}

async fn exchange_tcp(server: SocketAddr, query: &[u8], timeout: Duration) -> std::io::Result<Vec<u8>> {
    let run = async {
        let mut s = tokio::net::TcpStream::connect(server).await?;
        let mut framed = (query.len() as u16).to_be_bytes().to_vec();
        framed.extend_from_slice(query);
        s.write_all(&framed).await?;
        let mut len = [0u8; 2];
        s.read_exact(&mut len).await?;
        let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
        s.read_exact(&mut buf).await?;
        Ok(buf)
    };
    tokio::time::timeout(timeout, run).await.map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
}

/// `ops.dnsQuery(name, rrtype, servers, timeoutMs, tries) -> Promise<answers>`: rejects with
/// c-ares error codes (ENOTFOUND, ENODATA, ESERVFAIL, EREFUSED, ETIMEOUT, ECONNREFUSED, ...).
fn op_dns_query(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_string(args, 0, ctx)?;
    let rrtype = arg_string(args, 1, ctx)?;
    let syscall = arg_string(args, 5, ctx)?;
    let mut servers = Vec::new();
    if let Some(obj) = args.get_or_undefined(2).as_object()
        && let Ok(arr) = JsArray::from_object(obj)
    {
        for i in 0..arr.length(ctx)? {
            let s = arr.get(i, ctx)?.to_string(ctx)?.to_std_string_lossy();
            if let Ok(a) = s.parse::<SocketAddr>() {
                servers.push(a);
            } else if let Ok(ip) = s.parse::<IpAddr>() {
                servers.push(SocketAddr::new(ip, 53));
            }
        }
    }
    let timeout = Duration::from_millis(arg_f64(args, 3, ctx)?.max(1.0) as u64);
    let tries = arg_f64(args, 4, ctx)?.max(1.0) as usize;
    let qtype = rrtype_code(&rrtype).unwrap_or(1);
    Ok(engine::spawn_result(
        ctx,
        async move {
            if servers.is_empty() {
                return Err(dns_error("ECONNREFUSED", &syscall, &name));
            }
            let mut last = "ETIMEOUT";
            for _ in 0..tries {
                for server in &servers {
                    let id: u16 = rand::random();
                    let query = encode_query(id, &name, qtype).map_err(|code| dns_error(code, &syscall, &name))?;
                    let mut reply = match exchange_udp(*server, &query, timeout).await {
                        Ok(r) => r,
                        Err(e) => {
                            last = if e.kind() == std::io::ErrorKind::TimedOut { "ETIMEOUT" } else { "ECONNREFUSED" };
                            continue;
                        }
                    };
                    let Some(mut parsed) = parse_response(&reply, id) else {
                        last = "EBADRESP";
                        continue;
                    };
                    if parsed.1
                        && let Ok(r) = exchange_tcp(*server, &query, timeout).await
                    {
                        reply = r;
                        if let Some(p) = parse_response(&reply, id) {
                            parsed = p;
                        }
                    }
                    let (rcode, _, answers) = parsed;
                    return match rcode {
                        0 if answers.is_empty() => Err(dns_error("ENODATA", &syscall, &name)),
                        0 => Ok(Value::Array(answers)),
                        1 => Err(dns_error("EFORMERR", &syscall, &name)),
                        2 => Err(dns_error("ESERVFAIL", &syscall, &name)),
                        3 => Err(dns_error("ENOTFOUND", &syscall, &name)),
                        4 => Err(dns_error("ENOTIMP", &syscall, &name)),
                        5 => Err(dns_error("EREFUSED", &syscall, &name)),
                        _ => Err(dns_error("EBADRESP", &syscall, &name)),
                    };
                }
            }
            Err(dns_error(last, &syscall, &name))
        },
        |v, ctx| crate::convert::from_json(&v, ctx),
    ))
}

/// `ops.dnsLookupService(address, port) -> Promise<[hostname, service]>` (getnameinfo).
fn op_dns_lookup_service(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let address = arg_string(args, 0, ctx)?;
    let port = arg_f64(args, 1, ctx)? as u16;
    Ok(engine::spawn_result(
        ctx,
        async move {
            let addr = address.parse::<IpAddr>().map_err(|_| dns_error("EINVAL", "getnameinfo", &address))?;
            let sa = SocketAddr::new(addr, port);
            let addr2 = address.clone();
            tokio::task::spawn_blocking(move || {
                let (storage, len) = sockaddr_of(&sa);
                let mut host = [0 as libc::c_char; 1025];
                let mut serv = [0 as libc::c_char; 32];
                // SAFETY: buffers are valid for their lengths; storage holds a sockaddr of `len`.
                let r = unsafe {
                    libc::getnameinfo(
                        (&storage as *const libc::sockaddr_storage).cast(),
                        len,
                        host.as_mut_ptr(),
                        host.len() as libc::socklen_t,
                        serv.as_mut_ptr(),
                        serv.len() as libc::socklen_t,
                        libc::NI_NAMEREQD,
                    )
                };
                if r != 0 {
                    return Err(dns_error("ENOTFOUND", "getnameinfo", &addr2));
                }
                // SAFETY: getnameinfo NUL-terminates on success.
                let cstr = |b: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(b.as_ptr()) }.to_string_lossy().into_owned();
                Ok((cstr(&host), cstr(&serv)))
            })
            .await
            .map_err(|_| dns_error("EAI_FAIL", "getnameinfo", &address))?
        },
        |(host, service), ctx| Ok(JsArray::from_iter([engine::js_str(&host), engine::js_str(&service)], ctx).into()),
    ))
}

fn sockaddr_of(sa: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: sockaddr_storage is plain data large enough for both families.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match sa {
        SocketAddr::V4(v4) => {
            let sin = libc::sockaddr_in {
                #[cfg(any(
                    target_vendor = "apple",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "netbsd",
                    target_os = "dragonfly"
                ))]
                sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(v4.ip().octets()) },
                sin_zero: [0; 8],
            };
            // SAFETY: writing a sockaddr_in into the storage.
            unsafe { std::ptr::write((&mut storage as *mut libc::sockaddr_storage).cast(), sin) };
            (storage, std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t)
        }
        SocketAddr::V6(v6) => {
            let sin6 = libc::sockaddr_in6 {
                #[cfg(any(
                    target_vendor = "apple",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "netbsd",
                    target_os = "dragonfly"
                ))]
                sin6_len: std::mem::size_of::<libc::sockaddr_in6>() as u8,
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo(),
                sin6_addr: libc::in6_addr { s6_addr: v6.ip().octets() },
                sin6_scope_id: v6.scope_id(),
            };
            // SAFETY: writing a sockaddr_in6 into the storage.
            unsafe { std::ptr::write((&mut storage as *mut libc::sockaddr_storage).cast(), sin6) };
            (storage, std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t)
        }
    }
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[("dnsQuery", 6, op_dns_query), ("dnsLookupService", 2, op_dns_lookup_service)];
