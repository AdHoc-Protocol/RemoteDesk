// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! DNS TXT lookup returning byte strings for a key.
//!
//! Queries go over UDP to the public servers [`PUBLIC_DNS`] at once, then, when they do not answer or
//! know of no such record, to all the operating system's configured DNS servers at once, past what
//! fits into UDP over TCP. Server discovery reads `/etc/resolv.conf` on Unix and uses `ipconfig` on
//! Windows. Packet construction and response parsing are tested without network access.

use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token};

/// How long to wait for an answer of each group of servers.
const TIMEOUT: Duration = Duration::from_millis(3000);

/// Public DNS servers that [`value`] asks first. The first [`PUBLIC_DNS_WHOLE`] give a long answer
/// whole. The others are for where those are closed, and are asked only after a third of the time
/// without an answer: they are nearer and faster there, and would come first everywhere, but they
/// drop part of the records of a name and do not say so ( by a check: 17, 7 and 8 of the 24 records
/// of github.com ).
const PUBLIC_DNS: [IpAddr; 7] = [
    IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
    IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
    IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)),
    IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888)),
    IpAddr::V4(Ipv4Addr::new(223, 5, 5, 5)),
    IpAddr::V4(Ipv4Addr::new(119, 29, 29, 29)),
    IpAddr::V4(Ipv4Addr::new(114, 114, 114, 114)),
];

const PUBLIC_DNS_WHOLE: usize = 4;

/// The size of a UDP answer this side takes ( EDNS, RFC 6891 ). A larger size is not asked for, as
/// a UDP answer cut into fragments is often lost on the way.
const UDP_SIZE: u16 = 1232;

/// A name as DNS knows it: without the dots at the end, a label in another alphabet as `xn--...`
/// ( lowercased, without further Unicode mapping ). `None` — the lengths do not fit a DNS name.
fn ascii_name(key: &str) -> Option<String> {
    let key = key.trim_end_matches('.');
    let mut domain = String::with_capacity(key.len());
    for label in key.split('.') {
        if !domain.is_empty() {
            domain.push('.');
        }
        let start = domain.len();
        if label.is_ascii() {
            domain.push_str(label);
        } else {
            if 63 < label.chars().count() {
                return None;
            }
            domain.push_str("xn--");
            punycode(&label.to_lowercase(), &mut domain);
        }
        if domain.len() == start || 63 < domain.len() - start {
            return None;
        }
    }
    (domain.len() <= 253).then_some(domain)
}

/// Appends `label` in Punycode ( RFC 3492 ). The caller keeps a label to 63 characters, so `delta`
/// does not overflow.
fn punycode(label: &str, out: &mut String) {
    fn digit(d: u32) -> char {
        (if d < 26 { b'a' + d as u8 } else { b'0' + (d - 26) as u8 }) as char
    }
    let basic = label.chars().filter(char::is_ascii).count() as u32;
    out.extend(label.chars().filter(char::is_ascii));
    if 0 < basic {
        out.push('-');
    }
    let (mut n, mut delta, mut bias, mut handled) = (128u32, 0u32, 72u32, basic);
    let total = label.chars().count() as u32;
    while handled < total {
        let m = label.chars().map(|c| c as u32).filter(|c| n <= *c).min().unwrap_or(n);
        delta += (m - n) * (handled + 1);
        n = m;
        for c in label.chars().map(|c| c as u32) {
            if c < n {
                delta += 1;
            } else if c == n {
                let (mut q, mut k) = (delta, 36);
                loop {
                    let t = if k <= bias {
                        1
                    } else if bias + 26 <= k {
                        26
                    } else {
                        k - bias
                    };
                    if q < t {
                        break;
                    }
                    out.push(digit(t + (q - t) % (36 - t)));
                    q = (q - t) / (36 - t);
                    k += 36;
                }
                out.push(digit(q));

                let mut d = if handled == basic { delta / 700 } else { delta / 2 };
                d += d / (handled + 1);
                k = 0;
                while 455 < d {
                    d /= 35;
                    k += 36;
                }
                bias = k + 36 * d / (d + 38);
                delta = 0;
                handled += 1;
            }
        }
        delta += 1;
        n += 1;
    }
}

