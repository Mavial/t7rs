//! Spins up a real server on localhost and drives it with raw protocol clients.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use t7_proto::identity::IdentityKey;
use t7_proto::*;
use t7_server::config::{Config, DEFAULT_CONFIG};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::{TcpSocket, TcpStream};

type Tls = tls::ClientStream<TcpStream>;

struct TestClient {
    r: BufReader<ReadHalf<Tls>>,
    w: WriteHalf<Tls>,
    buf: Vec<u8>,
    /// What this connection's identity proof signs.
    exporter: [u8; 32],
}

/// One key per nickname, so reconnecting under a name is the same person.
fn key(nick: &str) -> IdentityKey {
    let mut seed = [0u8; 32];
    seed.iter_mut().zip(nick.bytes()).for_each(|(s, b)| *s = b);
    IdentityKey::from_seed(seed)
}

/// Loopback addresses for clients that must not share an IP.
const HOME: Ipv4Addr = Ipv4Addr::LOCALHOST;

impl TestClient {
    async fn connect(addr: SocketAddr, nick: &str, pw: Option<&str>) -> (Self, S2C) {
        Self::connect_as(addr, nick, pw, PROTOCOL_VERSION).await
    }
    async fn connect_as(addr: SocketAddr, nick: &str, pw: Option<&str>, protocol: u16) -> (Self, S2C) {
        let c = Self::open(addr, HOME).await;
        let proof = key(nick).prove(&c.exporter);
        c.hello(nick, pw, protocol, proof).await
    }
    /// From another loopback address, with any key.
    async fn connect_from(addr: SocketAddr, from: Ipv4Addr, nick: &str, key: &IdentityKey) -> (Self, S2C) {
        let c = Self::open(addr, from).await;
        let proof = key.prove(&c.exporter);
        c.hello(nick, None, PROTOCOL_VERSION, proof).await
    }
    /// A TLS connection that hasn't said Hello yet.
    async fn open(addr: SocketAddr, from: Ipv4Addr) -> Self {
        let sock = TcpSocket::new_v4().unwrap();
        sock.bind((from, 0).into()).unwrap();
        let (tls, _) = tls::connect(sock.connect(addr).await.unwrap(), None).await.unwrap();
        let exporter = tls::exporter(tls.get_ref().1);
        let (r, w) = tokio::io::split(tls);
        Self { r: BufReader::new(r), w, buf: Vec::new(), exporter }
    }
    async fn hello(mut self, nick: &str, pw: Option<&str>, protocol: u16, identity: IdentityProof) -> (Self, S2C) {
        self.send(C2S::Hello { protocol, nickname: nick.into(), password: pw.map(Into::into), identity }).await;
        let first = self.recv().await;
        (self, first)
    }
    async fn send(&mut self, m: C2S) {
        write_frame(&mut self.w, &m).await.unwrap();
    }
    async fn recv(&mut self) -> S2C {
        tokio::time::timeout(Duration::from_secs(2), read_frame(&mut self.r, &mut self.buf))
            .await
            .expect("timed out waiting for message")
            .unwrap()
    }
    /// Reads until the server hangs up.
    async fn closed(&mut self) {
        let read = async {
            while read_frame::<_, S2C>(&mut self.r, &mut self.buf).await.is_ok() {}
        };
        tokio::time::timeout(Duration::from_secs(2), read).await.expect("the server kept the connection open");
    }
}

async fn start(cfg_text: &str) -> SocketAddr {
    start_in(cfg_text, None).await.0
}

/// A server keeping its files in `dir`; abort the task to stop it.
async fn start_in(cfg_text: &str, dir: Option<PathBuf>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
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
        if let Err(e) = t7_server::serve(listener, &cfg, dir.as_deref()).await {
            panic!("test server failed: {e}");
        }
    });
    (addr, task)
}

