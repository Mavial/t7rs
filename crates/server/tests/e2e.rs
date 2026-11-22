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
        let (r, w) = TcpStream::connect(addr).await.unwrap().into_split();
        let mut c = Self { r: BufReader::new(r), w, buf: Vec::new() };
        c.send(C2S::Hello {
            protocol: PROTOCOL_VERSION,
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { t7_server::serve(listener, &cfg).await });
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
