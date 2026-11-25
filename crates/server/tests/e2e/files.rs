use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use t7_proto::files::{self, CHUNK_BYTES, ChannelFile, Command, Event, Reply, Request, SessionKey, Upload};
use t7_proto::perms::{Perm, Perms, RoleId};
use t7_proto::*;
use tokio::net::TcpStream;

use crate::harness::*;

const LEGACY: &str = r#"
[[channels]]
name = "Lobby"
default = true
[[channels]]
name = "Studio"
password = "open"
[[channels]]
name = "Hangout"
"#;

type Transfer = tls::ClientStream<TcpStream>;

async fn subscribe(c: &mut TestClient) -> (u64, SessionKey) {
    c.send(C2S::Files(Command::Subscribe)).await;
    let S2C::Files(Event::Ready { limit, key }) = c.until(|m| matches!(m, S2C::Files(Event::Ready { .. }))).await
    else {
        unreachable!()
    };
    c.until(|m| matches!(m, S2C::Files(Event::Access { .. }))).await;
    (limit, key)
}

async fn open(addr: SocketAddr, nick: &str, client: ClientId, ticket: SessionKey, request: Request) -> Transfer {
    let (mut stream, _) = tls::connect(TcpStream::connect(addr).await.unwrap(), None).await.unwrap();
    let identity = key(nick).prove(&tls::exporter(stream.get_ref().1));
    write_frame(&mut stream, &C2S::FileHello { client, key: ticket, identity, request }).await.unwrap();
    stream
}

async fn reply(stream: &mut Transfer) -> Reply {
    tokio::time::timeout(Duration::from_secs(3), read_frame(stream, &mut Vec::new())).await.unwrap().unwrap()
}

async fn list(c: &mut TestClient, query: &str) -> Vec<ChannelFile> {
    c.send(C2S::Files(Command::List { request: 42, channel: None, before: None, query: query.into() })).await;
    match c.until(|m| matches!(m, S2C::Files(Event::Page { request: 42, .. }))).await {
        S2C::Files(Event::Page { files, .. }) => files,
        _ => unreachable!(),
    }
}

fn upload(channel: ChannelId, name: &str, size: u64) -> Request {
    Request::Upload { channel, name: name.into(), size, caption: "A file in chat".into() }
}

async fn join(c: &mut TestClient, client: ClientId, channel: ChannelId, password: Option<&str>) {
    c.send(C2S::JoinChannel { channel, password: password.map(str::to_owned) }).await;
    c.until(|m| matches!(m, S2C::ClientMoved { id, channel: moved } if *id == client && *moved == channel)).await;
}

async fn no_partial(dir: &Path) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if std::fs::read_dir(dir.join("files"))
                .unwrap()
                .all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".upload-"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("partial upload was left on disk");
}

