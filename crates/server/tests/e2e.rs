//! Spins up a real server on localhost and drives it with raw protocol clients.

use std::time::Duration;

use t7_proto::*;
use t7_server::config::{Config, DEFAULT_CONFIG};
use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

struct TestClient {
    r: BufReader<OwnedReadHalf>,
    w: OwnedWriteHalf,
    buf: Vec<u8>,
}

impl TestClient {
    async fn connect(addr: std::net::SocketAddr, nick: &str, pw: Option<&str>) -> (Self, S2C) {
        Self::connect_as(addr, nick, pw, PROTOCOL_VERSION).await
    }
    async fn connect_as(addr: std::net::SocketAddr, nick: &str, pw: Option<&str>, protocol: u16) -> (Self, S2C) {
        let (r, w) = TcpStream::connect(addr).await.unwrap().into_split();
        let mut c = Self { r: BufReader::new(r), w, buf: Vec::new() };
        c.send(C2S::Hello {
            protocol,
            nickname: nick.into(),
            password: pw.map(Into::into),
        })
        .await;
        let first = c.recv().await;
        (c, first)
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
}

async fn start(cfg_text: &str) -> std::net::SocketAddr {
    let cfg: Config = toml::from_str(cfg_text).unwrap();
    // The server also binds UDP on the TCP port's number, so pick a port free for both.
    let listener = loop {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        if std::net::UdpSocket::bind(l.local_addr().unwrap()).is_ok() {
            break l;
        }
    };
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Err(e) = t7_server::serve(listener, &cfg).await {
            panic!("test server failed: {e}");
        }
    });
    addr
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
    b.send(C2S::JoinChannel { channel: 4 }).await;
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
    a.send(C2S::JoinChannel { channel: 2 }).await;
    a.recv().await;
    b.recv().await;
    b.send(C2S::JoinChannel { channel: 2 }).await;
    assert!(matches!(b.recv().await, S2C::Error { .. }));
}

/// A voice endpoint: one UDP socket per test client.
struct Voice {
    sock: tokio::net::UdpSocket,
    id: ClientId,
    token: u64,
    buf: Vec<u8>,
}

impl Voice {
    async fn open(server: std::net::SocketAddr, welcome: &S2C) -> Self {
        let S2C::Welcome { your_id, voice_token, .. } = welcome else { panic!("{welcome:?}") };
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(server).await.unwrap();
        let mut v = Self { sock, id: *your_id, token: *voice_token, buf: Vec::new() };
        // The ping registers our UDP address with the server.
        v.send(voice::ToServer::Ping { nonce: 5 }).await;
        assert_eq!(v.recv_owned().await, Some(Got::Pong(5)));
        v
    }
    async fn send(&mut self, msg: voice::ToServer<'_>) {
        voice::encode_to_server(self.id, self.token, msg, &mut self.buf);
        self.sock.send(&self.buf).await.unwrap();
    }
    async fn recv_owned(&mut self) -> Option<Got> {
        let mut buf = [0u8; 1500];
        let n = tokio::time::timeout(Duration::from_millis(300), self.sock.recv(&mut buf)).await.ok()?.ok()?;
        match voice::parse_to_client(&buf[..n])? {
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
async fn voice_is_routed_within_the_channel() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut a, wa) = TestClient::connect(addr, "a", None).await;
    let (mut b, wb) = TestClient::connect(addr, "b", None).await;
    let (mut c, wc) = TestClient::connect(addr, "c", None).await;
    let (mut va, mut vb, mut vc) =
        (Voice::open(addr, &wa).await, Voice::open(addr, &wb).await, Voice::open(addr, &wc).await);

    // c leaves for another channel; wait until everyone has seen the move.
    c.send(C2S::JoinChannel { channel: 4 }).await;
    for t in [&mut a, &mut b, &mut c] {
        while !matches!(t.recv().await, S2C::ClientMoved { .. }) {}
    }

    va.send(voice::ToServer::Audio { seq: 1, payload: &[9, 9, 9] }).await;
    assert_eq!(vb.recv_owned().await, Some(Got::Audio(va.id, 1, vec![9, 9, 9])));
    assert_eq!(vc.recv_owned().await, None, "other channel must not hear it");
    assert_eq!(va.recv_owned().await, None, "speaker must not hear itself");

    // A forged token is ignored.
    let real = va.token;
    va.token ^= 1;
    va.send(voice::ToServer::Audio { seq: 2, payload: &[1] }).await;
    assert_eq!(vb.recv_owned().await, None);
    va.token = real;

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
async fn pokes_reach_new_clients_and_old_clients_still_connect() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut a, wa) = TestClient::connect(addr, "a", None).await;
    let (mut b, wb) = TestClient::connect(addr, "b", None).await;
    // A client from before pokes existed is still welcome.
    let (_old, wold) = TestClient::connect_as(addr, "old", None, MIN_PROTOCOL_VERSION).await;
    let (S2C::Welcome { your_id: a_id, .. }, S2C::Welcome { your_id: b_id, .. }, S2C::Welcome { your_id: old_id, .. }) =
        (&wa, &wb, &wold)
    else {
        panic!("not welcomed: {wold:?}");
    };
    for _ in 0..2 {
        a.recv().await; // b, old joined
    }
    b.recv().await; // old joined

    a.send(C2S::Poke { target: *b_id, message: "  over here  ".into() }).await;
    assert_eq!(b.recv().await, S2C::Poked { from: *a_id, message: "over here".into() });

    a.send(C2S::Poke { target: *old_id, message: String::new() }).await;
    assert!(matches!(a.recv().await, S2C::Error { ref message } if message.contains("out of date")));

    a.send(C2S::Poke { target: *a_id, message: String::new() }).await;
    assert!(matches!(a.recv().await, S2C::Error { .. }), "can't poke yourself");

    a.send(C2S::Poke { target: *b_id, message: "x".repeat(MAX_POKE_LEN + 1) }).await;
    assert!(matches!(a.recv().await, S2C::Error { .. }));

    let (_, too_new) = TestClient::connect_as(addr, "future", None, PROTOCOL_VERSION + 1).await;
    assert!(matches!(too_new, S2C::Rejected { ref reason } if reason.contains("server needs an update")));
}
