//! TLS relay for one hostname. Uses the game's existing authenticated socket only.
use crate::{
    Error, Result,
    cloud::History,
    io_error, net_error, platform,
    protocol::{self, Guard, HOST},
    proxy_certs,
    proxy_wire::{self, Broker, RelayFrames, Session},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Seek, Write},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub const ROUTE_SCRIPT: &str = include_str!("../assets/proxy_route.ps1");
pub type Headers = HashMap<String, String>;
pub fn read_headers(stream: &mut impl Read) -> Result<(Vec<u8>, String, Headers)> {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).map_err(net_error)?;
        raw.push(byte[0]);
        if raw.len() > 65536 {
            return Err(Error::new("HTTP headers exceed limits."));
        }
    }
    let text: String = raw.iter().map(|&b| char::from(b)).collect();
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    for line in lines.filter(|s| !s.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| Error::new("Invalid HTTP headers."))?;
        let name = name.to_ascii_lowercase();
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || headers.contains_key(&name)
        {
            return Err(Error::new("Invalid HTTP headers."));
        }
        headers.insert(name, value.trim().into());
    }
    Ok((raw, first, headers))
}
fn header<'a>(h: &'a Headers, key: &str) -> &'a str {
    h.get(key).map(String::as_str).unwrap_or_default()
}
pub fn validate_handshake(first: &str, h: &Headers) -> Result<()> {
    let p: Vec<_> = first.split(' ').collect();
    let valid = p.len() == 3
        && p[0] == "GET"
        && p[2] == "HTTP/1.1"
        && ["/ws/gc", "/ws/gc2", "/ws/gc?PsyConnectionType=Player"].contains(&p[1])
        && [HOST, &format!("{HOST}:443")]
            .contains(&header(h, "host").to_ascii_lowercase().as_str())
        && header(h, "upgrade").eq_ignore_ascii_case("websocket")
        && header(h, "connection")
            .split(',')
            .any(|v| v.trim().eq_ignore_ascii_case("upgrade"))
        && header(h, "sec-websocket-version") == "13"
        && STANDARD
            .decode(header(h, "sec-websocket-key"))
            .is_ok_and(|v| v.len() == 16)
        && !header(h, "psytoken").is_empty()
        && !header(h, "psysessionid").is_empty();
    if valid {
        Ok(())
    } else {
        Err(Error::new(
            "Unexpected or unauthenticated game WebSocket handshake.",
        ))
    }
}
pub fn parse_player_id(text: &str) -> Option<String> {
    let pattern = regex::Regex::new(r"HandleLocalPlayerLoginStatusChanged[^\r\n]*?PlayerID=((?:Epic\|[a-fA-F0-9]{32}|Steam\|[0-9]{17})\|0)[^\r\n]*?LoginStatus=LS_LoggedIn[^\r\n]*?IsPrimary=True").unwrap();
    pattern.captures_iter(text).last().map(|c| c[1].into())
}
fn local_player_id() -> Result<String> {
    let mut f = std::fs::File::open(
        platform::documents().join("My Games/Rocket League/TAGame/Logs/Launch.log"),
    )
    .map_err(|_| Error::new("Game login identity is not available yet."))?;
    let size = f.metadata().map_err(io_error)?.len();
    f.seek(io::SeekFrom::Start(size.saturating_sub(8 * 1024 * 1024)))
        .map_err(io_error)?;
    let mut raw = Vec::new();
    f.take(8 * 1024 * 1024)
        .read_to_end(&mut raw)
        .map_err(io_error)?;
    parse_player_id(&String::from_utf8_lossy(&raw))
        .ok_or_else(|| Error::new("Waiting for the game's primary player login identity."))
}
pub fn resolve_upstream() -> Result<Vec<IpAddr>> {
    let mut command = platform::hidden_command("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", "Resolve-DnsName -Name 'ws.rlpp.psynet.gg' -Type A -DnsOnly -NoHostsFile -QuickTimeout -ErrorAction Stop | Where-Object Type -eq 'A' | Select-Object -ExpandProperty IPAddress"]);
    let output = platform::command_output(command, Duration::from_secs(15))?;
    let mut addresses = Vec::new();
    if output.status.success() {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Ok(IpAddr::V4(ip)) = line.trim().parse() {
                let octets = ip.octets();
                if !ip.is_loopback()
                    && !ip.is_private()
                    && !ip.is_link_local()
                    && !ip.is_unspecified()
                    && !ip.is_multicast()
                    && !ip.is_broadcast()
                    && !ip.is_documentation()
                    && octets[0] != 0
                    && octets[0] < 240
                    && !(octets[0] == 100 && (64..128).contains(&octets[1]))
                    && !(octets[0] == 198 && [18, 19].contains(&octets[1]))
                    && !addresses.contains(&IpAddr::V4(ip))
                {
                    addresses.push(IpAddr::V4(ip));
                }
            }
        }
    }
    if addresses.is_empty() {
        return Err(Error::new(
            "Could not resolve the real Rocket League server. Proxy routing is paused.",
        ));
    }
    Ok(addresses)
}
pub fn elevate_script(arguments: &str) -> Result<()> {
    let inner = format!("& {{\n{ROUTE_SCRIPT}\n}} {arguments}");
    let encoded = STANDARD.encode(
        inner
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    let command = format!(
        "$ErrorActionPreference='Stop'; Start-Process -FilePath powershell.exe -Verb RunAs -WindowStyle Hidden -ArgumentList '-NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand {encoded}'"
    );
    let mut ps = platform::hidden_command("powershell.exe");
    ps.args(["-NoProfile", "-NonInteractive", "-Command", &command]);
    if !platform::command_output(ps, Duration::from_secs(60))?
        .status
        .success()
    {
        return Err(Error::new(
            "Windows routing permission was declined or unavailable. Save & start to retry.",
        ));
    }
    Ok(())
}
fn listener(address: SocketAddr) -> Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(
        if address.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        },
        Type::STREAM,
        Some(Protocol::TCP),
    )
    .map_err(net_error)?;
    #[cfg(windows)]
    unsafe {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            SO_EXCLUSIVEADDRUSE, SOL_SOCKET, setsockopt,
        };
        let enabled = 1i32;
        if setsockopt(
            s.as_raw_socket() as _,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            &enabled as *const i32 as *const u8,
            4,
        ) != 0
        {
            return Err(Error::new(
                "Could not reserve the proxy listener exclusively.",
            ));
        }
    }
    if address.is_ipv6() {
        s.set_only_v6(true).map_err(net_error)?;
    }
    s.bind(&address.into()).map_err(net_error)?;
    s.listen(16).map_err(net_error)?;
    s.set_nonblocking(true).map_err(net_error)?;
    Ok(s.into())
}
pub struct CaptureProxy {
    dir: PathBuf,
    pub enabled: AtomicBool,
    pub started: AtomicBool,
    stop: AtomicBool,
    valid_trust: AtomicBool,
    pub broker: Broker,
    connections: AtomicUsize,
    detail: Mutex<String>,
    nonce: String,
    route: Mutex<String>,
    trust_at: Mutex<Instant>,
    dns: Mutex<(Instant, Vec<IpAddr>)>,
    ipv6: AtomicBool,
    startup: Mutex<()>,
    listeners_alive: AtomicUsize,
}
impl CaptureProxy {
    pub fn new(dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            dir: dir.into(),
            enabled: AtomicBool::new(false),
            started: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            valid_trust: AtomicBool::new(false),
            broker: Broker::default(),
            connections: AtomicUsize::new(0),
            detail: Mutex::new("Set up periodic capture before selecting that mode.".into()),
            nonce: hex::encode(rand::random::<[u8; 32]>()),
            route: Mutex::new("not_started".into()),
            trust_at: Mutex::new(Instant::now()),
            dns: Mutex::new((Instant::now(), vec![])),
            ipv6: AtomicBool::new(false),
            startup: Mutex::new(()),
            listeners_alive: AtomicUsize::new(0),
        })
    }
    pub fn detail(&self) -> String {
        self.detail.lock().unwrap().clone()
    }
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }
    pub fn close_allowed(&self) -> bool {
        !(self.started.load(Ordering::Relaxed)
            && (self.enabled.load(Ordering::Relaxed) || self.connections() > 0)
            && platform::game_running() != Some(false))
    }
    fn status(&self, s: impl Into<String>) {
        *self.detail.lock().unwrap() = s.into();
    }
    fn addresses(&self) -> Result<Vec<IpAddr>> {
        let mut dns = self.dns.lock().unwrap();
        if dns.1.is_empty() || dns.0.elapsed() > Duration::from_secs(60) {
            *dns = (Instant::now(), resolve_upstream()?);
        }
        Ok(dns.1.clone())
    }
    pub fn start(self: &Arc<Self>) -> Result<()> {
        let _startup = self.startup.lock().unwrap();
        if !self.enabled.load(Ordering::Relaxed) {
            return Ok(());
        }
        if self.started.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.stop.store(false, Ordering::Relaxed);
        let setup = (|| {
            if !proxy_certs::trusted(&self.dir) {
                return Err(Error::new(
                    "Periodic capture needs certificate setup. Choose Set up periodic capture.",
                ));
            }
            proxy_certs::prepare(&self.dir)?;
            self.addresses()?;
            let config = proxy_certs::server_config(&self.dir)?;
            let local = listener("127.0.0.1:443".parse().unwrap())?;
            let ipv6 = listener("[::1]:443".parse().unwrap()).ok();
            let health = listener(
                format!("127.0.0.1:{}", proxy_certs::HEALTH_PORT)
                    .parse()
                    .unwrap(),
            )?;
            self.valid_trust.store(true, Ordering::Relaxed);
            *self.trust_at.lock().unwrap() = Instant::now();
            self.ipv6.store(ipv6.is_some(), Ordering::Relaxed);
            self.started.store(true, Ordering::Relaxed);
            let proxy = self.clone();
            std::thread::spawn(move || proxy.health_loop(health));
            for sock in std::iter::once(local).chain(ipv6) {
                let proxy = self.clone();
                let config = config.clone();
                self.listeners_alive.fetch_add(1, Ordering::Relaxed);
                std::thread::spawn(move || proxy.accept_loop(sock, config));
            }
            self.status("Proxy ready. Approve Windows administrator access for hostname routing.");
            elevate_script(&format!(
                "-OwnerPid {} -OwnerStart {} -Nonce {}",
                std::process::id(),
                platform::process_birth()?,
                self.nonce
            ))?;
            self.status("Proxy ready. Launch Rocket League to capture its session.");
            Ok(())
        })();
        if let Err(e) = &setup {
            self.close();
            self.status(e.message.clone());
        }
        setup
    }
    pub fn close(&self) {
        self.enabled.store(false, Ordering::Relaxed);
        self.started.store(false, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
        if let Some(s) = self.broker.session() {
            s.close();
        }
    }
    fn accept_loop(self: Arc<Self>, listener: TcpListener, config: Arc<rustls::ServerConfig>) {
        while !self.stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    if self.connections.fetch_add(1, Ordering::Relaxed) >= 16 {
                        self.connections.fetch_sub(1, Ordering::Relaxed);
                        continue;
                    }
                    let p = self.clone();
                    let c = config.clone();
                    std::thread::spawn(move || {
                        if p.connection(stream, c).is_err() {
                            p.status("Game proxy connection ended. Waiting for reconnect; no second login was attempted.");
                        }
                        p.connections.fetch_sub(1, Ordering::Relaxed);
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(40))
                }
                Err(_) => break,
            }
        }
        self.listeners_alive.fetch_sub(1, Ordering::Relaxed);
    }
    fn connection(&self, stream: TcpStream, config: Arc<rustls::ServerConfig>) -> Result<()> {
        let mut local = Endpoint::new(
            stream,
            rustls::Connection::Server(rustls::ServerConnection::new(config).map_err(net_error)?),
        )?;
        let (raw, first, headers) = read_headers(&mut local)?;
        validate_handshake(&first, &headers)?;
        let config = proxy_certs::client_config()?;
        let mut upstream = None;
        for ip in self.addresses()? {
            if let Ok(tcp) =
                TcpStream::connect_timeout(&SocketAddr::new(ip, 443), Duration::from_secs(10))
            {
                let conn = rustls::ClientConnection::new(config.clone(), HOST.try_into().unwrap())
                    .map_err(net_error)?;
                if let Ok(ep) = Endpoint::new(tcp, rustls::Connection::Client(conn)) {
                    upstream = Some(ep);
                    break;
                }
            }
        }
        let mut upstream =
            upstream.ok_or_else(|| Error::new("Real game server could not be reached."))?;
        let request: Vec<_> = raw
            .split(|&b| b == b'\n')
            .filter(|line| {
                !line
                    .to_ascii_lowercase()
                    .starts_with(b"sec-websocket-extensions:")
            })
            .collect();
        upstream
            .write_all(&request.join(&b'\n'))
            .map_err(net_error)?;
        upstream.flush().map_err(net_error)?;
        let (response, status, accepted) = read_headers(&mut upstream)?;
        let expected = STANDARD.encode(Sha1::digest(
            format!(
                "{}258EAFA5-E914-47DA-95CA-C5AB0DC85B11",
                header(&headers, "sec-websocket-key")
            )
            .as_bytes(),
        ));
        if !status.starts_with("HTTP/1.1 101 ")
            || header(&accepted, "sec-websocket-accept") != expected
            || accepted.contains_key("sec-websocket-extensions")
        {
            return Err(Error::new("Upstream WebSocket upgrade rejected."));
        }
        local.write_all(&response).map_err(net_error)?;
        local.flush().map_err(net_error)?;
        local.socket.set_nonblocking(true).map_err(net_error)?;
        upstream.socket.set_nonblocking(true).map_err(net_error)?;
        let (session, requests) = Session::new();
        self.broker.attach(session.clone());
        self.status("Connected to Rocket League's existing session.");
        let result = (|| {
            let mut frames = RelayFrames::default();
            let mut pending = VecDeque::new();
            while !self.stop.load(Ordering::Relaxed) && session.alive.load(Ordering::Relaxed) {
                local.pump()?;
                upstream.pump()?;
                while let Some(f) = proxy_wire::next_frame(&mut local.incoming, true)? {
                    upstream.queue(frames.from_game(f)?)?;
                }
                while let Some(f) = proxy_wire::next_frame(&mut upstream.incoming, false)? {
                    if let Some(raw) = frames.from_server(f, &session)? {
                        local.queue(raw)?;
                    }
                }
                pending.extend(requests.try_iter());
                if frames.can_inject() {
                    while let Some(req) = pending.pop_front() {
                        if req.active.load(Ordering::Relaxed)
                            && self.enabled.load(Ordering::Relaxed)
                            && self.valid_trust.load(Ordering::Relaxed)
                            && platform::game_running() == Some(true)
                        {
                            upstream.queue(req.payload)?;
                        }
                    }
                }
                local.pump_output()?;
                upstream.pump_output()?;
                std::thread::sleep(Duration::from_millis(3));
            }
            Ok(())
        })();
        session.close();
        self.broker.detach(&session);
        result
    }
    fn health_loop(self: Arc<Self>, listener: TcpListener) {
        while !self.stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    let _ = self.health_request(&mut stream);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(40))
                }
                Err(_) => break,
            }
        }
    }
    fn health_request(&self, stream: &mut TcpStream) -> Result<()> {
        let (_, first, headers) = read_headers(stream)?;
        let supplied = header(&headers, "x-rlu-nonce").as_bytes();
        let expected = self.nonce.as_bytes();
        let authorized = supplied.len() == expected.len()
            && supplied
                .iter()
                .zip(expected)
                .fold(0u8, |x, (a, b)| x | (a ^ b))
                == 0;
        let (status, content_type, data) = if first == "GET /rl-uploader.crl HTTP/1.1" {
            (
                200,
                "application/pkix-crl",
                std::fs::read(self.dir.join("proxy/rl-uploader.crl")).map_err(io_error)?,
            )
        } else if first == "GET /health HTTP/1.1" && authorized {
            let mut checked = self.trust_at.lock().unwrap();
            if checked.elapsed() > Duration::from_secs(5) {
                self.valid_trust
                    .store(proxy_certs::trusted(&self.dir), Ordering::Relaxed);
                *checked = Instant::now();
            }
            let ready = self.started.load(Ordering::Relaxed)
                && !self.stop.load(Ordering::Relaxed)
                && self.listeners_alive.load(Ordering::Relaxed) > 0
                && self.valid_trust.load(Ordering::Relaxed)
                && (self.enabled.load(Ordering::Relaxed) || self.connections() > 0);
            (
                200,
                "application/json",
                serde_json::to_vec(
                    &json!({"ready":ready,"ipv6":self.ipv6.load(Ordering::Relaxed)}),
                )
                .unwrap(),
            )
        } else if first == "POST /route-status HTTP/1.1" && authorized {
            let len = header(&headers, "content-length")
                .parse::<usize>()
                .unwrap_or(0);
            if len == 0 || len > 1024 {
                return Err(Error::new("Invalid route status."));
            }
            let mut body = vec![0; len];
            stream.read_exact(&mut body).map_err(net_error)?;
            let v: Value =
                serde_json::from_slice(&body).map_err(|_| Error::new("Invalid route status."))?;
            let state = v["state"].as_str().unwrap_or_default();
            if !["ready", "active", "stopped", "error"].contains(&state) {
                return Err(Error::new("Invalid route status."));
            }
            *self.route.lock().unwrap() = state.into();
            if state == "error" {
                self.status("Proxy routing failed. Close Rocket League and check administrator access or conflicting hostname mappings.");
            }
            (200, "application/json", b"{}".to_vec())
        } else {
            (404, "application/json", b"{}".to_vec())
        };
        write!(stream,"HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",data.len()).map_err(net_error)?;
        stream.write_all(&data).map_err(net_error)
    }
}
impl History for CaptureProxy {
    fn history(&self, guard: &Guard) -> Result<(String, Vec<Value>)> {
        protocol::check(guard)?;
        if !self.enabled.load(Ordering::Relaxed)
            || !self.valid_trust.load(Ordering::Relaxed)
            || self.stop.load(Ordering::Relaxed)
            || platform::game_running() != Some(true)
        {
            return Err(Error::new(
                "Live capture is not ready. No second login will be created.",
            ));
        }
        let session = self.broker.session().ok_or_else(|| Error::new("Waiting for the game's captured session. Start the proxy before launching Rocket League."))?;
        let player = local_player_id()?;
        let matches = session.history(&player, guard, Duration::from_secs(20))?;
        Ok((player, matches))
    }
}
/// One thread owns each pair of TLS states. No concurrent access to a TLS stream.
struct Endpoint {
    socket: TcpStream,
    connection: rustls::Connection,
    incoming: Vec<u8>,
    outgoing: VecDeque<Vec<u8>>,
    offset: usize,
    queued: usize,
}
impl Endpoint {
    fn new(mut socket: TcpStream, mut connection: rustls::Connection) -> Result<Self> {
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(net_error)?;
        socket
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(net_error)?;
        socket.set_nodelay(true).map_err(net_error)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while connection.is_handshaking() {
            if Instant::now() >= deadline {
                return Err(Error::new("TLS handshake timed out."));
            }
            connection.complete_io(&mut socket).map_err(net_error)?;
        }
        Ok(Self {
            socket,
            connection,
            incoming: vec![],
            outgoing: VecDeque::new(),
            offset: 0,
            queued: 0,
        })
    }
    fn queue(&mut self, data: Vec<u8>) -> Result<()> {
        if self.queued + data.len() > proxy_wire::MAX_MESSAGE * 2 {
            return Err(Error::new(
                "Game connection stalled; relay buffer limit reached.",
            ));
        }
        self.queued += data.len();
        self.outgoing.push_back(data);
        Ok(())
    }
    fn pump_output(&mut self) -> Result<()> {
        while let Some(front) = self.outgoing.front() {
            let n = match self.connection.writer().write(&front[self.offset..]) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
                Err(e) => return Err(net_error(e)),
            };
            if n == 0 {
                break;
            }
            self.offset += n;
            self.queued -= n;
            if self.offset == front.len() {
                self.outgoing.pop_front();
                self.offset = 0;
            }
        }
        while self.connection.wants_write() {
            match self.connection.write_tls(&mut self.socket) {
                Ok(0) => return Err(Error::new("Game connection closed.")),
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(net_error(e)),
            }
        }
        Ok(())
    }
    fn pump(&mut self) -> Result<()> {
        match self.connection.read_tls(&mut self.socket) {
            Ok(0) => return Err(Error::new("Game connection closed.")),
            Ok(_) => {
                self.connection.process_new_packets().map_err(net_error)?;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
            Err(e) => return Err(net_error(e)),
        }
        let mut buffer = [0u8; 65536];
        loop {
            match self.connection.reader().read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    self.incoming.extend_from_slice(&buffer[..n]);
                    if self.incoming.len() > proxy_wire::MAX_MESSAGE {
                        return Err(Error::new("Game connection exceeded buffer limits."));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(net_error(e)),
            }
        }
        Ok(())
    }
}
impl Read for Endpoint {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.connection.reader().read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.connection.complete_io(&mut self.socket)?;
                }
                other => return other,
            }
        }
    }
}
impl Write for Endpoint {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.connection.writer().write(buf)?;
        self.flush()?;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        while self.connection.wants_write() {
            self.connection.write_tls(&mut self.socket)?;
        }
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod tls_tests {
    use super::*;

    fn client(
        dir: &Path,
        name: &str,
        address: SocketAddr,
    ) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(std::fs::read(dir.join("proxy/root.cer")).unwrap().into())
            .unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let connection =
            rustls::ClientConnection::new(Arc::new(config), name.to_owned().try_into().unwrap())
                .unwrap();
        let socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        rustls::StreamOwned::new(connection, socket)
    }

