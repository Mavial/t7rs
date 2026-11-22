//! Spins up a real server on localhost and drives it with raw protocol clients.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use t7_proto::identity::IdentityKey;
use t7_proto::perms::{self, Perm, Perms, RoleChange, RoleId, Roles};
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
const ADMIN_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
const ELSEWHERE: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 3);

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
        let read = async { while read_frame::<_, S2C>(&mut self.r, &mut self.buf).await.is_ok() {} };
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
    async fn open(server: SocketAddr, welcome: &S2C) -> Self {
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

    // So is one sealed with another key, even with a's id on it. a's counters
    // stay below 10 in this test, so the forger skips past them: only the key
    // check can drop its datagram, not the replay window.
    let mut forger = voice::ClientSide::new(va.id, &[0x42; 32]);
    let mut forged = Vec::new();
    for _ in 0..10 {
        forger.seal(voice::ToServer::Ping { nonce: 0 }, &mut forged);
    }
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
async fn fingerprint(addr: SocketAddr) -> tls::Fingerprint {
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

fn settings(name: &str) -> ChannelSettings {
    ChannelSettings { name: name.into(), topic: String::new(), max_clients: None, password: PasswordChange::Keep }
}

fn id_of(list: &[ChannelInfo], name: &str) -> ChannelId {
    list.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no channel {name}")).id
}

fn data_dir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("t7-e2e-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn claim_token(dir: &Path) -> String {
    let text = std::fs::read_to_string(dir.join("roles.toml")).expect("roles.toml");
    let file: toml::Table = toml::from_str(&text).unwrap();
    file.get("claim_token").and_then(|t| t.as_str()).unwrap_or_else(|| panic!("no claim token: {text}")).to_owned()
}

async fn claim(addr: SocketAddr, dir: &Path, from: Ipv4Addr) -> (TestClient, ClientId, Vec<ChannelInfo>) {
    let (mut owner, welcome) = TestClient::connect_from(addr, from, "admin", &key("admin")).await;
    let S2C::Welcome { your_id, channels, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    owner.send(C2S::ClaimOwnership { token: claim_token(dir) }).await;
    owner.until(|m| *m == S2C::ClientRole { id: your_id, role: Some("Owner".into()) }).await;
    (owner, your_id, channels)
}

async fn start_owned(test: &str, from: Ipv4Addr) -> (SocketAddr, TestClient, ClientId, Vec<ChannelInfo>) {
    let dir = data_dir(test);
    let (addr, _) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (owner, id, channels) = claim(addr, &dir, from).await;
    (addr, owner, id, channels)
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

fn may_in(welcome: &S2C, channel: ChannelId) -> Perms {
    let S2C::Welcome { permissions, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    *permissions.channels.get(&channel).unwrap_or_else(|| panic!("no channel {channel}"))
}

#[tokio::test]
async fn the_first_to_claim_owns_the_server() {
    let dir = data_dir("claim");
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut alice, wa) = TestClient::connect(addr, "alice", None).await;
    let (mut bob, wb) = TestClient::connect(addr, "bob", None).await;
    let token = claim_token(&dir);
    let alice_id = welcomed_id(&wa);
    let S2C::Welcome { permissions, .. } = &wb else { unreachable!() };
    assert!(!permissions.server.has(Perm::ManageRoles));
    alice.until(|m| matches!(m, S2C::ClientJoined(c) if c.nickname == "bob")).await;

    bob.send(C2S::ClaimOwnership { token: "guess".into() }).await;
    assert!(error(&mut bob).await.contains("wrong"));

    alice.send(C2S::ClaimOwnership { token: token.clone() }).await;
    let S2C::Permissions(mine) = alice.until(|m| matches!(m, S2C::Permissions(_))).await else { unreachable!() };
    assert_eq!(mine.server, Perms::ALL);
    let S2C::Roles(roles) = alice.until(|m| matches!(m, S2C::Roles(_))).await else { unreachable!() };
    assert_eq!(roles.owner, Some(key("alice").identity()));
    assert_eq!(roles.members[&key("alice").identity()].nickname, "alice");
    let owner = S2C::ClientRole { id: alice_id, role: Some("Owner".into()) };
    assert_eq!(bob.until(|m| matches!(m, S2C::ClientRole { .. })).await, owner);

    bob.send(C2S::ClaimOwnership { token: token.clone() }).await;
    assert!(error(&mut bob).await.contains("already used"));
    let saved = std::fs::read_to_string(dir.join("roles.toml")).unwrap();
    assert!(!saved.contains("claim_token"), "{saved}");
    task.abort();
    drop((alice, bob));

    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (_, welcome) = TestClient::connect(addr, "alice", None).await;
    let S2C::Welcome { your_id, clients, permissions, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    assert_eq!(clients.iter().find(|c| c.id == your_id).unwrap().role.as_deref(), Some("Owner"));
    assert_eq!(permissions.server, Perms::ALL);
    let (mut bob, _) = TestClient::connect(addr, "bob", None).await;
    bob.send(C2S::ClaimOwnership { token }).await;
    assert!(error(&mut bob).await.contains("already used"));
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn clients_from_before_roles_are_told_to_update() {
    let addr = start(DEFAULT_CONFIG).await;
    let (_, reply) = TestClient::connect_as(addr, "old", None, 5).await;
    assert!(rejected(&reply).contains("please update your T7"), "{reply:?}");
}

#[tokio::test]
async fn installs_from_before_roles_keep_their_moderators() {
    let dir = data_dir("migrate");
    std::fs::write(dir.join("admin_token.txt"), "old-token").unwrap();
    let moderator = key("mod").identity().to_hex();
    let channels = format!(
        "[[channels]]\nname = \"Lobby\"\ndefault = true\n\n[[channels]]\nname = \"Gaming\"\nmoderators = \
         [\"{moderator}\"]\n\n  [[channels.children]]\n  name = \"Squad\"\n\n[[channels]]\nname = \"AFK\"\n"
    );
    std::fs::write(dir.join("channels.toml"), channels).unwrap();
    let cfg = format!("admin_token = \"old-token\"\n{DEFAULT_CONFIG}");
    let (addr, task) = start_in(&cfg, Some(dir.clone())).await;
    let (mut moder, wm) = TestClient::connect(addr, "mod", None).await;
    assert!(!dir.join("admin_token.txt").exists(), "the admin token is gone");
    assert!(!claim_token(&dir).is_empty(), "a claim token replaces it");

    let S2C::Welcome { channels, .. } = &wm else { unreachable!() };
    let [gaming, squad, afk] = ["Gaming", "Squad", "AFK"].map(|name| id_of(channels, name));
    assert_eq!(channels.iter().find(|c| c.id == gaming).unwrap().moderators, [key("mod").identity()]);
    for (channel, kicks) in [(gaming, true), (squad, true), (afk, false)] {
        assert_eq!(may_in(&wm, channel).has(Perm::Kick), kicks, "{channel}");
    }
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let user_id = welcomed_id(&wu);
    user.send(C2S::JoinChannel { channel: squad, password: None }).await;
    moder.until(|m| *m == S2C::ClientMoved { id: user_id, channel: squad }).await;
    moder.send(C2S::Kick { target: user_id, from_server: false, reason: String::new() }).await;
    assert!(
        matches!(user.until(|m| matches!(m, S2C::ClientKicked { .. })).await, S2C::ClientKicked { id, .. } if id == user_id)
    );

    moder.send(C2S::ClaimOwnership { token: "old-token".into() }).await;
    assert!(error(&mut moder).await.contains("wrong"));
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn channels_are_managed_with_permissions() {
    let dir = data_dir("manage");
    let (addr, _) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let user_id = welcomed_id(&wu);
    let S2C::Welcome { channels, .. } = &wu else { unreachable!() };
    let lobby = id_of(channels, "Lobby");

    // Anyone can make a temporary channel and is moved into it.
    user.send(C2S::CreateChannel {
        parent: None,
        settings: settings("Raid"),
        temporary: true,
        kind: ChannelKind::Voice,
    })
    .await;
    let list = user.channels().await;
    let raid = id_of(&list, "Raid");
    assert!(list.iter().find(|c| c.id == raid).unwrap().temporary);
    assert_eq!(
        user.until(|m| matches!(m, S2C::ClientMoved { .. })).await,
        S2C::ClientMoved { id: user_id, channel: raid }
    );

    // ...but not a permanent one, and not edit channels they didn't make.
    user.send(C2S::CreateChannel {
        parent: None,
        settings: settings("Mine"),
        temporary: false,
        kind: ChannelKind::Voice,
    })
    .await;
    assert!(error(&mut user).await.contains("can't create permanent channels"));
    user.send(C2S::DeleteChannel { id: lobby }).await;
    assert!(error(&mut user).await.contains("can't delete"));

    // The creator may edit their temporary channel, adding a password.
    let locked = ChannelSettings { password: PasswordChange::Set("pw".into()), ..settings("Raid Night") };
    user.send(C2S::EditChannel { id: raid, settings: locked }).await;
    let list = user.channels().await;
    assert!(list.iter().any(|c| c.name == "Raid Night" && c.has_password));

    let (mut admin, admin_id, _) = claim(addr, &dir, HOME).await;
    admin.send(C2S::JoinChannel { channel: raid, password: None }).await;
    assert_eq!(
        admin.until(|m| matches!(m, S2C::ClientMoved { .. })).await,
        S2C::ClientMoved { id: admin_id, channel: raid }
    );

    admin
        .send(C2S::CreateChannel {
            parent: None,
            settings: settings("Music"),
            temporary: false,
            kind: ChannelKind::Voice,
        })
        .await;
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
    assert_eq!(
        user.until(|m| matches!(m, S2C::ClientMoved { .. })).await,
        S2C::ClientMoved { id: user_id, channel: music }
    );
    assert!(user.channels().await.iter().all(|c| c.id != lobby));

    admin.send(C2S::JoinChannel { channel: music, password: None }).await;
    let list = admin.channels().await;
    assert!(list.iter().all(|c| c.id != raid), "empty temporary channel should be gone");
}

#[tokio::test]
async fn channel_edits_and_the_certificate_survive_a_restart() {
    let dir = data_dir("restart");
    let cfg: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let first = fingerprint(addr).await;
    let cert = std::fs::read(dir.join("tls_cert.der")).expect("certificate saved");
    assert_eq!(tls::Fingerprint::of(&cert), first);
    let (mut admin, _, _) = claim(addr, &dir, HOME).await;
    admin
        .send(C2S::CreateChannel {
            parent: None,
            settings: settings("Saved"),
            temporary: false,
            kind: ChannelKind::Voice,
        })
        .await;
    admin.channels().await;
    task.abort();
    drop(admin);

    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
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
    let (addr, mut admin, admin_id, channels) = start_owned("mod-kicks", HOME).await;
    let (gaming, squad, lobby) = (id_of(&channels, "Gaming"), id_of(&channels, "Squad 1"), id_of(&channels, "Lobby"));
    let (mut moder, wm) = TestClient::connect(addr, "mod", None).await;
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let (mod_id, user_id) = (welcomed_id(&wm), welcomed_id(&wu));
    admin.send(C2S::SetModerator { channel: gaming, identity: key("mod").identity(), moderator: true }).await;
    moder.channels().await;

    // The user goes to a sub-channel of Gaming.
    user.send(C2S::JoinChannel { channel: squad, password: None }).await;
    moder.until(|m| *m == S2C::ClientMoved { id: user_id, channel: squad }).await;

    user.send(C2S::Kick { target: mod_id, from_server: false, reason: String::new() }).await;
    assert!(error(&mut user).await.contains("can't kick"));
    moder.send(C2S::Kick { target: user_id, from_server: true, reason: "bye".into() }).await;
    assert!(error(&mut moder).await.contains("can't kick"));
    moder.send(C2S::Kick { target: admin_id, from_server: false, reason: String::new() }).await;
    assert!(error(&mut moder).await.contains("can't kick"), "the owner");

    // A channel kick: everyone hears who and why, and the user lands in the default channel.
    moder.send(C2S::Kick { target: user_id, from_server: false, reason: "  off topic ".into() }).await;
    let kicked = S2C::ClientKicked { id: user_id, by: mod_id, from_server: false, reason: "off topic".into() };
    assert_eq!(admin.until(|m| matches!(m, S2C::ClientKicked { .. })).await, kicked);
    assert_eq!(user.until(|m| matches!(m, S2C::ClientKicked { .. })).await, kicked);
    assert_eq!(user.recv().await, S2C::ClientMoved { id: user_id, channel: lobby });

    // In the default channel there is nowhere to kick them to.
    admin.send(C2S::Kick { target: user_id, from_server: false, reason: String::new() }).await;
    assert!(error(&mut admin).await.contains("nowhere to kick"));

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
async fn kicked_people_land_where_a_joining_client_would() {
    let (addr, mut admin, _, channels) = start_owned("kick-landing", HOME).await;
    let [lobby, hangout, gaming, squad] = ["Lobby", "Hangout", "Gaming", "Squad 1"].map(|name| id_of(&channels, name));
    let (mut moder, _) = TestClient::connect(addr, "mod", None).await;
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let user_id = welcomed_id(&wu);
    for channel in [gaming, hangout] {
        admin.send(C2S::SetModerator { channel, identity: key("mod").identity(), moderator: true }).await;
        moder.channels().await;
    }
    user.send(C2S::JoinChannel { channel: squad, password: None }).await;
    moder.until(|m| *m == S2C::ClientMoved { id: user_id, channel: squad }).await;

    // The default channel needs a password: the kick lands them in the first open channel.
    let locked = ChannelSettings { password: PasswordChange::Set("pw".into()), ..settings("Lobby") };
    admin.send(C2S::EditChannel { id: lobby, settings: locked }).await;
    moder.channels().await;
    moder.send(C2S::Kick { target: user_id, from_server: false, reason: String::new() }).await;
    moder.until(|m| matches!(m, S2C::ClientKicked { .. })).await;
    assert_eq!(moder.recv().await, S2C::ClientMoved { id: user_id, channel: hangout });

    // The default channel is full (admin and mod are in it), and the kick is
    // out of Hangout, so the next open channel it is.
    let full = ChannelSettings { max_clients: Some(2), password: PasswordChange::Remove, ..settings("Lobby") };
    admin.send(C2S::EditChannel { id: lobby, settings: full }).await;
    moder.channels().await;
    moder.send(C2S::Kick { target: user_id, from_server: false, reason: String::new() }).await;
    moder.until(|m| matches!(m, S2C::ClientKicked { .. })).await;
    assert_eq!(moder.recv().await, S2C::ClientMoved { id: user_id, channel: gaming });
}

#[tokio::test]
async fn bans_keep_identities_and_addresses_out() {
    let (addr, mut admin, admin_id, _) = start_owned("bans", ADMIN_IP).await;
    let (mut mallory, wm) = TestClient::connect(addr, "mallory", None).await;
    let mallory_id = welcomed_id(&wm);

    // An identity ban kicks them off with the reason...
    admin.send(C2S::Ban { target: mallory_id, duration_secs: None, reason: "spam".into(), include_ip: false }).await;
    let kicked =
        S2C::ClientKicked { id: mallory_id, by: admin_id, from_server: true, reason: "banned for good (spam)".into() };
    assert_eq!(mallory.until(|m| matches!(m, S2C::ClientKicked { .. })).await, kicked);
    mallory.closed().await;
    // ...and keeps that key out from any address.
    for from in [HOME, ELSEWHERE] {
        let (_, reply) = TestClient::connect_from(addr, from, "mallory", &key("mallory")).await;
        assert_eq!(rejected(&reply), "You are banned from this server (spam).");
    }
    // Their address wasn't banned, so another key gets in from it.
    let (_, reply) = TestClient::connect_from(addr, HOME, "newcomer", &key("newcomer")).await;
    welcomed_id(&reply);

    // An IP ban also keeps out new keys from that address.
    let eve_ip = Ipv4Addr::new(127, 0, 0, 4);
    let (mut eve, we) = TestClient::connect_from(addr, eve_ip, "eve", &key("eve")).await;
    admin
        .send(C2S::Ban { target: welcomed_id(&we), duration_secs: Some(3600), reason: String::new(), include_ip: true })
        .await;
    eve.until(|m| matches!(m, S2C::ClientKicked { .. })).await;
    let (_, reply) = TestClient::connect_from(addr, eve_ip, "eve2", &key("eve2")).await;
    let ends = ["59 minutes", "1 hour"].map(|left| format!("You are banned from this server. The ban ends in {left}."));
    assert!(ends.iter().any(|e| e == rejected(&reply)), "{reply:?}");

    // Held open: a dropped connection could leave before the ban arrives.
    let (_flatmate, wb) = TestClient::connect_from(addr, ADMIN_IP, "flatmate", &key("flatmate")).await;
    admin
        .send(C2S::Ban { target: welcomed_id(&wb), duration_secs: None, reason: String::new(), include_ip: true })
        .await;
    assert!(error(&mut admin).await.contains("your IP address"));

    // The list has both bans; lifting eve's lets her address back in.
    admin.send(C2S::ListBans).await;
    let S2C::Bans(list) = admin.until(|m| matches!(m, S2C::Bans(_))).await else { unreachable!() };
    assert_eq!(list.len(), 2, "{list:?}");
    let eve_ban = list.iter().find(|b| b.identity == key("eve").identity()).expect("eve's ban");
    assert_eq!(eve_ban.ip, Some(eve_ip.into()));
    assert_eq!((eve_ban.nickname.as_str(), eve_ban.banned_by.as_str()), ("eve", "admin"));
    assert_eq!(eve_ban.expires_unix, Some(eve_ban.created_unix + 3600));
    let mallory_ban = list.iter().find(|b| b.identity == key("mallory").identity()).expect("mallory's ban");
    assert_eq!((mallory_ban.ip, mallory_ban.expires_unix, mallory_ban.reason.as_str()), (None, None, "spam"));

    admin.send(C2S::Unban { id: eve_ban.id }).await;
    let S2C::Bans(list) = admin.until(|m| matches!(m, S2C::Bans(_))).await else { unreachable!() };
    assert_eq!(list.iter().map(|b| b.id).collect::<Vec<_>>(), [mallory_ban.id]);
    let (_, reply) = TestClient::connect_from(addr, eve_ip, "eve", &key("eve")).await;
    welcomed_id(&reply);

    let (mut user, _) = TestClient::connect(addr, "user", None).await;
    user.send(C2S::ListBans).await;
    assert!(error(&mut user).await.contains("can't see the ban list"));
    user.send(C2S::Unban { id: mallory_ban.id }).await;
    assert!(error(&mut user).await.contains("can't lift bans"));
}

#[tokio::test]
async fn ip_bans_eject_everyone_at_that_address() {
    let (addr, mut admin, admin_id, _) = start_owned("ip-bans", ADMIN_IP).await;
    let (mut eve, we) = TestClient::connect_from(addr, ELSEWHERE, "eve", &key("eve")).await;
    let (mut sock, ws) = TestClient::connect_from(addr, ELSEWHERE, "sock", &key("sock")).await;
    let (mut bystander, _) = TestClient::connect_from(addr, HOME, "bystander", &key("bystander")).await;
    let (eve_id, sock_id) = (welcomed_id(&we), welcomed_id(&ws));

    admin.send(C2S::Ban { target: eve_id, duration_secs: None, reason: "spam".into(), include_ip: true }).await;
    for (client, id) in [(&mut eve, eve_id), (&mut sock, sock_id)] {
        let kicked = client.until(|m| matches!(m, S2C::ClientKicked { id: k, .. } if *k == id)).await;
        let notice = S2C::ClientKicked { id, by: admin_id, from_server: true, reason: "banned for good (spam)".into() };
        assert_eq!(kicked, notice);
        client.closed().await;
    }
    // Someone at another address stays.
    bystander.send(C2S::Ping { nonce: 7 }).await;
    assert_eq!(bystander.until(|m| matches!(m, S2C::Pong { .. })).await, S2C::Pong { nonce: 7 });
}

#[tokio::test]
async fn moderators_dont_rearrange_channels() {
    let (addr, mut admin, _, channels) = start_owned("rearrange", HOME).await;
    let afk = id_of(&channels, "AFK");
    let (mut moder, _) = TestClient::connect(addr, "mod", None).await;
    admin.send(C2S::SetModerator { channel: afk, identity: key("mod").identity(), moderator: true }).await;
    let mut before = moder.channels().await;

    moder.send(C2S::MoveChannel { id: afk, parent: None, position: 0 }).await;
    assert!(error(&mut moder).await.contains("can't move this channel"));
    moder.send(C2S::SetDefaultChannel { id: afk }).await;
    assert!(error(&mut moder).await.contains("can't change the default channel"));

    let (_, welcome) = TestClient::connect(addr, "later", None).await;
    let S2C::Welcome { channels: mut after, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    before.sort_by_key(|c| c.id);
    after.sort_by_key(|c| c.id);
    assert_eq!(after, before, "the channel tree changed");
}

#[tokio::test]
async fn temporary_bans_end() {
    let (addr, mut admin, _, _) = start_owned("temp-bans", ADMIN_IP).await;
    let (mut mallory, wm) = TestClient::connect(addr, "mallory", None).await;
    // Bans count whole seconds, so this one lasts between one and two.
    admin
        .send(C2S::Ban {
            target: welcomed_id(&wm),
            duration_secs: Some(2),
            reason: "cool off".into(),
            include_ip: true,
        })
        .await;
    let kicked = mallory.until(|m| matches!(m, S2C::ClientKicked { .. })).await;
    assert!(
        matches!(kicked, S2C::ClientKicked { ref reason, .. } if reason == "banned for less than a minute (cool off)"),
        "{kicked:?}"
    );

    let (_, reply) = TestClient::connect(addr, "mallory", None).await;
    assert!(rejected(&reply).contains("ends in less than a minute"), "{reply:?}");
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let (_, reply) = TestClient::connect(addr, "mallory", None).await;
    welcomed_id(&reply);
    admin.send(C2S::ListBans).await;
    assert_eq!(admin.until(|m| matches!(m, S2C::Bans(_))).await, S2C::Bans(Vec::new()), "expired bans leave the list");
}

#[tokio::test]
async fn moderators_and_bans_survive_a_restart() {
    let dir = data_dir("moderation");
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut admin, _, channels) = claim(addr, &dir, ADMIN_IP).await;
    let gaming = id_of(&channels, "Gaming");
    let (mut user, _) = TestClient::connect(addr, "user", None).await;
    user.send(C2S::SetModerator { channel: gaming, identity: key("user").identity(), moderator: true }).await;
    assert!(error(&mut user).await.contains("can't choose who moderates"));
    admin.send(C2S::SetModerator { channel: gaming, identity: key("mod").identity(), moderator: true }).await;
    let list = admin.channels().await;
    assert_eq!(list.iter().find(|c| c.id == gaming).unwrap().moderators, [key("mod").identity()]);
    // Held open, so the ClientLeft below is the ban's kick and not a disconnect.
    let (_mallory, wm) = TestClient::connect(addr, "mallory", None).await;
    admin
        .send(C2S::Ban { target: welcomed_id(&wm), duration_secs: None, reason: "spam".into(), include_ip: true })
        .await;
    // The server saves before it kicks.
    admin.until(|m| matches!(m, S2C::ClientLeft { .. })).await;
    task.abort();
    drop(admin);

    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (_, reply) = TestClient::connect(addr, "mallory", None).await;
    assert!(rejected(&reply).contains("spam"), "the ban was lost: {reply:?}");
    let (mut admin, welcome) = TestClient::connect_from(addr, ADMIN_IP, "admin", &key("admin")).await;
    let S2C::Welcome { channels, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    assert_eq!(channels.iter().find(|c| c.id == gaming).unwrap().moderators, [key("mod").identity()]);
    admin.send(C2S::ListBans).await;
    let S2C::Bans(list) = admin.until(|m| matches!(m, S2C::Bans(_))).await else { unreachable!() };
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].identity, list[0].ip), (key("mallory").identity(), Some(HOME.into())));

    // Removing the moderator by identity works while they are offline, and sticks.
    admin.send(C2S::SetModerator { channel: gaming, identity: key("mod").identity(), moderator: false }).await;
    assert!(admin.channels().await.iter().all(|c| c.moderators.is_empty()));
    // The server saves after it broadcasts; a round trip waits for that.
    admin.sync().await;
    let saved = std::fs::read_to_string(dir.join("channels.toml")).unwrap();
    assert!(!saved.contains("moderators"), "{saved}");
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

impl TestClient {
    async fn sync(&mut self) -> Vec<S2C> {
        self.send(C2S::Ping { nonce: 0 }).await;
        let mut before = Vec::new();
        loop {
            match self.recv().await {
                S2C::Pong { nonce: 0 } => return before,
                m => before.push(m),
            }
        }
    }

    async fn change_roles(&mut self, change: RoleChange) -> Roles {
        self.sync().await;
        self.send(C2S::ChangeRoles(change)).await;
        match self.until(|m| matches!(m, S2C::Roles(_) | S2C::Error { .. })).await {
            S2C::Roles(roles) => roles,
            other => panic!("refused: {other:?}"),
        }
    }
}

fn assign(nick: &str, role: RoleId, held: bool) -> RoleChange {
    RoleChange::Assign { identity: key(nick).identity(), role, held }
}

#[tokio::test]
async fn people_act_only_on_those_below_their_role() {
    let (addr, mut owner, owner_id, channels) = start_owned("hierarchy", HOME).await;
    let hangout = id_of(&channels, "Hangout");
    let (mut admin, wa) = TestClient::connect(addr, "boss", None).await;
    let (mut moder, wm) = TestClient::connect(addr, "mod", None).await;
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let (admin_id, mod_id, user_id) = (welcomed_id(&wa), welcomed_id(&wm), welcomed_id(&wu));
    owner.change_roles(assign("boss", RoleId::ADMIN, true)).await;
    let roles = owner.change_roles(assign("mod", RoleId::MODERATOR, true)).await;
    assert_eq!(roles.members[&key("mod").identity()].roles, [RoleId::MODERATOR]);
    let retitled = S2C::ClientRole { id: mod_id, role: Some("Moderator".into()) };
    assert_eq!(user.until(|m| matches!(m, S2C::ClientRole { id, .. } if *id == mod_id)).await, retitled);
    let S2C::Permissions(mine) = moder.until(|m| matches!(m, S2C::Permissions(_))).await else { unreachable!() };
    assert!(mine.server.has(Perm::Kick) && !mine.server.has(Perm::Ban));
    for c in [&mut admin, &mut user] {
        c.send(C2S::JoinChannel { channel: hangout, password: None }).await;
    }
    moder.until(|m| *m == S2C::ClientMoved { id: admin_id, channel: hangout }).await;
    moder.until(|m| *m == S2C::ClientMoved { id: user_id, channel: hangout }).await;

    let kick = |target| C2S::Kick { target, from_server: false, reason: String::new() };
    moder.send(kick(admin_id)).await;
    assert!(error(&mut moder).await.contains("their role isn't below yours"));
    moder.send(C2S::Kick { target: owner_id, from_server: true, reason: String::new() }).await;
    assert!(error(&mut moder).await.contains("their role isn't below yours"));
    moder.send(C2S::Ban { target: user_id, duration_secs: None, reason: String::new(), include_ip: false }).await;
    assert!(error(&mut moder).await.contains("can't ban"));
    moder.send(kick(user_id)).await;
    user.until(|m| matches!(m, S2C::ClientKicked { id, .. } if *id == user_id)).await;
    moder.send(C2S::JoinChannel { channel: hangout, password: None }).await;
    admin.until(|m| *m == S2C::ClientMoved { id: mod_id, channel: hangout }).await;
    admin.send(kick(mod_id)).await;
    moder.until(|m| matches!(m, S2C::ClientKicked { id, .. } if *id == mod_id)).await;

    let refused = [
        assign("user", RoleId::ADMIN, true),
        assign("boss", RoleId::ADMIN, false),
        RoleChange::Rename { id: RoleId::ADMIN, name: "Boss".into() },
        RoleChange::Move { id: RoleId::MODERATOR, position: 0 },
    ];
    for change in refused {
        admin.send(C2S::ChangeRoles(change.clone())).await;
        assert!(error(&mut admin).await.contains("below your own"), "{change:?}");
    }
    let roles = admin.change_roles(RoleChange::Create { name: "DJ".into() }).await;
    let dj = roles.ranked.last().unwrap().id;
    let roles = admin.change_roles(assign("user", dj, true)).await;
    assert_eq!(roles.members[&key("user").identity()].roles, [dj]);
    moder.send(C2S::ChangeRoles(assign("user", dj, false))).await;
    assert!(error(&mut moder).await.contains("can't manage roles"));
    let (mut nobody, _) = TestClient::connect(addr, "nobody", None).await;
    nobody.send(C2S::SetModerator { channel: hangout, identity: key("boss").identity(), moderator: true }).await;
    assert!(error(&mut nobody).await.contains("can't choose who moderates"));
}

#[tokio::test]
async fn roles_and_who_holds_them_survive_a_restart() {
    let dir = data_dir("roles");
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut owner, _, _) = claim(addr, &dir, HOME).await;
    let (bob, _) = TestClient::connect(addr, "bob", None).await;
    let roles = owner.change_roles(RoleChange::Create { name: "DJ".into() }).await;
    let dj = roles.ranked.last().unwrap().id;
    let perms = Perms::of(&[Perm::View, Perm::Join, Perm::MuteMembers]);
    owner.change_roles(RoleChange::SetPermissions { id: dj, perms }).await;
    owner.change_roles(RoleChange::Move { id: dj, position: 0 }).await;
    owner.change_roles(assign("bob", RoleId::MODERATOR, true)).await;
    let roles = owner.change_roles(assign("bob", dj, true)).await;
    assert_eq!(roles.members[&key("bob").identity()].roles, [RoleId::MODERATOR, dj]);
    task.abort();
    drop((owner, bob));

    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut owner, _) = TestClient::connect(addr, "admin", None).await;
    let S2C::Roles(roles) = owner.until(|m| matches!(m, S2C::Roles(_))).await else { unreachable!() };
    assert_eq!(roles.ranked[0], perms::Role { id: dj, name: "DJ".into(), perms });
    let (bob, welcome) = TestClient::connect_from(addr, HOME, "Bobby", &key("bob")).await;
    let S2C::Welcome { your_id, clients, permissions, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    assert_eq!(clients.iter().find(|c| c.id == your_id).unwrap().role.as_deref(), Some("DJ"));
    assert!(permissions.server.has(Perm::MuteMembers) && permissions.server.has(Perm::Kick));
    let S2C::Roles(roles) = owner.until(|m| matches!(m, S2C::Roles(_))).await else { unreachable!() };
    assert_eq!(roles.members[&key("bob").identity()].nickname, "Bobby");

    drop(bob);
    owner.until(|m| matches!(m, S2C::ClientLeft { .. })).await;
    owner.change_roles(assign("bob", dj, false)).await;
    owner.send(C2S::ChangeRoles(assign("bob", dj, true))).await;
    assert!(error(&mut owner).await.contains("online"), "granting needs them online");
    task.abort();
    drop(owner);

    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut owner, _) = TestClient::connect(addr, "admin", None).await;
    let (_, welcome) = TestClient::connect(addr, "bob", None).await;
    let S2C::Welcome { your_id, clients, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    assert_eq!(clients.iter().find(|c| c.id == your_id).unwrap().role.as_deref(), Some("Moderator"));
    owner.change_roles(assign("bob", RoleId::MODERATOR, false)).await;
    let roles = owner.change_roles(RoleChange::Delete { id: dj }).await;
    assert!(roles.role(dj).is_none() && !roles.members.contains_key(&key("bob").identity()));
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_owner_reset_hands_the_server_to_the_next_claimer() {
    let dir = data_dir("owner-reset");
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (owner, _, _) = claim(addr, &dir, HOME).await;
    task.abort();
    drop(owner);

    let token = t7_server::reset_owner_claim(&dir).unwrap();
    assert_eq!(claim_token(&dir), token);
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut old, welcome) = TestClient::connect(addr, "admin", None).await;
    let S2C::Welcome { permissions, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    assert_eq!(permissions.server, Perms::ALL, "the owner stays until the token is redeemed");
    let (mut new, welcome) = TestClient::connect(addr, "newcomer", None).await;
    new.send(C2S::ClaimOwnership { token }).await;
    let S2C::Permissions(was) = old.until(|m| matches!(m, S2C::Permissions(_))).await else { unreachable!() };
    assert!(!was.server.has(Perm::ManageRoles), "the old owner is no longer one");
    let new_id = welcomed_id(&welcome);
    let owner = S2C::ClientRole { id: new_id, role: Some("Owner".into()) };
    assert_eq!(old.until(|m| matches!(m, S2C::ClientRole { id, .. } if *id == new_id)).await, owner);
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

const PRIVATE: &str = r#"
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

fn names(list: &[ChannelInfo]) -> Vec<&str> {
    let mut names: Vec<&str> = list.iter().map(|c| c.name.as_str()).collect();
    names.sort_unstable();
    names
}

#[tokio::test]
async fn a_channel_hidden_from_everyone_hides_its_people_but_not_from_moderators() {
    let dir = data_dir("hidden");
    let (addr, task) = start_in(PRIVATE, Some(dir.clone())).await;
    let (mut owner, _, _) = claim(addr, &dir, HOME).await;
    let S2C::Welcome { channels, .. } = TestClient::connect(addr, "admin", None).await.1 else { unreachable!() };
    let [lobby, staff, office] = ["Lobby", "Staff", "Office"].map(|name| id_of(&channels, name));
    let (mut moder, wm) = TestClient::connect(addr, "mod", None).await;
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let mod_id = welcomed_id(&wm);
    let S2C::Welcome { channels, permissions, .. } = &wu else { unreachable!() };
    assert_eq!(names(channels), ["AFK", "Lobby"]);
    assert!(!permissions.sees(staff) && !permissions.sees(office));
    assert!(channels.iter().all(|c| c.overrides.is_empty()), "only editors get overrides");

    owner.change_roles(assign("mod", RoleId::MODERATOR, true)).await;
    assert_eq!(names(&moder.channels().await), ["AFK", "Lobby", "Office", "Staff"]);

    moder.send(C2S::JoinChannel { channel: office, password: None }).await;
    assert_eq!(user.until(|m| matches!(m, S2C::ClientHidden { .. })).await, S2C::ClientHidden { id: mod_id });
    for c in [&mut owner, &mut moder] {
        c.until(|m| *m == S2C::ClientMoved { id: mod_id, channel: office }).await;
    }
    let (mut late, wl) = TestClient::connect(addr, "late", None).await;
    let S2C::Welcome { clients, .. } = &wl else { unreachable!() };
    assert!(clients.iter().all(|c| c.id != mod_id), "{clients:?}");
    moder.send(C2S::SendText { target: TextTarget::Server, text: "psst".into() }).await;
    owner.until(|m| matches!(m, S2C::Text { from, .. } if *from == mod_id)).await;
    let heard = late.sync().await;
    assert!(heard.iter().all(|m| !matches!(m, S2C::Text { from, .. } if *from == mod_id)), "{heard:?}");

    user.send(C2S::JoinChannel { channel: office, password: None }).await;
    assert_eq!(error(&mut user).await, "channel does not exist");
    user.send(C2S::SendText { target: TextTarget::Client(mod_id), text: "hi".into() }).await;
    assert!(error(&mut user).await.contains("no longer online"));

    let (mut stays, ws) = TestClient::connect(addr, "mod2", None).await;
    let stays_id = welcomed_id(&ws);
    owner.change_roles(assign("mod2", RoleId::MODERATOR, true)).await;
    stays.channels().await;
    stays.send(C2S::JoinChannel { channel: office, password: None }).await;
    moder.until(|m| *m == S2C::ClientMoved { id: stays_id, channel: office }).await;
    owner.change_roles(assign("mod", RoleId::MODERATOR, false)).await;
    let moved = moder.until(|m| matches!(m, S2C::ClientMoved { .. } | S2C::ClientHidden { .. })).await;
    assert_eq!(moved, S2C::ClientMoved { id: mod_id, channel: lobby });
    assert_eq!(moder.recv().await, S2C::ClientHidden { id: stays_id });
    let S2C::ClientShown(back) = user.until(|m| matches!(m, S2C::ClientShown(_))).await else { unreachable!() };
    assert_eq!((back.id, back.channel), (mod_id, lobby));
    let S2C::Welcome { channels, .. } = TestClient::connect(addr, "mod", None).await.1 else { unreachable!() };
    assert_eq!(names(&channels), ["AFK", "Lobby"]);
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

const LAYERED: &str = r#"
[[channels]]
name = "Lobby"
default = true

[[channels]]
name = "Games"

  [[channels.overrides]]
  role = 0
  deny = ["speak", "send_messages"]

  [[channels.overrides]]
  role = 2
  allow = ["speak"]

  [[channels.children]]
  name = "Squad"

    [[channels.children.overrides]]
    role = 0
    allow = ["send_messages"]

    [[channels.children.children]]
    name = "Duo"
"#;

fn talks(permissions: &perms::Permissions, channel: ChannelId) -> (bool, bool) {
    (permissions.may_in(channel, Perm::Speak), permissions.may_in(channel, Perm::SendMessages))
}

#[tokio::test]
async fn the_closest_setting_wins_inherit_takes_the_parents_and_allow_beats_deny() {
    let dir = data_dir("layered");
    let (addr, task) = start_in(LAYERED, Some(dir.clone())).await;
    let (mut owner, _, channels) = claim(addr, &dir, HOME).await;
    let [lobby, games, squad, duo] = ["Lobby", "Games", "Squad", "Duo"].map(|name| id_of(&channels, name));
    let (_, wu) = TestClient::connect(addr, "user", None).await;
    let S2C::Welcome { permissions, .. } = &wu else { unreachable!() };
    assert_eq!(talks(permissions, lobby), (true, true));
    assert_eq!(talks(permissions, games), (false, false));
    assert_eq!(talks(permissions, squad), (false, true));
    assert_eq!(talks(permissions, duo), (false, true), "Duo inherits Squad");

    let (mut moder, _) = TestClient::connect(addr, "mod", None).await;
    owner.change_roles(assign("mod", RoleId::MODERATOR, true)).await;
    let S2C::Permissions(p) = moder.until(|m| matches!(m, S2C::Permissions(_))).await else { unreachable!() };
    assert_eq!(talks(&p, games), (true, false));
    assert_eq!(talks(&p, squad), (true, true));
    assert_eq!(talks(&p, duo), (true, true));
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

fn new_channel(name: &str, parent: Option<ChannelId>, kind: ChannelKind) -> C2S {
    C2S::CreateChannel { parent, settings: settings(name), temporary: false, kind }
}

#[tokio::test]
async fn a_category_is_never_joined_or_nested() {
    let (addr, mut admin, admin_id, channels) = start_owned("categories", HOME).await;
    let lobby = id_of(&channels, "Lobby");
    admin.send(new_channel("Games", None, ChannelKind::Category)).await;
    let list = admin.channels().await;
    let games = id_of(&list, "Games");
    assert_eq!(list.iter().find(|c| c.id == games).unwrap().kind, ChannelKind::Category);
    let after = admin.sync().await;
    assert!(after.iter().all(|m| !matches!(m, S2C::ClientMoved { .. })), "its creator stays put: {after:?}");

    admin.send(C2S::JoinChannel { channel: games, password: None }).await;
    assert!(error(&mut admin).await.contains("category can't be joined"));
    admin.send(C2S::SetDefaultChannel { id: games }).await;
    assert!(error(&mut admin).await.contains("category can't be the default"));
    admin.send(new_channel("Inner", Some(games), ChannelKind::Category)).await;
    assert!(error(&mut admin).await.contains("only be at the top level"));
    admin.send(C2S::MoveChannel { id: games, parent: Some(lobby), position: 0 }).await;
    assert!(error(&mut admin).await.contains("only be at the top level"));
    let temporary =
        C2S::CreateChannel { parent: None, settings: settings("Pop-up"), temporary: true, kind: ChannelKind::Category };
    admin.send(temporary).await;
    assert!(error(&mut admin).await.contains("can't be temporary"));

    admin.send(new_channel("Squad", Some(games), ChannelKind::Voice)).await;
    let list = admin.channels().await;
    let squad = id_of(&list, "Squad");
    assert_eq!(list.iter().find(|c| c.id == squad).unwrap().parent, Some(games));
    let moved = admin.until(|m| matches!(m, S2C::ClientMoved { .. })).await;
    assert_eq!(moved, S2C::ClientMoved { id: admin_id, channel: squad });
    let (mut user, _) = TestClient::connect(addr, "user", None).await;
    user.send(new_channel("Mine", None, ChannelKind::Category)).await;
    assert!(error(&mut user).await.contains("can't create permanent channels"));
}

fn set(channel: ChannelId, role: RoleId, allow: &[Perm], deny: &[Perm]) -> C2S {
    C2S::SetOverride { channel, role, allow: Perms::of(allow), deny: Perms::of(deny) }
}

fn overrides_in(list: &[ChannelInfo], channel: ChannelId) -> Vec<perms::Override> {
    list.iter().find(|c| c.id == channel).unwrap_or_else(|| panic!("no channel {channel}")).overrides.clone()
}

impl TestClient {
    async fn overrides(&mut self, channel: ChannelId) -> Vec<perms::Override> {
        let list = self
            .until(|m| matches!(m, S2C::Channels(l) if l.iter().any(|c| c.id == channel && !c.overrides.is_empty())))
            .await;
        let S2C::Channels(list) = list else { unreachable!() };
        overrides_in(&list, channel)
    }
}

#[tokio::test]
async fn overrides_take_effect_at_once_and_survive_a_restart() {
    let dir = data_dir("override-restart");
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut owner, _, channels) = claim(addr, &dir, HOME).await;
    let [lobby, hangout] = ["Lobby", "Hangout"].map(|name| id_of(&channels, name));
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let user_id = welcomed_id(&wu);
    user.send(C2S::JoinChannel { channel: hangout, password: None }).await;
    for c in [&mut owner, &mut user] {
        c.until(|m| *m == S2C::ClientMoved { id: user_id, channel: hangout }).await;
    }

    owner.send(set(hangout, RoleId::EVERYONE, &[], &[Perm::View])).await;
    let hidden = perms::Override::denying(RoleId::EVERYONE, Perms::of(&[Perm::View]));
    assert_eq!(owner.overrides(hangout).await, [hidden]);
    assert!(user.channels().await.iter().all(|c| c.id != hangout));
    assert_eq!(user.recv().await, S2C::ClientMoved { id: user_id, channel: lobby });
    owner.send(set(lobby, RoleId::EVERYONE, &[], &[Perm::View])).await;
    assert!(error(&mut owner).await.contains("see the default channel"));
    task.abort();
    drop((owner, user));

    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (_, welcome) = TestClient::connect(addr, "user", None).await;
    let S2C::Welcome { channels, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    assert!(channels.iter().all(|c| c.name != "Hangout"), "{channels:?}");
    let (_, welcome) = TestClient::connect(addr, "admin", None).await;
    let S2C::Welcome { channels, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    assert_eq!(overrides_in(&channels, id_of(&channels, "Hangout")), [hidden]);
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn people_set_permissions_only_where_they_may_and_for_roles_below_theirs() {
    let (addr, mut owner, _, channels) = start_owned("override-hierarchy", HOME).await;
    let [lobby, hangout] = ["Lobby", "Hangout"].map(|name| id_of(&channels, name));
    let (mut boss, _) = TestClient::connect(addr, "boss", None).await;
    let (mut moder, _) = TestClient::connect(addr, "mod", None).await;
    owner.change_roles(assign("boss", RoleId::ADMIN, true)).await;
    owner.change_roles(assign("mod", RoleId::MODERATOR, true)).await;

    moder.send(set(hangout, RoleId::EVERYONE, &[], &[Perm::Speak])).await;
    assert!(error(&mut moder).await.contains("can't change this channel's permissions"));
    boss.send(set(hangout, RoleId::ADMIN, &[], &[Perm::Speak])).await;
    assert!(error(&mut boss).await.contains("roles below your own"));
    boss.send(set(hangout, RoleId(99), &[], &[Perm::Speak])).await;
    assert!(error(&mut boss).await.contains("does not exist"));
    boss.send(set(hangout, RoleId::MODERATOR, &[Perm::ManagePermissions], &[])).await;
    let granted = perms::Override::allowing(RoleId::MODERATOR, Perms::of(&[Perm::ManagePermissions]));
    assert_eq!(owner.overrides(hangout).await, [granted]);

    let S2C::Permissions(p) = moder.until(|m| matches!(m, S2C::Permissions(_))).await else { unreachable!() };
    assert!(p.may_in(hangout, Perm::ManagePermissions) && !p.may_in(lobby, Perm::ManagePermissions));
    moder.until(|m| matches!(m, S2C::Roles(_))).await;
    moder.send(set(hangout, RoleId::MODERATOR, &[], &[Perm::Speak])).await;
    assert!(error(&mut moder).await.contains("roles below your own"));
    moder.send(set(lobby, RoleId::EVERYONE, &[], &[Perm::Speak])).await;
    assert!(error(&mut moder).await.contains("can't change this channel's permissions"));
    moder.send(set(hangout, RoleId::EVERYONE, &[], &[Perm::Speak])).await;
    let muted = perms::Override::denying(RoleId::EVERYONE, Perms::of(&[Perm::Speak]));
    assert_eq!(owner.overrides(hangout).await, [muted, granted]);
}