/// Builds a raw DNS TXT-record request for `domain`.
pub fn create_request(domain: &str, id: u16) -> Vec<u8> {
    let mut request = vec![0u8; 12 + domain.len() + 2 + 4];

    // DNS header fields.
    request[0] = (id >> 8) as u8;
    request[1] = id as u8;
    request[2] = 0x01; // QR=0, OPCODE=0, AA=0, TC=0, RD=1
    request[5] = 0x01; // QDCOUNT=1

    // The domain name in the question section, label by label.
    let mut index = 12;
    let mut p = index;
    index += 1;
    for &b in domain.as_bytes() {
        if b == b'.' {
            request[p] = (index - p - 1) as u8;
            p = index;
            index += 1;
        } else {
            request[index] = b;
            index += 1;
        }
    }
    request[p] = (index - p - 1) as u8; // the length of the last label

    index += 2; // terminate the name, then the question type and class
    request[index] = 0x10; // QTYPE = TXT
    index += 1;
    request[index + 1] = 0x01; // QCLASS = IN
    request.truncate(index + 2);
    request
}

/// Whether `response` answers the question of `plain`: the id, QR=1, one question and that question,
/// the case of its letters aside.
fn is_answer(response: &[u8], plain: &[u8]) -> bool {
    plain.len() <= response.len() && response[..2] == plain[..2] && response[2] & 0x80 != 0 && response[4] == 0 && response[5] == 1 && response[12..plain.len()].eq_ignore_ascii_case(&plain[12..])
}

/// Parses a raw DNS response into the TXT record values: one entry per TXT record of class IN, its
/// `<character-string>`s concatenated; other records ( a CNAME before them... ) are skipped. Records
/// in a reply may follow in arbitrary order.
pub fn parse_response(response: &[u8]) -> Vec<Vec<u8>> {
    if response.len() < 12 {
        return Vec::new();
    }
    let question_count = (response[4] as usize) << 8 | response[5] as usize;
    let answer_count = (response[6] as usize) << 8 | response[7] as usize;

    let mut index = 12;
    for _ in 0..question_count {
        // Skip the question section: QNAME, then QTYPE + QCLASS.
        let Some(end) = skip_name(response, index) else { return Vec::new() };
        index = end + 4;
    }

    let mut records = Vec::with_capacity(answer_count);
    for _ in 0..answer_count {
        // NAME: labels or a compression pointer, then TYPE, CLASS, TTL, RDLENGTH.
        match skip_name(response, index) {
            Some(end) if end + 10 <= response.len() => index = end,
            _ => break,
        }
        let txt = response[index..index + 4] == [0, 16, 0, 1]; // TYPE = TXT, CLASS = IN
        index += 8; // TYPE, CLASS, TTL
        let rdlength = (response[index] as usize) << 8 | response[index + 1] as usize;
        index += 2;

        let max = (index + rdlength).min(response.len());
        if !txt {
            index = max;
            continue;
        }

        let mut record = Vec::new();
        // TXT-DATA: one or more <character-string>s — a length octet followed by that many
        // characters.
        while index < max {
            let len = response[index] as usize;
            index += 1;
            let end = (index + len).min(max);
            record.extend_from_slice(&response[index..end]);
            index = end;
        }
        records.push(record);
    }
    records
}

/// The index right behind the domain name at `index`: labels up to the zero octet, or ending with a
/// two-octet compression pointer (RFC 1035 §4.1.4). `None` — it is not there whole.
fn skip_name(response: &[u8], mut index: usize) -> Option<usize> {
    loop {
        let len = *response.get(index)? as usize;
        if len == 0 {
            return Some(index + 1);
        }
        if len & 0xC0 == 0xC0 {
            return (index + 2 <= response.len()).then_some(index + 2);
        }
        if len & 0xC0 != 0 {
            return None;
        }
        index += len + 1;
    }
}

