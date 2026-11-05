//! The network commands: `ifconfig`, `ping`, `nslookup`, `wget` and `nc`.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use feos_app::{App, Context, Platform};
use feos_console::{eprintln, print, println};
use feos_hal::net::{Endpoint, Ipv4Addr, NetError, Network, SocketId, SocketInfo};

use crate::netd;

/// How long a DHCP lease may take, for a command run straight after boot.
const CONFIG_WAIT_MS: u64 = 15_000;
/// How long to look up a name.
const DNS_WAIT_MS: u64 = 6000;
/// How long a TCP connection may take to come up.
const CONNECT_WAIT_MS: u64 = 20_000;
/// How long a transfer may go without moving a byte.
const STALL_MS: u64 = 60_000;
/// One wait, between looks for a kill.
const SLICE_MS: u64 = 200;
/// Read and written at a time.
const CHUNK: usize = 16 * 1024;

fn network(context: &Context<'_>, command: &str) -> Option<&'static dyn Network> {
    let Some(net) = context.platform.net else {
        eprintln!("{command}: no network card (start QEMU with -device virtio-net)");
        return None;
    };
    if let Some(platform) = feos_app::platform() {
        netd::ensure_running(platform);
    }
    Some(net)
}

fn resolve(net: &dyn Network, command: &str, host: &str) -> Option<Ipv4Addr> {
    match net.resolve(host, CONFIG_WAIT_MS + DNS_WAIT_MS) {
        Ok(addr) => Some(addr),
        Err(error) => {
            eprintln!("{command}: {host}: {}", error.message());
            None
        }
    }
}

/// `ifconfig`: how the interface is set up, and how busy it's been.
pub struct Ifconfig;

impl App for Ifconfig {
    fn name(&self) -> &'static str {
        "ifconfig"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["ip"]
    }

    fn usage(&self) -> &'static str {
        "ifconfig [-s]"
    }

    fn description(&self) -> &'static str {
        "show the network interface, or with -s its sockets"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let Some(net) = network(context, "ifconfig") else {
            return 1;
        };
        if context.argv.first() == Some(&"-s") {
            let mut sockets = [SocketInfo {
                state: "",
                local: Endpoint::default(),
                remote: Endpoint::default(),
                owner: 0,
            }; 64];
            let count = net.sockets(&mut sockets);
            println!("{:<12} {:<22} {:<22} PID", "STATE", "LOCAL", "REMOTE");
            for socket in &sockets[..count] {
                let endpoint = |end: Endpoint| {
                    let mut text = String::new();
                    let _ = core::fmt::Write::write_fmt(&mut text, format_args!("{}:{}", end.addr, end.port));
                    text
                };
                println!("{:<12} {:<22} {:<22} {}", socket.state, endpoint(socket.local), endpoint(socket.remote), socket.owner);
            }
            return 0;
        }
        let config = net.config();
        let [a, b, c, d, e, f] = config.mac;
        println!("eth0: {} ether {a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}", if config.link_up { "UP" } else { "DOWN" });
        if config.addr.is_unspecified() {
            println!("    no address yet: waiting for DHCP");
        } else {
            println!(
                "    inet {} netmask {} gateway {} dns {}",
                config.addr, config.netmask, config.gateway, config.dns
            );
            println!("    dhcp server {} lease {} s", config.dhcp_server, config.lease_remaining_ms / 1000);
        }
        let stats = net.stats();
        println!(
            "    rx {} frames {} bytes, {} dropped; tx {} frames {} bytes, {} dropped",
            stats.rx_frames, stats.rx_bytes, stats.rx_dropped, stats.tx_frames, stats.tx_bytes, stats.tx_dropped
        );
        println!(
            "    tcp {} sockets, {} segments retransmitted; udp {} sockets",
            stats.tcp_sockets, stats.tcp_retransmits, stats.udp_sockets
        );
        0
    }
}

/// `ping`: ICMP echo.
pub struct Ping;