#[tokio::test]
async fn join_move_chat_leave() {
    let addr = start(DEFAULT_CONFIG).await;

    let (mut a, welcome) = TestClient::connect(addr, "alice", None).await;
    let S2C::Welcome { your_id: a_id, channels, clients, .. } = welcome else { panic!("{welcome:?}") };
    assert_eq!(clients.len(), 1);
    assert_eq!(channels.len(), 6);

    // Duplicate nickname is auto-suffixed.
    let (mut b, welcome) = TestClient::connect(addr, "Alice", None).await;
    let S2C::Welcome { your_id: b_id, clients, .. } = welcome else { panic!() };
    assert_eq!(clients.iter().find(|c| c.id == b_id).unwrap().nickname, "Alice1");
    assert!(matches!(a.recv().await, S2C::ClientJoined(ref c) if c.id == b_id));

    // Channel chat reaches both (same default channel).
    a.send(C2S::SendText { target: TextTarget::Channel, text: "hi".into() }).await;
    for c in [&mut a, &mut b] {
        assert!(matches!(c.recv().await, S2C::Text { from, ref text, .. } if from == a_id && text == "hi"));
    }

    // b moves away; channel chat no longer reaches b, but server chat does.
    b.send(C2S::JoinChannel { channel: 4, password: None }).await;
    for c in [&mut a, &mut b] {
        assert_eq!(c.recv().await, S2C::ClientMoved { id: b_id, channel: 4 });
    }
    a.send(C2S::SendText { target: TextTarget::Channel, text: "only me".into() }).await;
    assert!(matches!(a.recv().await, S2C::Text { .. }));
    a.send(C2S::SendText { target: TextTarget::Server, text: "all".into() }).await;
    assert!(matches!(b.recv().await, S2C::Text { target: TextTarget::Server, ref text, .. } if text == "all"));

    // Ping/pong.
    b.send(C2S::Ping { nonce: 42 }).await;
    assert_eq!(b.recv().await, S2C::Pong { nonce: 42 });

    // Leaving is broadcast.
    drop(b);
    assert!(matches!(a.recv().await, S2C::Text { .. })); // own server message echo
    assert!(matches!(a.recv().await, S2C::ClientLeft { id, .. } if id == b_id));
}

#[tokio::test]
async fn full_channel_and_password() {
    let cfg = r#"
        password = "secret"
        [[channels]]
        name = "Lobby"
        [[channels]]
        name = "Solo"
        max_clients = 1
    "#;
    let addr = start(cfg).await;

    let (_, rejected) = TestClient::connect(addr, "x", Some("nope")).await;
    assert!(matches!(rejected, S2C::Rejected { .. }));

    let (mut a, _) = TestClient::connect(addr, "a", Some("secret")).await;
    let (mut b, _) = TestClient::connect(addr, "b", Some("secret")).await;
    a.recv().await; // b joined
    a.send(C2S::JoinChannel { channel: 2, password: None }).await;
    a.recv().await;
    b.recv().await;
    b.send(C2S::JoinChannel { channel: 2, password: None }).await;
    assert!(matches!(b.recv().await, S2C::Error { .. }));
}

/// A voice endpoint: one UDP socket per test client.
struct Voice {
    sock: tokio::net::UdpSocket,
    id: ClientId,
    crypto: voice::ClientSide,
    /// The last datagram sent, as it went on the wire.
    sent: Vec<u8>,
}

