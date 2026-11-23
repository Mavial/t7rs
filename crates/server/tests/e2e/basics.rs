//! Joining, chatting, voice, pokes, and what each connection proves.

use std::time::Duration;

use t7_proto::*;
use t7_server::config::DEFAULT_CONFIG;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::harness::{Got, HOME, TestClient, Voice, key, rejected, start};

#[tokio::test]
async fn join_move_chat_leave() {
    let addr = start(DEFAULT_CONFIG).await;

    let (mut a, welcome) = TestClient::connect(addr, "alice", None).await;
    let S2C::Welcome { your_id: a_id, channels, clients, .. } = welcome else { panic!("{welcome:?}") };
    assert_eq!(clients.len(), 1);
    assert_eq!(channels.len(), 7);

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
