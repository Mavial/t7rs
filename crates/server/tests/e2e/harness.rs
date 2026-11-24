//! What the end-to-end tests share: raw protocol clients, voice endpoints, and
//! servers started on localhost.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use t7_proto::identity::IdentityKey;
use t7_proto::perms::{self, Perm, Perms, RoleChange, RoleId, Roles};
use t7_proto::*;
use t7_server::config::{Config, DEFAULT_CONFIG};
use tokio::io::{BufReader, ReadHalf, WriteHalf};
use tokio::net::{TcpSocket, TcpStream};

type Tls = tls::ClientStream<TcpStream>;

pub(crate) struct TestClient {
    r: BufReader<ReadHalf<Tls>>,
    w: WriteHalf<Tls>,
    buf: Vec<u8>,
    /// What this connection's identity proof signs.
    pub(crate) exporter: [u8; 32],
}

/// One key per nickname, so reconnecting under a name is the same person.
pub(crate) fn key(nick: &str) -> IdentityKey {
    let mut seed = [0u8; 32];
    seed.iter_mut().zip(nick.bytes()).for_each(|(s, b)| *s = b);
    IdentityKey::from_seed(seed)
}

/// Loopback addresses for clients that must not share an IP.
pub(crate) const HOME: Ipv4Addr = Ipv4Addr::LOCALHOST;
pub(crate) const ADMIN_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
pub(crate) const ELSEWHERE: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 3);

impl TestClient {
    pub(crate) async fn connect(addr: SocketAddr, nick: &str, pw: Option<&str>) -> (Self, S2C) {
        Self::connect_as(addr, nick, pw, PROTOCOL_VERSION).await
    }
    pub(crate) async fn connect_as(addr: SocketAddr, nick: &str, pw: Option<&str>, protocol: u16) -> (Self, S2C) {
        let c = Self::open(addr, HOME).await;
        let proof = key(nick).prove(&c.exporter);
        c.hello(nick, pw, protocol, proof).await
    }
    /// From another loopback address, with any key.
    pub(crate) async fn connect_from(addr: SocketAddr, from: Ipv4Addr, nick: &str, key: &IdentityKey) -> (Self, S2C) {
        let c = Self::open(addr, from).await;
        let proof = key.prove(&c.exporter);
        c.hello(nick, None, PROTOCOL_VERSION, proof).await
    }
    /// A TLS connection that hasn't said Hello yet.
    pub(crate) async fn open(addr: SocketAddr, from: Ipv4Addr) -> Self {
        let sock = TcpSocket::new_v4().unwrap();
        sock.bind((from, 0).into()).unwrap();
        let (tls, _) = tls::connect(sock.connect(addr).await.unwrap(), None).await.unwrap();
        let exporter = tls::exporter(tls.get_ref().1);
        let (r, w) = tokio::io::split(tls);
        Self { r: BufReader::new(r), w, buf: Vec::new(), exporter }
    }
    pub(crate) async fn hello(
        mut self,
        nick: &str,
        pw: Option<&str>,
        protocol: u16,
        identity: IdentityProof,
    ) -> (Self, S2C) {
        self.send(C2S::Hello { protocol, nickname: nick.into(), password: pw.map(Into::into), identity }).await;
        let first = self.recv().await;
        (self, first)
    }
    pub(crate) async fn send(&mut self, m: C2S) {
        write_frame(&mut self.w, &m).await.unwrap();
    }
    pub(crate) async fn recv(&mut self) -> S2C {
        tokio::time::timeout(Duration::from_secs(2), read_frame(&mut self.r, &mut self.buf))
            .await
            .expect("timed out waiting for message")
            .unwrap()
    }
    /// Reads until the server hangs up.
    pub(crate) async fn closed(&mut self) {
        let read = async { while read_frame::<_, S2C>(&mut self.r, &mut self.buf).await.is_ok() {} };
        tokio::time::timeout(Duration::from_secs(2), read).await.expect("the server kept the connection open");
    }
    /// Skips messages until one matches.
    pub(crate) async fn until(&mut self, want: impl Fn(&S2C) -> bool) -> S2C {
        loop {
            let m = self.recv().await;
            if want(&m) {
                return m;
            }
        }
    }
    pub(crate) async fn channels(&mut self) -> Vec<ChannelInfo> {
        match self.until(|m| matches!(m, S2C::Channels(_))).await {
            S2C::Channels(list) => list,
            _ => unreachable!(),
        }
    }
    pub(crate) async fn sync(&mut self) -> Vec<S2C> {
        self.send(C2S::Ping { nonce: 0 }).await;
        let mut before = Vec::new();
        loop {
            match self.recv().await {
                S2C::Pong { nonce: 0 } => return before,
                m => before.push(m),
            }
        }
    }