impl Voice {
    async fn open(server: std::net::SocketAddr, welcome: &S2C) -> Self {
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
    async fn send(&mut self, msg: voice::ToServer<'_>) {
        self.crypto.seal(msg, &mut self.sent);
        self.sock.send(&self.sent).await.unwrap();
    }
    async fn recv_owned(&mut self) -> Option<Got> {
        let mut buf = [0u8; 1500];
        let n = tokio::time::timeout(Duration::from_millis(300), self.sock.recv(&mut buf)).await.ok()?.ok()?;
        match self.crypto.open(&mut buf[..n])? {
            voice::ToClient::Pong { nonce } => Some(Got::Pong(nonce)),
            voice::ToClient::Audio { from, seq, payload } => Some(Got::Audio(from, seq, payload.to_vec())),
        }
    }
}

#[derive(Debug, PartialEq)]
enum Got {
    Pong(u64),
    Audio(ClientId, u32, Vec<u8>),
}

#[tokio::test]
async fn voice_is_routed_encrypted_within_the_channel() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut a, wa) = TestClient::connect(addr, "a", None).await;
    let (mut b, wb) = TestClient::connect(addr, "b", None).await;
    let (mut c, wc) = TestClient::connect(addr, "c", None).await;
    let (mut va, mut vb, mut vc) =
        (Voice::open(addr, &wa).await, Voice::open(addr, &wb).await, Voice::open(addr, &wc).await);

    // c leaves for another channel; wait until everyone has seen the move.
    c.send(C2S::JoinChannel { channel: 4, password: None }).await;
    for t in [&mut a, &mut b, &mut c] {
        while !matches!(t.recv().await, S2C::ClientMoved { .. }) {}
    }

    // Each listener opens the audio with its own key.
    va.send(voice::ToServer::Audio { seq: 1, payload: &[9, 9, 9] }).await;
    assert_eq!(vb.recv_owned().await, Some(Got::Audio(va.id, 1, vec![9, 9, 9])));
    assert_eq!(vc.recv_owned().await, None, "other channel must not hear it");
    assert_eq!(va.recv_owned().await, None, "speaker must not hear itself");

    // A replayed datagram is dropped.
    va.sock.send(&va.sent).await.unwrap();
    assert_eq!(vb.recv_owned().await, None, "replay was forwarded");

    // So is one sealed with another key, even with a's id on it.
    let mut forger = voice::ClientSide::new(va.id, &[0x42; 32]);
    let mut forged = Vec::new();
    forger.seal(voice::ToServer::Audio { seq: 2, payload: &[1] }, &mut forged);
    va.sock.send(&forged).await.unwrap();
    assert_eq!(vb.recv_owned().await, None, "forgery was forwarded");

    // Neither disturbed the real sender.
    va.send(voice::ToServer::Audio { seq: 2, payload: &[2] }).await;
    assert_eq!(vb.recv_owned().await, Some(Got::Audio(va.id, 2, vec![2])));

    // A deafened listener gets nothing; a muted speaker is not forwarded.
    b.send(C2S::SetStatus(ClientStatus { deafened: true, ..Default::default() })).await;
    while !matches!(a.recv().await, S2C::ClientStatus { .. }) {}
    va.send(voice::ToServer::Audio { seq: 3, payload: &[1] }).await;
    assert_eq!(vb.recv_owned().await, None);

    b.send(C2S::SetStatus(ClientStatus::default())).await;
    a.send(C2S::SetStatus(ClientStatus { muted: true, ..Default::default() })).await;
    let mut seen = 0;
    while seen < 2 {
        if matches!(b.recv().await, S2C::ClientStatus { .. }) {
            seen += 1;
        }
    }
    va.send(voice::ToServer::Audio { seq: 4, payload: &[1] }).await;
    assert_eq!(vb.recv_owned().await, None);
}

#[tokio::test]
async fn pokes_and_protocol_versions() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut a, wa) = TestClient::connect(addr, "a", None).await;
    let (mut b, wb) = TestClient::connect(addr, "b", None).await;
    let (S2C::Welcome { your_id: a_id, .. }, S2C::Welcome { your_id: b_id, .. }) = (&wa, &wb) else {
        panic!("not welcomed");
    };
    a.recv().await; // b joined

    a.send(C2S::Poke { target: *b_id, message: "  over here  ".into() }).await;
    assert_eq!(b.recv().await, S2C::Poked { from: *a_id, message: "over here".into() });

    a.send(C2S::Poke { target: *a_id, message: String::new() }).await;
    assert!(matches!(a.recv().await, S2C::Error { .. }), "can't poke yourself");

    a.send(C2S::Poke { target: *b_id, message: "x".repeat(MAX_POKE_LEN + 1) }).await;
    assert!(matches!(a.recv().await, S2C::Error { .. }));

    let (_, too_old) = TestClient::connect_as(addr, "old", None, MIN_PROTOCOL_VERSION - 1).await;
    assert!(matches!(too_old, S2C::Rejected { ref reason } if reason.contains("update your T7")));
    let (_, too_new) = TestClient::connect_as(addr, "future", None, PROTOCOL_VERSION + 1).await;
    assert!(matches!(too_new, S2C::Rejected { ref reason } if reason.contains("server needs an update")));
}