    #[test]
    fn tls_endpoint_moves_large_frames_with_nonblocking_backpressure() {
        let dir = tempfile::tempdir().unwrap();
        proxy_certs::prepare(dir.path()).unwrap();
        let config = proxy_certs::server_config(dir.path()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut peer = Endpoint::new(
                tcp,
                rustls::Connection::Server(rustls::ServerConnection::new(config).unwrap()),
            )
            .unwrap();
            let (_, first, _) = read_headers(&mut peer).unwrap();
            assert_eq!(first, "GET /ws/gc2 HTTP/1.1");
            peer.write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
                .unwrap();
            peer.socket.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut echo = false;
            loop {
                assert!(Instant::now() < deadline, "TLS relay stalled");
                peer.pump().unwrap();
                if !echo {
                    if let Some(frame) = proxy_wire::next_frame(&mut peer.incoming, true).unwrap() {
                        assert_eq!(frame.payload, vec![0xa5; 262144]);
                        peer.queue(frame.raw).unwrap();
                        echo = true;
                    }
                } else if peer.incoming == b"ack" {
                    break;
                }
                peer.pump_output().unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let mut client = client(dir.path(), HOST, address);
        client
            .write_all(b"GET /ws/gc2 HTTP/1.1\r\nHost: ws.rlpp.psynet.gg\r\n\r\n")
            .unwrap();
        client.flush().unwrap();
        assert!(read_headers(&mut client).unwrap().1.contains("101"));
        let raw = proxy_wire::encode_frame(&vec![0xa5; 262144], 2, true, true);
        for part in raw.chunks(7777) {
            client.write_all(part).unwrap();
        }
        client.flush().unwrap();
        let mut echo = vec![0u8; raw.len()];
        client.read_exact(&mut echo).unwrap();
        assert_eq!(raw, echo);
        client.write_all(b"ack").unwrap();
        client.flush().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn proxy_tls_rejects_a_different_server_name() {
        let dir = tempfile::tempdir().unwrap();
        proxy_certs::prepare(dir.path()).unwrap();
        let config = proxy_certs::server_config(dir.path()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            assert!(
                Endpoint::new(
                    tcp,
                    rustls::Connection::Server(rustls::ServerConnection::new(config).unwrap())
                )
                .is_err()
            );
        });
        let mut client = client(dir.path(), "unrelated.example", address);
        assert!(client.write_all(b"hello").is_err());
        server.join().unwrap();
    }
}