#[tokio::test]
async fn round_trip_password_and_restart_after_channel_reorder() {
    let dir = data_dir("attachment-restart");
    std::fs::write(dir.join("channels.toml"), LEGACY).unwrap();
    let (addr, task) = start_in(LEGACY, Some(dir.to_path_buf())).await;
    let (mut admin, admin_id, channels) = claim(addr, &dir, HOME).await;
    let studio = id_of(&channels, "Studio");
    assert!(std::fs::read_to_string(dir.join("channels.toml")).unwrap().contains("file_key"));
    let lobby = id_of(&channels, "Lobby");
    let (mut sender, welcome) = TestClient::connect(addr, "sender", None).await;
    let sender_id = welcomed_id(&welcome);
    let (limit, ticket) = subscribe(&mut sender).await;
    assert_eq!(limit, files::DEFAULT_LIMIT);
    join(&mut sender, sender_id, studio, Some("open")).await;

    let (mut legacy, hello) = TestClient::connect_as(addr, "older", None, 9).await;
    join(&mut legacy, welcomed_id(&hello), studio, Some("open")).await;
    let bytes: Vec<_> = (0..CHUNK_BYTES + 17).map(|i| (i % 251) as u8).collect();
    let mut stream = open(addr, "sender", sender_id, ticket, upload(studio, "Replay.bin", bytes.len() as u64)).await;
    assert!(matches!(reply(&mut stream).await, Reply::Ready { size } if size == bytes.len() as u64));
    write_frame(&mut stream, &Upload::Chunk(bytes[..CHUNK_BYTES].to_vec())).await.unwrap();
    assert_eq!(list(&mut sender, "").await, []);
    write_frame(&mut stream, &Upload::Chunk(bytes[CHUNK_BYTES..].to_vec())).await.unwrap();
    write_frame(&mut stream, &Upload::Finish).await.unwrap();
    assert!(matches!(reply(&mut stream).await, Reply::Done));
    assert!(!legacy.sync().await.iter().any(|m| matches!(m, S2C::Files(_))));
    let posted = list(&mut sender, "rEPlAy").await.pop().unwrap();
    assert_eq!(posted.attachment.caption, "A file in chat");
    assert_eq!(posted.attachment.author, key("sender").identity());
    let file = posted.attachment.key.id;
    let request = Request::Download { id: file };

    admin
        .send(C2S::EditChannel {
            id: studio,
            settings: ChannelSettings {
                name: "Studio".into(),
                topic: String::new(),
                max_clients: None,
                password: PasswordChange::Set("updated".into()),
            },
        })
        .await;
    sender.until(|m| matches!(m,S2C::Files(Event::Access { readable,.. }) if !readable.contains(&studio))).await;
    assert_eq!(list(&mut sender, "").await, []);
    sender.send(C2S::JoinChannel { channel: studio, password: Some("updated".into()) }).await;
    sender.until(|m| matches!(m,S2C::Files(Event::Access { readable,.. }) if readable.contains(&studio))).await;
    assert_eq!(list(&mut sender, "").await, std::slice::from_ref(&posted));
    join(&mut sender, sender_id, lobby, None).await;
    assert_eq!(list(&mut sender, "").await, []);
    assert!(matches!(
        reply(&mut open(addr, "sender", sender_id, ticket, request.clone()).await).await,
        Reply::Error(_)
    ));
    let (_, admin_ticket) = subscribe(&mut admin).await;
    assert!(matches!(
        reply(&mut open(addr, "sender", admin_id, admin_ticket, request.clone()).await).await,
        Reply::Error(_)
    ));
    assert!(matches!(reply(&mut open(addr, "admin", admin_id, ticket, request.clone()).await).await, Reply::Error(_)));

    admin.send(C2S::MoveChannel { id: studio, parent: None, position: 0 }).await;
    admin.channels().await;
    admin.send(C2S::Files(Command::SetLimit(6 * files::GB))).await;
    admin.until(|m| matches!(m, S2C::Files(Event::Limit(_)))).await;
    drop((sender, admin, stream));
    task.abort();
    let _ = task.await;

    let (addr, _) = start_in(LEGACY, Some(dir.to_path_buf())).await;
    let (mut admin, welcome) = TestClient::connect(addr, "admin", None).await;
    let S2C::Welcome { your_id, channels, .. } = welcome else { panic!("{welcome:?}") };
    let new_studio = id_of(&channels, "Studio");
    assert_ne!(new_studio, studio);
    let (limit, ticket) = subscribe(&mut admin).await;
    assert_eq!(limit, 6 * files::GB);
    assert_eq!(list(&mut admin, "").await[0], ChannelFile { channel: new_studio, ..posted });
    let mut stream = open(addr, "admin", your_id, ticket, request.clone()).await;
    assert!(matches!(reply(&mut stream).await, Reply::Ready { size } if size == bytes.len() as u64));
    let mut saved = Vec::new();
    while let Reply::Chunk(chunk) = reply(&mut stream).await {
        assert!(chunk.len() <= CHUNK_BYTES);
        saved.extend(chunk);
    }
    assert_eq!(saved, bytes);
    let mut large =
        open(addr, "admin", your_id, ticket, upload(id_of(&channels, "Lobby"), "large.zip", 5 * files::GB)).await;
    assert!(matches!(reply(&mut large).await, Reply::Ready { size } if size == 5 * files::GB));
    drop(large);
    no_partial(&dir).await;

    admin.send(C2S::DeleteChannel { id: new_studio }).await;
    admin.channels().await;
    assert_eq!(list(&mut admin, "").await, []);
    assert!(matches!(reply(&mut open(addr, "admin", your_id, ticket, request).await).await, Reply::Error(_)));
}

#[tokio::test]
async fn rejects_overruns_and_revokes_access_during_upload() {
    let dir = data_dir("attachment-errors");
    let (addr, _) = start_in(LEGACY, Some(dir.to_path_buf())).await;
    let (mut admin, _, channels) = claim(addr, &dir, HOME).await;
    let (mut guest, welcome) = TestClient::connect(addr, "guest", None).await;
    let guest_id = welcomed_id(&welcome);
    let lobby = id_of(&channels, "Lobby");
    let (_, ticket) = subscribe(&mut guest).await;
    guest.send(C2S::Files(Command::SetLimit(1))).await;
    assert!(matches!(
        guest.until(|m| matches!(m, S2C::Files(Event::Error { .. }))).await,
        S2C::Files(Event::Error { .. })
    ));
    for request in [
        upload(lobby, "large.zip", files::DEFAULT_LIMIT + 1),
        upload(lobby, "../unsafe", 0),
        upload(id_of(&channels, "Studio"), "file.txt", 0),
    ] {
        assert!(matches!(reply(&mut open(addr, "guest", guest_id, ticket, request).await).await, Reply::Error(_)));
    }
    let mut short = open(addr, "guest", guest_id, ticket, upload(lobby, "short.bin", 3)).await;
    assert!(matches!(reply(&mut short).await, Reply::Ready { .. }));
    write_frame(&mut short, &Upload::Chunk(vec![0; 4])).await.unwrap();
    assert!(matches!(reply(&mut short).await, Reply::Error(_)));
    no_partial(&dir).await;

    let mut revoked = open(addr, "guest", guest_id, ticket, upload(lobby, "revoked.bin", 1)).await;
    assert!(matches!(reply(&mut revoked).await, Reply::Ready { .. }));
    write_frame(&mut revoked, &Upload::Chunk(vec![42])).await.unwrap();
    admin
        .send(C2S::SetOverride {
            channel: lobby,
            role: RoleId::EVERYONE,
            allow: Perms::NONE,
            deny: Perms::of(&[Perm::SendMessages]),
        })
        .await;
    guest.until(|m| matches!(m, S2C::Files(Event::Access { can_post_here: false, .. }))).await;
    write_frame(&mut revoked, &Upload::Finish).await.unwrap();
    assert!(matches!(reply(&mut revoked).await, Reply::Error(_)));
    no_partial(&dir).await;
    assert_eq!(list(&mut guest, "").await, []);

    let (mut legacy, _) = TestClient::connect_as(addr, "legacy", None, 9).await;
    assert!(!legacy.sync().await.iter().any(|m| matches!(m, S2C::Files(_))));
}