/// The certificate fingerprint a server presents.
async fn fingerprint(addr: std::net::SocketAddr) -> tls::Fingerprint {
    tls::connect(TcpStream::connect(addr).await.unwrap(), None).await.unwrap().1
}

#[tokio::test]
async fn clients_without_tls_are_told_to_update() {
    let addr = start(DEFAULT_CONFIG).await;
    // A protocol 4 client: a plain frame, no TLS. Its Hello frame length (22)
    // starts with the same byte as a TLS record.
    let mut old = TcpStream::connect(addr).await.unwrap();
    // Protocol 4's Hello, which had no identity.
    #[derive(serde::Serialize)]
    enum OldC2S {
        Hello { protocol: u16, nickname: String, password: Option<String> },
    }
    let hello = OldC2S::Hello { protocol: 4, nickname: "eighteen_char_nick".into(), password: None };
    let frame = encode_frame(&hello);
    assert_eq!(frame[0], tls::HANDSHAKE_RECORD);
    old.write_all(&frame).await.unwrap();
    let reply: S2C = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut old, &mut Vec::new()))
        .await
        .expect("no reply")
        .unwrap();
    assert!(matches!(reply, S2C::Rejected { ref reason } if reason.contains("please update your T7")), "{reply:?}");
    let mut rest = Vec::new();
    old.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty(), "the server hangs up after the rejection");

    // TLS clients on the same server are unaffected.
    let (_, welcome) = TestClient::connect(addr, "new", None).await;
    assert!(matches!(welcome, S2C::Welcome { .. }));
}

impl TestClient {
    /// Skips messages until one matches.
    async fn until(&mut self, want: impl Fn(&S2C) -> bool) -> S2C {
        loop {
            let m = self.recv().await;
            if want(&m) {
                return m;
            }
        }
    }
    async fn channels(&mut self) -> Vec<ChannelInfo> {
        match self.until(|m| matches!(m, S2C::Channels(_))).await {
            S2C::Channels(list) => list,
            _ => unreachable!(),
        }
    }
}