impl App for Ping {
    fn name(&self) -> &'static str {
        "ping"
    }

    fn usage(&self) -> &'static str {
        "ping [-c count] [-s size] <host>"
    }

    fn description(&self) -> &'static str {
        "send ICMP echo requests"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let Some(net) = network(context, "ping") else {
            return 1;
        };
        let mut count = 4u32;
        let mut size = 56usize;
        let mut host = None;
        let mut args = context.argv.iter();
        while let Some(&arg) = args.next() {
            match arg {
                "-c" => match args.next().and_then(|text| text.parse().ok()) {
                    Some(value) => count = value,
                    None => return usage(self),
                },
                "-s" => match args.next().and_then(|text| text.parse().ok()) {
                    Some(value) => size = value,
                    None => return usage(self),
                },
                _ => host = Some(arg),
            }
        }
        let Some(host) = host else {
            return usage(self);
        };
        let Some(target) = resolve(net, "ping", host) else {
            return 1;
        };
        net.wait_configured(CONFIG_WAIT_MS);
        println!("PING {host} ({target}): {size} data bytes");
        let processes = context.platform.processes;
        let mut received = 0;
        for sequence in 0..count {
            if sequence != 0 {
                processes.sleep_ms(1000);
            }
            match net.ping(target, sequence as u16, size, 2000) {
                Ok(micros) => {
                    received += 1;
                    println!(
                        "{} bytes from {target}: seq={sequence} time={}.{:03} ms",
                        size + 8,
                        micros / 1000,
                        micros % 1000
                    );
                }
                Err(NetError::TimedOut) => println!("no answer from {target}: seq={sequence}"),
                Err(NetError::Interrupted) => return feos_app::EXIT_KILLED,
                Err(error) => {
                    eprintln!("ping: {}", error.message());
                    return 1;
                }
            }
        }
        println!("{count} packets transmitted, {received} received");
        i32::from(received == 0)
    }
}

/// `nslookup`: asks the name server for an address.
pub struct Nslookup;

impl App for Nslookup {
    fn name(&self) -> &'static str {
        "nslookup"
    }

    fn usage(&self) -> &'static str {
        "nslookup <name>"
    }

    fn description(&self) -> &'static str {
        "look up a name's address"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let Some(net) = network(context, "nslookup") else {
            return 1;
        };
        let Some(&name) = context.argv.first() else {
            return usage(self);
        };
        let Some(addr) = resolve(net, "nslookup", name) else {
            return 1;
        };
        println!("Server: {}", net.config().dns);
        println!("Name: {name}");
        println!("Address: {addr}");
        0
    }
}

/// Where a URL points.
#[derive(Debug, PartialEq, Eq)]
struct Url<'a> {
    host: &'a str,
    port: u16,
    path: &'a str,
}

fn parse_url(url: &str) -> Option<Url<'_>> {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    if rest.contains("://") {
        return None;
    }
    let (authority, path) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, 80),
    };
    if host.is_empty() || port == 0 || path.bytes().any(|byte| byte <= b' ' || byte == 0x7f) {
        return None;
    }
    Some(Url { host, port, path })
}

/// What an HTTP response's head says.
#[derive(Debug, PartialEq, Eq)]
struct ResponseHead {
    status: u16,
    length: Option<u64>,
}

fn parse_head(head: &[u8]) -> Option<ResponseHead> {
    let head = core::str::from_utf8(head).ok()?;
    let mut lines = head.split("\r\n");
    let status = lines.next()?;
    if !status.starts_with("HTTP/1.") {
        return None;
    }
    let status = status.split(' ').nth(1)?.parse().ok()?;
    let mut length = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().ok();
            }
        }
    }
    Some(ResponseHead { status, length })
}

/// Where the end of the head is, in what's been read so far.
fn find_head_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|window| window == b"\r\n\r\n").map(|at| at + 4)
}

fn absolute(path: &str) -> String {
    if path.starts_with('/') {
        String::from(path)
    } else {
        let mut full = String::from("/");
        full.push_str(path);
        full
    }
}