/// The OS-configured DNS servers.
#[cfg(not(windows))]
fn dns_servers() -> Vec<IpAddr> {
    let mut servers = Vec::new();
    if let Ok(conf) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in conf.lines() {
            let line = line.trim();
            if let Some(addr) = line.strip_prefix("nameserver") {
                if let Ok(ip) = addr.trim().parse() {
                    servers.push(ip);
                }
            }
        }
    }
    servers
}

/// The OS-configured DNS servers, Windows edition.
#[cfg(windows)]
fn dns_servers() -> Vec<IpAddr> {
    match ipconfig::get_adapters() {
        Ok(adapters) => adapters
            .iter()
            .filter(|a| a.oper_status() == ipconfig::OperStatus::IfOperStatusUp)
            .flat_map(|a| a.dns_servers().iter().copied())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Over TCP: what did not fit into UDP, or a server that does not know of OPT. `None` — it gave
/// nothing.
fn over_tcp(server: IpAddr, plain: &[u8], deadline: Instant) -> Option<Vec<Vec<u8>>> {
    let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    let mut tcp = TcpStream::connect_timeout(&SocketAddr::new(server, 53), left).ok()?;
    tcp.set_read_timeout(Some(left)).ok()?;

    let mut head = Vec::with_capacity(2 + plain.len());
    head.extend_from_slice(&(plain.len() as u16).to_be_bytes());
    head.extend_from_slice(plain);
    tcp.write_all(&head).ok()?;

    let mut size = [0u8; 2];
    tcp.read_exact(&mut size).ok()?;
    let mut response = vec![0u8; u16::from_be_bytes(size) as usize];
    tcp.read_exact(&mut response).ok()?;
    (is_answer(&response, plain) && matches!(response[3] & 0x0F, 0 | 3)).then(|| parse_response(&response))
}

/// Over TCP from every server at once, as a router often answers UDP and has no TCP at all: the
/// first answer that came. `None` — nobody gave one.
fn over_tcp_all(servers: &[IpAddr], plain: &[u8], deadline: Instant) -> Option<Vec<Vec<u8>>> {
    let (tx, rx) = std::sync::mpsc::channel();
    for &server in servers {
        let (tx, plain) = (tx.clone(), plain.to_vec());
        std::thread::spawn(move || {
            let _ = tx.send(over_tcp(server, &plain, deadline));
        });
    }
    drop(tx);
    rx.iter().flatten().next()
}

/// The TXT records of a name: DNS as a read-only key-value storage ( RFC 1035 ), asked through the
/// public servers [`PUBLIC_DNS`]. When they do not answer, as in a network that closes the port of
/// DNS, or know of no such record, as with a name inside a network, the DNS servers of this computer
/// are asked. The result: the records of this computer's servers, when they have any; otherwise the
/// answer of the public servers, when there was one; otherwise the answer of this computer's servers.
///
/// The servers of a group are asked at once, the public ones past the first [`PUBLIC_DNS_WHOLE`] from
/// the first repeat on, and the question is repeated while nobody answered: the first valid answer
/// with records is taken. "No such record" may be what a server remembers from before the record was
/// made, for up to half an hour: the server is done with, and the others get a third of the time for
/// a record. Each group has [`TIMEOUT`] of its own, and a family of addresses this computer does not have
/// is passed over. An answer counts when it comes from a server asked and echoes the random id and
/// the question. A long answer comes whole: over UDP up to 1232 bytes, past that over TCP, to the
/// servers of the same group: when one of the first ones answered, only to the first ones, as the
/// others would cut the answer.
///
/// DNS proves nothing about what it says: whoever is on the way can forge a reply. A value is
/// public, and it must not be able to send the program anywhere a forged one would harm.
///
/// One item per TXT record, its character strings joined; empty — the name has none; `None` — no
/// server answered, or `key` can not be a name of DNS.
pub fn value(key: &str) -> Option<Vec<Vec<u8>>> {
    let domain = ascii_name(key)?;
    let mut id = [0u8; 2];
    getrandom::fill(&mut id).ok()?;
    let plain = create_request(&domain, u16::from_be_bytes(id));

    // The same question with an OPT record: the size of a UDP answer this side takes.
    let mut request = plain.clone();
    request[11] = 1; // ARCOUNT
    request.extend_from_slice(&[0, 0, 41]); // its NAME is the root, TYPE = OPT
    request.extend_from_slice(&UDP_SIZE.to_be_bytes()); // CLASS = the size
    request.extend_from_slice(&[0; 6]); // TTL, RDLENGTH

    let answer = ask(&PUBLIC_DNS, PUBLIC_DNS_WHOLE, &plain, &request);
    if answer.as_ref().is_some_and(|records| !records.is_empty()) {
        return answer;
    }

    let mut servers = dns_servers();
    let mut seen = Vec::with_capacity(servers.len());
    servers.retain(|s| {
        let new = !seen.contains(s);
        seen.push(*s);
        new
    });

    let configured = ask(&servers, servers.len(), &plain, &request);
    if configured.as_ref().is_some_and(|records| !records.is_empty()) {
        return configured;
    }
    answer.or(configured)
}

/// One group of servers, [`TIMEOUT`] for it: at once and again, the first valid answer with records.
/// `first` — how many of the servers are asked at once; the others — from the next repeat on. Empty —
/// only "no such record" came. `None` — nobody gave an answer.
fn ask(servers: &[IpAddr], first: usize, plain: &[u8], request: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut poll = Poll::new().ok()?;
    let mut events = Events::with_capacity(2);
    let mut udp: [Option<UdpSocket>; 2] = [None, None]; // IPv4, IPv6
    for server in servers {
        let family = server.is_ipv6() as usize;
        if udp[family].is_none() {
            let bind: SocketAddr = if server.is_ipv6() {
                (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
            } else {
                (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
            };
            if let Ok(mut socket) = UdpSocket::bind(bind) {
                if poll.registry().register(&mut socket, Token(family), Interest::READABLE).is_ok() {
                    udp[family] = Some(socket);
                }
            }
        }
    }

    let mut deadline = Instant::now() + TIMEOUT;
    let mut none = None;
    let step = TIMEOUT / 3;
    let mut again = Instant::now();
    let mut repeat = false;
    let mut silent: Vec<IpAddr> = servers.iter().copied().filter(|s| udp[s.is_ipv6() as usize].is_some()).collect(); // a family that is not there is passed over
    let mut buffer = [0u8; 4096];

    while !silent.is_empty() {
        let now = Instant::now();
        if deadline <= now {
            return none;
        }
        if again <= now {
            again = now + step;
            for server in &silent {
                if !repeat && !servers[..first].contains(server) {
                    continue;
                }
                if let Some(socket) = &udp[server.is_ipv6() as usize] {
                    let _ = socket.send_to(request, SocketAddr::new(*server, 53));
                    // no route to it: the others are not to suffer
                }
            }
            repeat = true;
        }

        match poll.poll(&mut events, Some(again.min(deadline) - now)) {
            Err(e) if e.kind() != ErrorKind::Interrupted => return none,
            _ => {}
        }

        for socket in udp.iter().flatten() {
            loop {
                let (length, from) = match socket.recv_from(&mut buffer) {
                    Ok(received) => received,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) if deadline <= Instant::now() => return none,
                    Err(_) => continue, // an ICMP of a server that does not listen: the repeat and the deadline take care of it
                };
                let response = &buffer[..length];
                let Some(sender) = silent.iter().position(|s| *s == from.ip()) else { continue };
                if !is_answer(response, plain) {
                    continue;
                }
                match response[3] & 0x0F {
                    rcode @ (0 | 3) if rcode == 3 || response[2] & 0x02 == 0 => {
                        // no such name, or whole
                        let answered = parse_response(response);
                        if !answered.is_empty() {
                            return Some(answered);
                        }
                        // "No such record" may be what this server remembers from before the record was made, for up to half an hour:
                        // a record from another one wins, and they get a third of the time for it.
                        none = Some(answered);
                        deadline = deadline.min(Instant::now() + step);
                    }
                    // TC: it does not fit; FORMERR: a server that does not know of OPT. Those of the first ones among themselves: the others would cut the answer.
                    0 | 1 => return over_tcp_all(if servers[..first].contains(&silent[sender]) { &servers[..first] } else { servers }, plain, deadline),
                    _ => {}
                }
                silent.swap_remove(sender); // it can not tell
            }
        }
    }
    none
}

// Tests (offline: the packet formats only)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout() {
        let req = create_request("example.com", 0x1234);
        assert_eq!(&req[0..2], &[0x12, 0x34], "query id, big-endian");
        assert_eq!(req[2], 0x01, "RD set");
        assert_eq!(req[5], 0x01, "QDCOUNT=1");
        assert_eq!(&req[12..25], b"\x07example\x03com\x00");
        assert_eq!(&req[25..29], &[0x00, 0x10, 0x00, 0x01], "QTYPE=TXT, QCLASS=IN");
        assert_eq!(req.len(), 29);
    }

    #[test]
    fn response_parsing() {
        // Header: 1 question, 2 answers; question repeats the query name; answers use the
        // compression pointer 0xC00C for NAME.
        let mut r = vec![0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00];
        r.extend_from_slice(b"\x03key\x07example\x00");
        r.extend_from_slice(&[0x00, 0x10, 0x00, 0x01]); // QTYPE, QCLASS

        // Answer 1: TXT with two character-strings "hel" + "lo".
        r.extend_from_slice(&[0xC0, 0x0C]); // NAME pointer
        r.extend_from_slice(&[0x00, 0x10, 0x00, 0x01]); // TYPE=TXT, CLASS=IN
        r.extend_from_slice(&[0x00, 0x00, 0x00, 0x3C]); // TTL
        r.extend_from_slice(&[0x00, 0x07]); // RDLENGTH = 7
        r.extend_from_slice(b"\x03hel\x02lo");

        // Answer 2: a non-TXT record (A): skipped.
        r.extend_from_slice(&[0xC0, 0x0C]);
        r.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // TYPE=A
        r.extend_from_slice(&[0x00, 0x00, 0x00, 0x3C]);
        r.extend_from_slice(&[0x00, 0x04]); // RDLENGTH = 4
        r.extend_from_slice(&[127, 0, 0, 1]);

        let records = parse_response(&r);
        assert_eq!(records, vec![b"hello".to_vec()], "a non-TXT answer yields no value");
    }

    #[test]
    fn response_parsing_skips_other_records() {
        // A CNAME before the TXT, and a TXT of another class after it.
        let mut r = vec![0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00];
        r.extend_from_slice(b"\x03key\x07example\x00");
        r.extend_from_slice(&[0x00, 0x10, 0x00, 0x01]);

        r.extend_from_slice(&[0xC0, 0x0C, 0x00, 0x05, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3C, 0x00, 0x02, 0xC0, 0x0C]); // CNAME
        r.extend_from_slice(&[0xC0, 0x0C, 0x00, 0x10, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3C, 0x00, 0x03]); // TXT, IN
        r.extend_from_slice(b"\x02ok");
        r.extend_from_slice(&[0xC0, 0x0C, 0x00, 0x10, 0x00, 0x03, 0x00, 0x00, 0x00, 0x3C, 0x00, 0x03]); // TXT, CH
        r.extend_from_slice(b"\x02no");

        assert_eq!(parse_response(&r), vec![b"ok".to_vec()]);

        // A string longer than its record, a record longer than the packet: stays inside.
        let cut = r.len() - 1;
        r[cut - 2] = 0xFF;
        assert_eq!(parse_response(&r[..cut]), vec![b"ok".to_vec()]);
        let mut r = vec![0x12, 0x34, 0x81, 0x80, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
        r.extend_from_slice(&[0x00, 0x00, 0x10, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3C, 0xFF, 0xFF, 0xFF, b'a']);
        assert_eq!(parse_response(&r), vec![b"a".to_vec()]);
    }

    #[test]
    fn answer_checks() {
        let plain = create_request("Example.com", 0x1234);
        let mut r = plain.clone();
        r[2] = 0x81;
        r[3] = 0x80;
        assert!(is_answer(&r, &plain));
        r[13] = b'E';
        r[14] = b'X';
        assert!(is_answer(&r, &plain), "the case of the letters aside");
        for (at, to) in [(0, 0x13u8), (2, 0x01), (5, 2), (4, 1), (14, b'y')] {
            let mut bad = r.clone();
            bad[at] = to;
            assert!(!is_answer(&bad, &plain), "byte {at}");
        }
        assert!(!is_answer(&r[..r.len() - 1], &plain), "cut");
    }

    #[test]
    fn name_conversion() {
        assert_eq!(ascii_name("_dmarc.Example.com.").as_deref(), Some("_dmarc.Example.com"));
        assert_eq!(ascii_name("пример.рф").as_deref(), Some("xn--e1afmkfd.xn--p1ai"));
        assert_eq!(ascii_name("ПРИМЕР.рф").as_deref(), Some("xn--e1afmkfd.xn--p1ai"));
        assert_eq!(ascii_name("münchen.de").as_deref(), Some("xn--mnchen-3ya.de"));
        assert_eq!(ascii_name("bücher.de").as_deref(), Some("xn--bcher-kva.de"));
        assert_eq!(ascii_name(""), None);
        assert_eq!(ascii_name("."), None);
        assert_eq!(ascii_name("a..b"), None);
        assert_eq!(ascii_name(&"a".repeat(64)), None);
        assert_eq!(ascii_name(&format!("{0}.{0}.{0}.{0}", "a".repeat(63))), None); // 255
        assert_eq!(ascii_name(&format!("{0}.{0}.{0}.{1}", "a".repeat(63), "a".repeat(61))).map(|n| n.len()), Some(253));
    }

    #[test]
    fn response_parsing_uncompressed_name() {
        // Answers may carry NAME as plain labels instead of a compression pointer.
        let mut r = vec![0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00];
        r.extend_from_slice(b"\x07example\x03com\x00");
        r.extend_from_slice(&[0x00, 0x10, 0x00, 0x01]);

        // Answer 1: NAME as labels, TXT "abc".
        r.extend_from_slice(b"\x07example\x03com\x00");
        r.extend_from_slice(&[0x00, 0x10, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3C, 0x00, 0x04]);
        r.extend_from_slice(b"\x03abc");

        // Answer 2: NAME as a label followed by a pointer, TXT "de".
        r.extend_from_slice(b"\x03key\xC0\x0C");
        r.extend_from_slice(&[0x00, 0x10, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3C, 0x00, 0x03]);
        r.extend_from_slice(b"\x02de");

        let records = parse_response(&r);
        assert_eq!(records, vec![b"abc".to_vec(), b"de".to_vec()]);
        assert_eq!(parse_response(&r[..r.len() - 12]), vec![b"abc".to_vec()], "a cut name ends the answers");
    }

    #[test]
    fn response_parsing_rejects_garbage() {
        assert!(parse_response(&[]).is_empty());
        assert!(parse_response(&[0u8; 5]).is_empty());
    }
}