/// The default config with a known admin token.
fn with_admin_token(token: &str) -> String {
    DEFAULT_CONFIG.replace(r#"admin_token = """#, &format!(r#"admin_token = "{token}""#))
}

fn settings(name: &str) -> ChannelSettings {
    ChannelSettings { name: name.into(), topic: String::new(), max_clients: None, password: PasswordChange::Keep }
}

fn id_of(list: &[ChannelInfo], name: &str) -> ChannelId {
    list.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no channel {name}")).id
}

#[tokio::test]
async fn channels_are_managed_with_permissions() {
    let cfg = with_admin_token("sekrit");
    let addr = start(&cfg).await;
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let (mut admin, wa) = TestClient::connect(addr, "admin", None).await;
    let (S2C::Welcome { your_id: user_id, channels, .. }, S2C::Welcome { your_id: admin_id, .. }) = (&wu, &wa) else {
        panic!("not welcomed");
    };
    let (user_id, admin_id) = (*user_id, *admin_id);
    let lobby = id_of(channels, "Lobby");
    user.recv().await; // admin joined

    // Anyone can make a temporary channel and is moved into it.
    user.send(C2S::CreateChannel { parent: None, settings: settings("Raid"), temporary: true }).await;
    let list = user.channels().await;
    let raid = id_of(&list, "Raid");
    assert!(list.iter().find(|c| c.id == raid).unwrap().temporary);
    assert_eq!(user.recv().await, S2C::ClientMoved { id: user_id, channel: raid });

    // ...but not a permanent one, and not edit channels they didn't make.
    user.send(C2S::CreateChannel { parent: None, settings: settings("Mine"), temporary: false }).await;
    assert!(matches!(user.until(|m| matches!(m, S2C::Error { .. })).await, S2C::Error { ref message } if message.contains("only admins")));
    user.send(C2S::DeleteChannel { id: lobby }).await;
    assert!(matches!(user.until(|m| matches!(m, S2C::Error { .. })).await, S2C::Error { .. }));

    // The creator may edit their temporary channel, adding a password.
    let locked = ChannelSettings { password: PasswordChange::Set("pw".into()), ..settings("Raid Night") };
    user.send(C2S::EditChannel { id: raid, settings: locked }).await;
    let list = user.channels().await;
    assert!(list.iter().any(|c| c.name == "Raid Night" && c.has_password));

    // A wrong token is refused; the right one makes you admin, and everyone hears about it.
    admin.send(C2S::Authenticate { token: "guess".into() }).await;
    assert!(matches!(admin.until(|m| matches!(m, S2C::Error { .. })).await, S2C::Error { ref message } if message.contains("wrong admin token")));
    admin.send(C2S::Authenticate { token: "sekrit".into() }).await;
    assert_eq!(user.until(|m| matches!(m, S2C::ClientAdmin { .. })).await, S2C::ClientAdmin { id: admin_id, admin: true });

    // Admins skip channel passwords; others need them.
    admin.send(C2S::JoinChannel { channel: raid, password: None }).await;
    assert_eq!(admin.until(|m| matches!(m, S2C::ClientMoved { .. })).await, S2C::ClientMoved { id: admin_id, channel: raid });

    // Admins make permanent channels, rearrange them and pick the default.
    admin.send(C2S::CreateChannel { parent: None, settings: settings("Music"), temporary: false }).await;
    let music = id_of(&admin.channels().await, "Music");
    admin.send(C2S::MoveChannel { id: music, parent: None, position: 0 }).await;
    let list = admin.channels().await;
    assert_eq!(list.iter().find(|c| c.id == music).unwrap().order, 0);
    admin.send(C2S::SetDefaultChannel { id: music }).await;
    assert!(admin.channels().await.iter().any(|c| c.id == music && c.default));

    // Deleting a channel moves the people inside to the default channel.
    user.send(C2S::JoinChannel { channel: lobby, password: None }).await;
    user.until(|m| matches!(m, S2C::ClientMoved { id, .. } if *id == user_id)).await;
    admin.send(C2S::DeleteChannel { id: lobby }).await;
    assert_eq!(user.until(|m| matches!(m, S2C::ClientMoved { .. })).await, S2C::ClientMoved { id: user_id, channel: music });
    assert!(user.channels().await.iter().all(|c| c.id != lobby));

    // The temporary channel vanished when the admin (its last occupant) left it.
    admin.send(C2S::JoinChannel { channel: music, password: None }).await;
    let list = admin.channels().await;
    assert!(list.iter().all(|c| c.id != raid), "empty temporary channel should be gone");
}

/// An empty directory of its own for a test's server files.
fn data_dir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("t7-e2e-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn channel_edits_and_the_certificate_survive_a_restart() {
    let dir = data_dir("restart");
    let cfg: Config = toml::from_str(&with_admin_token("t")).unwrap();
    let cfg_text = with_admin_token("t");
    let (addr, task) = start_in(&cfg_text, Some(dir.clone())).await;
    let first = fingerprint(addr).await;
    let cert = std::fs::read(dir.join("tls_cert.der")).expect("certificate saved");
    assert_eq!(tls::Fingerprint::of(&cert), first);
    let (mut admin, _) = TestClient::connect(addr, "admin", None).await;
    admin.send(C2S::Authenticate { token: "t".into() }).await;
    admin.send(C2S::CreateChannel { parent: None, settings: settings("Saved"), temporary: false }).await;
    admin.channels().await;
    task.abort();
    drop(admin);

    let (addr, task) = start_in(&cfg_text, Some(dir.clone())).await;
    assert_eq!(fingerprint(addr).await, first, "a restart must keep the certificate");
    let (_, welcome) = TestClient::connect(addr, "later", None).await;
    let S2C::Welcome { channels, .. } = welcome else { panic!("not welcomed") };
    assert!(channels.iter().any(|c| c.name == "Saved"), "channel lost on restart");
    assert_eq!(channels.len(), Channels::count(&cfg) + 1);
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

/// Channel count of a config, for comparisons.
struct Channels;
impl Channels {
    fn count(cfg: &Config) -> usize {
        fn walk(list: &[t7_server::config::ChannelConfig]) -> usize {
            list.iter().map(|c| 1 + walk(&c.children)).sum()
        }
        walk(&cfg.channels)
    }
}

/// Connects and logs in as admin.
async fn admin_login(addr: SocketAddr, from: Ipv4Addr) -> (TestClient, ClientId, Vec<ChannelInfo>) {
    let (mut admin, welcome) = TestClient::connect_from(addr, from, "admin", &key("admin")).await;
    let S2C::Welcome { your_id, channels, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    admin.send(C2S::Authenticate { token: "t".into() }).await;
    admin.until(|m| matches!(m, S2C::ClientAdmin { .. })).await;
    (admin, your_id, channels)
}

fn welcomed_id(welcome: &S2C) -> ClientId {
    match welcome {
        S2C::Welcome { your_id, .. } => *your_id,
        other => panic!("not welcomed: {other:?}"),
    }
}

fn rejected(reply: &S2C) -> &str {
    match reply {
        S2C::Rejected { reason } => reason,
        other => panic!("not rejected: {other:?}"),
    }
}

async fn error(c: &mut TestClient) -> String {
    match c.until(|m| matches!(m, S2C::Error { .. })).await {
        S2C::Error { message } => message,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn identities_are_proven_on_each_connection() {
    let addr = start(DEFAULT_CONFIG).await;
    let (_, welcome) = TestClient::connect(addr, "alice", None).await;
    let S2C::Welcome { your_id, clients, .. } = &welcome else { panic!("not welcomed: {welcome:?}") };
    assert_eq!(clients.iter().find(|c| c.id == *your_id).unwrap().identity, key("alice").identity());

    // A signature over other bytes than this connection's exporter.
    let c = TestClient::open(addr, HOME).await;
    let forged = key("mallory").prove(&[0; 32]);
    let (_, reply) = c.hello("mallory", None, PROTOCOL_VERSION, forged).await;
    assert!(rejected(&reply).contains("identity proof"), "{reply:?}");

    // A real proof, but from another connection.
    let first = TestClient::open(addr, HOME).await;
    let replayed = key("mallory").prove(&first.exporter);
    let (_, reply) = TestClient::open(addr, HOME).await.hello("mallory", None, PROTOCOL_VERSION, replayed).await;
    assert!(rejected(&reply).contains("identity proof"), "{reply:?}");

    // One person may connect twice.
    let (_, again) = TestClient::connect(addr, "alice", None).await;
    let S2C::Welcome { your_id, clients, .. } = &again else { panic!("not welcomed: {again:?}") };
    assert_eq!(clients.iter().find(|c| c.id == *your_id).unwrap().identity, key("alice").identity());
}

#[tokio::test]
async fn moderators_kick_from_their_channels_only() {
    let addr = start(&with_admin_token("t")).await;
    let (mut admin, admin_id, channels) = admin_login(addr, HOME).await;
    let (gaming, squad, lobby) = (id_of(&channels, "Gaming"), id_of(&channels, "Squad 1"), id_of(&channels, "Lobby"));
    let (mut moder, wm) = TestClient::connect(addr, "mod", None).await;
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let (mod_id, user_id) = (welcomed_id(&wm), welcomed_id(&wu));
    admin.send(C2S::SetModerator { channel: gaming, identity: key("mod").identity(), moderator: true }).await;
    moder.channels().await;

    // The user goes to a sub-channel of Gaming.
    user.send(C2S::JoinChannel { channel: squad, password: None }).await;
    moder.until(|m| *m == S2C::ClientMoved { id: user_id, channel: squad }).await;

    // Nobody else may kick, and a moderator may not kick off the server.
    user.send(C2S::Kick { target: mod_id, from_server: false, reason: String::new() }).await;
    assert!(error(&mut user).await.contains("can't kick"));
    moder.send(C2S::Kick { target: user_id, from_server: true, reason: "bye".into() }).await;
    assert!(error(&mut moder).await.contains("only admins"));
    moder.send(C2S::Kick { target: admin_id, from_server: false, reason: String::new() }).await;
    assert!(error(&mut moder).await.contains("can't kick"), "an admin");

    // A channel kick: everyone hears who and why, and the user lands in the default channel.
    moder.send(C2S::Kick { target: user_id, from_server: false, reason: "  off topic ".into() }).await;
    let kicked = S2C::ClientKicked { id: user_id, by: mod_id, from_server: false, reason: "off topic".into() };
    assert_eq!(admin.until(|m| matches!(m, S2C::ClientKicked { .. })).await, kicked);
    assert_eq!(user.until(|m| matches!(m, S2C::ClientKicked { .. })).await, kicked);
    assert_eq!(user.recv().await, S2C::ClientMoved { id: user_id, channel: lobby });

    // In the default channel there is nowhere to kick them to.
    moder.send(C2S::Kick { target: user_id, from_server: false, reason: String::new() }).await;
    assert!(error(&mut moder).await.contains("can't kick"));

    // An admin's server kick: the notice arrives before the connection closes.
    admin.send(C2S::Kick { target: user_id, from_server: true, reason: "x".repeat(MAX_REASON_LEN + 1) }).await;
    assert!(error(&mut admin).await.contains("at most"));
    admin.send(C2S::Kick { target: user_id, from_server: true, reason: "enough".into() }).await;
    let kicked = S2C::ClientKicked { id: user_id, by: admin_id, from_server: true, reason: "enough".into() };
    assert_eq!(user.until(|m| matches!(m, S2C::ClientKicked { .. })).await, kicked);
    user.closed().await;
    let left = moder.until(|m| matches!(m, S2C::ClientLeft { .. })).await;
    assert_eq!(left, S2C::ClientLeft { id: user_id, reason: LeaveReason::Kicked });
}

#[tokio::test]
async fn moderators_survive_a_restart() {
    let dir = data_dir("moderation");
    let cfg = with_admin_token("t");
    let (addr, task) = start_in(&cfg, Some(dir.clone())).await;
    let (mut admin, _, channels) = admin_login(addr, HOME).await;
    let gaming = id_of(&channels, "Gaming");
    // Only admins choose moderators.
    let (mut user, _) = TestClient::connect(addr, "user", None).await;
    user.send(C2S::SetModerator { channel: gaming, identity: key("user").identity(), moderator: true }).await;
    let refused = user.until(|m| matches!(m, S2C::Error { .. })).await;
    assert!(matches!(refused, S2C::Error { ref message } if message.contains("only admins")), "{refused:?}");
    admin.send(C2S::SetModerator { channel: gaming, identity: key("mod").identity(), moderator: true }).await;
    let list = admin.channels().await;
    assert_eq!(list.iter().find(|c| c.id == gaming).unwrap().moderators, [key("mod").identity()]);
    // The server saves after it broadcasts; a round trip waits for that.
    admin.send(C2S::Ping { nonce: 1 }).await;
    admin.until(|m| matches!(m, S2C::Pong { .. })).await;
    task.abort();
    drop(admin);

    let (addr, task) = start_in(&cfg, Some(dir.clone())).await;
    let (mut admin, _, channels) = admin_login(addr, HOME).await;
    assert_eq!(channels.iter().find(|c| c.id == gaming).unwrap().moderators, [key("mod").identity()]);

    // Removing the moderator by identity works while they are offline, and sticks.
    admin.send(C2S::SetModerator { channel: gaming, identity: key("mod").identity(), moderator: false }).await;
    assert!(admin.channels().await.iter().all(|c| c.moderators.is_empty()));
    admin.send(C2S::Ping { nonce: 2 }).await;
    admin.until(|m| matches!(m, S2C::Pong { .. })).await;
    let saved = std::fs::read_to_string(dir.join("channels.toml")).unwrap();
    assert!(!saved.contains("moderators"), "{saved}");
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}