/// `wget`: an HTTP GET, saved to a file.
pub struct Wget;

impl Wget {
    fn fail(error: NetError) -> i32 {
        if error == NetError::Interrupted {
            return feos_app::EXIT_KILLED;
        }
        eprintln!("wget: {}", error.message());
        1
    }

    fn save(net: &dyn Network, platform: &Platform, socket: SocketId, url: &Url<'_>, path: &str) -> i32 {
        let Some(tree) = platform.filesystem.and_then(|filesystem| filesystem.tree()) else {
            eprintln!("wget: no writable filesystem");
            return 1;
        };
        let mut request = String::new();
        let _ = core::fmt::Write::write_fmt(
            &mut request,
            format_args!(
                "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: feos-wget\r\nAccept: */*\r\nConnection: close\r\n\r\n",
                url.path, url.host
            ),
        );
        if let Err(error) = send_all(net, platform, socket, request.as_bytes()) {
            return Self::fail(error);
        }

        // The head, which can arrive in any number of pieces.
        let mut buffer = vec![0u8; CHUNK];
        let mut head: Vec<u8> = Vec::new();
        let body_start;
        loop {
            match recv_some(net, platform, socket, &mut buffer) {
                Ok(0) => {
                    eprintln!("wget: the server closed the connection early");
                    return 1;
                }
                Ok(count) => head.extend_from_slice(&buffer[..count]),
                Err(error) => return Self::fail(error),
            }
            if let Some(end) = find_head_end(&head) {
                body_start = end;
                break;
            }
            if head.len() > 64 * 1024 {
                eprintln!("wget: the response's head is too long");
                return 1;
            }
        }
        let Some(response) = parse_head(&head[..body_start]) else {
            eprintln!("wget: not an HTTP response");
            return 1;
        };
        if !(200..300).contains(&response.status) {
            eprintln!("wget: server said {}", response.status);
            return 1;
        }

        if tree.write_file(path, &[], false).is_err() {
            eprintln!("wget: can't create {path}");
            return 1;
        }
        let mut written = 0u64;
        let first = &head[body_start..];
        if !first.is_empty() {
            if tree.write_at(path, 0, first).is_err() {
                eprintln!("wget: can't write {path}");
                return 1;
            }
            written += first.len() as u64;
        }
        loop {
            match recv_some(net, platform, socket, &mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if tree.write_at(path, written, &buffer[..count]).is_err() {
                        eprintln!("wget: can't write {path}");
                        return 1;
                    }
                    written += count as u64;
                }
                Err(error) => return Self::fail(error),
            }
        }
        if let Some(length) = response.length {
            if length != written {
                eprintln!("wget: got {written} bytes of {length}");
                return 1;
            }
        }
        println!("wget: saved {written} bytes to {path}");
        0
    }
}

impl App for Wget {
    fn name(&self) -> &'static str {
        "wget"
    }

    fn usage(&self) -> &'static str {
        "wget [-O file] <http://host[:port]/path>"
    }

    fn description(&self) -> &'static str {
        "fetch a URL over HTTP and save it"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let Some(net) = network(context, "wget") else {
            return 1;
        };
        let mut output = None;
        let mut target = None;
        let mut args = context.argv.iter();
        while let Some(&arg) = args.next() {
            if arg == "-O" {
                let Some(&path) = args.next() else {
                    return usage(self);
                };
                output = Some(path);
            } else {
                target = Some(arg);
            }
        }
        let Some(target) = target else {
            return usage(self);
        };
        let Some(url) = parse_url(target) else {
            eprintln!("wget: only http:// URLs: {target}");
            return 2;
        };
        let path = match output {
            Some(path) => absolute(path),
            None => {
                let name = url.path.rsplit('/').next().unwrap_or("");
                absolute(if name.is_empty() { "index.html" } else { name })
            }
        };
        let Some(addr) = resolve(net, "wget", url.host) else {
            return 1;
        };
        net.wait_configured(CONFIG_WAIT_MS);
        let socket = match net.tcp_connect(Endpoint { addr, port: url.port }, CONNECT_WAIT_MS) {
            Ok(socket) => socket,
            Err(error) => return Self::fail(error),
        };
        let status = Self::save(net, context.platform, socket, &url, &path);
        net.close(socket);
        status
    }
}