    pub(crate) async fn change_roles(&mut self, change: RoleChange) -> Roles {
        self.sync().await;
        self.send(C2S::ChangeRoles(change)).await;
        match self.until(|m| matches!(m, S2C::Roles(_) | S2C::Error { .. })).await {
            S2C::Roles(roles) => roles,
            other => panic!("refused: {other:?}"),
        }
    }
    pub(crate) async fn overrides(&mut self, channel: ChannelId) -> Vec<perms::Override> {
        let list = self
            .until(|m| matches!(m, S2C::Channels(l) if l.iter().any(|c| c.id == channel && !c.overrides.is_empty())))
            .await;
        let S2C::Channels(list) = list else { unreachable!() };
        overrides_in(&list, channel)
    }
}

pub(crate) async fn start(cfg_text: &str) -> SocketAddr {
    start_in(cfg_text, None).await.0
}

/// A server keeping its files in `dir`; abort the task to stop it.
pub(crate) async fn start_in(cfg_text: &str, dir: Option<PathBuf>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    serve(cfg_text, dir, None).await
}

/// Starts a server; `owned` is removed once the server task ends, which at the
/// latest is when the test's runtime shuts down.
async fn serve(
    cfg_text: &str,
    dir: Option<PathBuf>,
    owned: Option<DataDir>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let cfg: Config = toml::from_str(cfg_text).unwrap();
    // The server also binds UDP on the TCP port's number, so pick a port free for both.
    let listener = loop {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        if std::net::UdpSocket::bind(l.local_addr().unwrap()).is_ok() {
            break l;
        }
    };
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _owned = owned;
        if let Err(e) = t7_server::serve(listener, &cfg, dir.as_deref()).await {
            panic!("test server failed: {e}");
        }
    });
    (addr, task)
}

/// A voice endpoint: one UDP socket per test client.
pub(crate) struct Voice {
    pub(crate) sock: tokio::net::UdpSocket,
    pub(crate) id: ClientId,
    crypto: voice::ClientSide,
    /// The last datagram sent, as it went on the wire.
    pub(crate) sent: Vec<u8>,
}

impl Voice {
    pub(crate) async fn open(server: SocketAddr, welcome: &S2C) -> Self {
        let S2C::Welcome { your_id, voice_key, .. } = welcome else { panic!("{welcome:?}") };
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(server).await.unwrap();
        let crypto = voice::ClientSide::new(*your_id, voice_key);
        let mut v = Self { sock, id: *your_id, crypto, sent: Vec::new() };
        // The ping registers our UDP address with the server.
        v.send(voice::ToServer::Ping { nonce: 5 }).await;
        assert_eq!(v.recv_owned().await, Some(Got::Pong(5)));
        v
    }
    pub(crate) async fn send(&mut self, msg: voice::ToServer<'_>) {
        self.crypto.seal(msg, &mut self.sent);
        self.sock.send(&self.sent).await.unwrap();
    }
    pub(crate) async fn recv_owned(&mut self) -> Option<Got> {
        let mut buf = [0u8; 1500];
        let n = tokio::time::timeout(Duration::from_millis(300), self.sock.recv(&mut buf)).await.ok()?.ok()?;
        match self.crypto.open(&mut buf[..n])? {
            voice::ToClient::Pong { nonce } => Some(Got::Pong(nonce)),
            voice::ToClient::Audio { from, seq, payload } => Some(Got::Audio(from, seq, payload.to_vec())),
            voice::ToClient::Video { from, frame, part, parts, key, payload } => {
                Some(Got::Video { from, frame, part, parts, key, payload: payload.to_vec() })
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum Got {
    Pong(u64),
    Audio(ClientId, u32, Vec<u8>),
    Video { from: ClientId, frame: u32, part: u16, parts: u16, key: bool, payload: Vec<u8> },
}

pub(crate) fn settings(name: &str) -> ChannelSettings {
    ChannelSettings { name: name.into(), topic: String::new(), max_clients: None, password: PasswordChange::Keep }
}

pub(crate) fn id_of(list: &[ChannelInfo], name: &str) -> ChannelId {
    list.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no channel {name}")).id
}

/// A test's own data directory in the temp folder, removed when dropped, so
/// also when the test fails.
pub(crate) struct DataDir(PathBuf);

impl std::ops::Deref for DataDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for DataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn data_dir(test: &str) -> DataDir {
    let dir = std::env::temp_dir().join(format!("t7-e2e-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    DataDir(dir)
}

pub(crate) fn claim_token(dir: &Path) -> String {
    let text = std::fs::read_to_string(dir.join("roles.toml")).expect("roles.toml");
    let file: toml::Table = toml::from_str(&text).unwrap();
    file.get("claim_token").and_then(|t| t.as_str()).unwrap_or_else(|| panic!("no claim token: {text}")).to_owned()
}

pub(crate) async fn claim(addr: SocketAddr, dir: &Path, from: Ipv4Addr) -> (TestClient, ClientId, Vec<ChannelInfo>) {
    let (mut owner, welcome) = TestClient::connect_from(addr, from, "admin", &key("admin")).await;
    let S2C::Welcome { your_id, channels, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    owner.send(C2S::ClaimOwnership { token: claim_token(dir) }).await;
    owner.until(|m| *m == S2C::ClientRole { id: your_id, role: Some("Owner".into()) }).await;
    (owner, your_id, channels)
}

pub(crate) async fn start_owned(test: &str, from: Ipv4Addr) -> (SocketAddr, TestClient, ClientId, Vec<ChannelInfo>) {
    let dir = data_dir(test);
    let path = dir.to_path_buf();
    let (addr, _) = serve(DEFAULT_CONFIG, Some(path.clone()), Some(dir)).await;
    let (owner, id, channels) = claim(addr, &path, from).await;
    (addr, owner, id, channels)
}

pub(crate) fn welcomed_id(welcome: &S2C) -> ClientId {
    match welcome {
        S2C::Welcome { your_id, .. } => *your_id,
        other => panic!("not welcomed: {other:?}"),
    }
}

pub(crate) fn rejected(reply: &S2C) -> &str {
    match reply {
        S2C::Rejected { reason } => reason,
        other => panic!("not rejected: {other:?}"),
    }
}

pub(crate) async fn error(c: &mut TestClient) -> String {
    match c.until(|m| matches!(m, S2C::Error { .. })).await {
        S2C::Error { message } => message,
        _ => unreachable!(),
    }
}

pub(crate) fn assign(nick: &str, role: RoleId, held: bool) -> RoleChange {
    RoleChange::Assign { identity: key(nick).identity(), role, held }
}

pub(crate) const PRIVATE: &str = r#"
[[channels]]
name = "Lobby"
default = true

[[channels]]
name = "Staff"

  [[channels.overrides]]
  role = 0
  deny = ["view"]

  [[channels.overrides]]
  role = 2
  allow = ["view"]

  [[channels.children]]
  name = "Office"

[[channels]]
name = "AFK"
"#;

pub(crate) fn new_channel(name: &str, parent: Option<ChannelId>, kind: ChannelKind) -> C2S {
    C2S::CreateChannel { parent, settings: settings(name), temporary: false, kind }
}

pub(crate) fn set(channel: ChannelId, role: RoleId, allow: &[Perm], deny: &[Perm]) -> C2S {
    C2S::SetOverride { channel, role, allow: Perms::of(allow), deny: Perms::of(deny) }
}

pub(crate) fn overrides_in(list: &[ChannelInfo], channel: ChannelId) -> Vec<perms::Override> {
    list.iter().find(|c| c.id == channel).unwrap_or_else(|| panic!("no channel {channel}")).overrides.clone()
}