fn send_all(net: &dyn Network, platform: &Platform, socket: SocketId, mut data: &[u8]) -> Result<(), NetError> {
    let mut idle = 0;
    while !data.is_empty() {
        match net.tcp_send(socket, data, SLICE_MS) {
            Ok(count) => {
                data = &data[count..];
                idle = 0;
            }
            Err(NetError::TimedOut) => {
                idle += SLICE_MS;
                if idle >= STALL_MS {
                    return Err(NetError::TimedOut);
                }
            }
            Err(error) => return Err(error),
        }
        if platform.processes.kill_requested() {
            return Err(NetError::Interrupted);
        }
    }
    Ok(())
}

fn recv_some(net: &dyn Network, platform: &Platform, socket: SocketId, out: &mut [u8]) -> Result<usize, NetError> {
    let mut idle = 0;
    loop {
        match net.tcp_recv(socket, out, SLICE_MS) {
            Err(NetError::TimedOut) => {
                idle += SLICE_MS;
                if idle >= STALL_MS {
                    return Err(NetError::TimedOut);
                }
            }
            other => return other,
        }
        if platform.processes.kill_requested() {
            return Err(NetError::Interrupted);
        }
    }
}

/// What `nc` was told to do.
#[derive(Debug, Default, PartialEq, Eq)]
struct NcOptions<'a> {
    listen: bool,
    text: Option<&'a str>,
    file: Option<&'a str>,
    output: Option<&'a str>,
    host: Option<&'a str>,
    port: u16,
    keep_open: bool,
}

fn parse_nc<'a>(argv: &[&'a str]) -> Option<NcOptions<'a>> {
    let mut options = NcOptions::default();
    let mut words = Vec::new();
    let mut args = argv.iter();
    while let Some(&arg) = args.next() {
        match arg {
            "-l" => options.listen = true,
            "-k" => options.keep_open = true,
            "-s" => options.text = Some(args.next()?),
            "-f" => options.file = Some(args.next()?),
            "-o" => options.output = Some(args.next()?),
            _ => words.push(arg),
        }
    }
    let port = if options.listen {
        match words.as_slice() {
            [port] => port,
            _ => return None,
        }
    } else {
        match words.as_slice() {
            [host, port] => {
                options.host = Some(host);
                port
            }
            _ => return None,
        }
    };
    options.port = port.parse().ok().filter(|&port| port != 0)?;
    Some(options)
}

/// `nc`: a raw TCP connection, either way.
pub struct Nc;

impl Nc {
    /// Sends what it was given, finishes, and takes in everything the peer sends.
    fn exchange(net: &dyn Network, platform: &Platform, socket: SocketId, options: &NcOptions<'_>) -> i32 {
        let tree = platform.filesystem.and_then(|filesystem| filesystem.tree());
        let source = match options.file {
            Some(path) => {
                let path = absolute(path);
                let Some(tree) = tree else {
                    eprintln!("nc: no filesystem");
                    return 1;
                };
                if tree.stat(&path).is_err() {
                    eprintln!("nc: can't read {path}");
                    return 1;
                }
                Some(path)
            }
            None => None,
        };
        let target = options.output.map(absolute);
        if let (Some(path), Some(tree)) = (&target, tree) {
            if tree.write_file(path, &[], false).is_err() {
                eprintln!("nc: can't create {path}");
                return 1;
            }
        }

        let mut outgoing = vec![0u8; CHUNK];
        let mut text = options.text.map_or(&[][..], str::as_bytes);
        let mut pending_range = (0usize, 0usize);
        let mut sent_file = 0u64;
        let mut sending = true;
        let mut received = 0u64;
        let mut incoming = vec![0u8; CHUNK];
        let mut idle = 0u64;
        loop {
            if platform.processes.kill_requested() {
                return feos_app::EXIT_KILLED;
            }
            let mut moved = false;
            // Keeps both directions going, so a peer that reads and writes at once
            // can't deadlock with this.
            if sending {
                if !text.is_empty() {
                    match net.tcp_send(socket, text, 0) {
                        Ok(count) => {
                            text = &text[count..];
                            moved = true;
                        }
                        Err(NetError::TimedOut) => {}
                        Err(error) => return Self::report(error),
                    }
                } else if let (Some(path), Some(tree)) = (&source, tree) {
                    if pending_range.0 == pending_range.1 {
                        match tree.read_at(path, sent_file, &mut outgoing) {
                            Ok(count) => pending_range = (0, count),
                            Err(_) => {
                                eprintln!("nc: can't read {path}");
                                return 1;
                            }
                        }
                    }
                    if pending_range.0 == pending_range.1 {
                        sending = false;
                    } else {
                        match net.tcp_send(socket, &outgoing[pending_range.0..pending_range.1], 0) {
                            Ok(count) => {
                                pending_range.0 += count;
                                sent_file += count as u64;
                                moved = true;
                            }
                            Err(NetError::TimedOut) => {}
                            Err(error) => return Self::report(error),
                        }
                    }
                } else {
                    sending = false;
                }
                if !sending {
                    match net.tcp_shutdown(socket) {
                        Ok(()) => {}
                        Err(error) => return Self::report(error),
                    }
                }
            }
            // A short wait for data when there's nothing else to do.
            match net.tcp_recv(socket, &mut incoming, if moved { 0 } else { 20 }) {
                Ok(0) => {
                    if sending {
                        // The peer's done, and this end may not be.
                        if !moved {
                            platform.processes.yield_now();
                        }
                    } else {
                        break;
                    }
                }
                Ok(count) => {
                    moved = true;
                    match (&target, tree) {
                        (Some(path), Some(tree)) => {
                            if tree.write_at(path, received, &incoming[..count]).is_err() {
                                eprintln!("nc: can't write {path}");
                                return 1;
                            }
                        }
                        _ => write_stdout(&incoming[..count]),
                    }
                    received += count as u64;
                }
                Err(NetError::TimedOut) => {}
                Err(error) => return Self::report(error),
            }
            if moved {
                idle = 0;
            } else {
                idle += 20;
                if idle > STALL_MS * 10 {
                    eprintln!("nc: nothing moved for ten minutes");
                    return 1;
                }
            }
        }
        // Everything sent has been acknowledged before saying it's done.
        if let Err(error) = net.tcp_flush(socket, STALL_MS) {
            eprintln!("nc: {}", error.message());
            return 1;
        }
        if target.is_some() {
            println!("nc: received {received} bytes");
        }
        println!("nc: done, sent {} received {received}", sent_file + options.text.map_or(0, str::len) as u64);
        0
    }

    fn report(error: NetError) -> i32 {
        if error == NetError::Interrupted {
            return feos_app::EXIT_KILLED;
        }
        eprintln!("nc: {}", error.message());
        1
    }
}

/// Prints bytes as they came, with invalid UTF-8 cleaned up by the terminal.
fn write_stdout(bytes: &[u8]) {
    match core::str::from_utf8(bytes) {
        Ok(text) => print!("{text}"),
        Err(_) => feos_console::write_bytes(bytes),
    }
}

impl App for Nc {
    fn name(&self) -> &'static str {
        "nc"
    }

    fn usage(&self) -> &'static str {
        "nc [-s text] [-f file] [-o file] <host> <port> | nc -l [-k] [-s text] [-f file] [-o file] <port>"
    }

    fn description(&self) -> &'static str {
        "open a TCP connection, or wait for one"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let Some(net) = network(context, "nc") else {
            return 1;
        };
        let Some(options) = parse_nc(context.argv) else {
            return usage(self);
        };
        let platform = context.platform;
        if options.listen {
            net.wait_configured(CONFIG_WAIT_MS);
            let listener = match net.tcp_listen(options.port, 4) {
                Ok(listener) => listener,
                Err(error) => return Self::report(error),
            };
            println!("nc: listening on port {}", options.port);
            let mut status;
            loop {
                let accepted = net.tcp_accept(listener, SLICE_MS);
                let (socket, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(NetError::TimedOut) => {
                        if platform.processes.kill_requested() {
                            net.close(listener);
                            return feos_app::EXIT_KILLED;
                        }
                        continue;
                    }
                    Err(error) => {
                        net.close(listener);
                        return Self::report(error);
                    }
                };
                println!("nc: connection from {}:{}", peer.addr, peer.port);
                status = Self::exchange(net, platform, socket, &options);
                net.close(socket);
                if !options.keep_open {
                    break;
                }
            }
            net.close(listener);
            return status;
        }
        let Some(host) = options.host else {
            return usage(self);
        };
        let Some(addr) = resolve(net, "nc", host) else {
            return 1;
        };
        net.wait_configured(CONFIG_WAIT_MS);
        let socket = match net.tcp_connect(Endpoint { addr, port: options.port }, CONNECT_WAIT_MS) {
            Ok(socket) => socket,
            Err(error) => return Self::report(error),
        };
        let status = Self::exchange(net, platform, socket, &options);
        net.close(socket);
        status
    }
}

fn usage(app: &dyn App) -> i32 {
    eprintln!("usage: {}", app.usage());
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse() {
        assert_eq!(parse_url("http://a.b:8080/c/d?e=1"), Some(Url { host: "a.b", port: 8080, path: "/c/d?e=1" }));
        assert_eq!(parse_url("a.b"), Some(Url { host: "a.b", port: 80, path: "/" }));
        assert_eq!(parse_url("10.0.2.2:80/x"), Some(Url { host: "10.0.2.2", port: 80, path: "/x" }));
        assert_eq!(parse_url("https://a.b/"), None);
        assert_eq!(parse_url("http://:80/"), None);
        assert_eq!(parse_url("http://a:0/"), None);
        assert_eq!(parse_url("http://a:99999/"), None);
        assert_eq!(parse_url("http://a/b c"), None);
        assert_eq!(parse_url("http://a/b\r\nX: y"), None, "header injection");
    }

    #[test]
    fn response_heads_parse() {
        let head = b"HTTP/1.1 200 OK\r\nServer: x\r\ncontent-LENGTH: 12\r\n\r\n";
        assert_eq!(parse_head(head), Some(ResponseHead { status: 200, length: Some(12) }));
        assert_eq!(parse_head(b"HTTP/1.0 404 Not Found\r\n\r\n"), Some(ResponseHead { status: 404, length: None }));
        for bad in [&b""[..], b"garbage\r\n\r\n", b"HTTP/1.1\r\n\r\n", b"HTTP/1.1 abc\r\n\r\n", b"\xff\xfe\r\n\r\n"] {
            assert_eq!(parse_head(bad), None);
        }
        assert_eq!(find_head_end(b"abc\r\n\r\ndef"), Some(7));
        assert_eq!(find_head_end(b"abc\r\n"), None);
    }

    #[test]
    fn nc_arguments_parse() {
        assert_eq!(parse_nc(&["a", "80"]).map(|o| (o.host, o.port)), Some((Some("a"), 80)));
        let listen = parse_nc(&["-l", "-o", "/x", "90"]).unwrap();
        assert_eq!((listen.listen, listen.output, listen.port), (true, Some("/x"), 90));
        for bad in [&[][..], &["a"], &["-l"], &["a", "0"], &["a", "x"], &["a", "1", "2"], &["-s"]] {
            assert_eq!(parse_nc(bad), None, "{bad:?}");
        }
    }
}
