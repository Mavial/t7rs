//! Lobbies and the temporary rooms opened in them.

use std::fmt::Write as _;

use t7_proto::perms::Perm;
use t7_proto::*;
use t7_server::config::DEFAULT_CONFIG;

use crate::harness::{TestClient, error, id_of, settings, start, welcomed_id};

async fn until_owned_by(c: &mut TestClient, channel: ChannelId, owner: Option<ClientId>) {
    c.until(|m| matches!(m, S2C::Channels(l) if l.iter().any(|ch| ch.id == channel && ch.owner == owner))).await;
}

#[tokio::test]
async fn a_room_passes_to_whoever_has_been_in_it_longest() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut a, wa) = TestClient::connect(addr, "a", None).await;
    let (mut b, wb) = TestClient::connect(addr, "b", None).await;
    let (mut c, wc) = TestClient::connect(addr, "c", None).await;
    let (a_id, b_id, c_id) = (welcomed_id(&wa), welcomed_id(&wb), welcomed_id(&wc));
    let S2C::Welcome { channels, .. } = &wa else { unreachable!() };
    let lobby = id_of(channels, "Lobby");
    let raid =
        C2S::CreateChannel { parent: None, settings: settings("Raid"), temporary: true, kind: ChannelKind::Voice };
    a.send(raid).await;
    let raid = id_of(&a.channels().await, "Raid");
    for (t, id) in [(&mut b, b_id), (&mut c, c_id)] {
        t.send(C2S::JoinChannel { channel: raid, password: None }).await;
        a.until(|m| *m == S2C::ClientMoved { id, channel: raid }).await;
    }

    a.send(C2S::JoinChannel { channel: lobby, password: None }).await;
    until_owned_by(&mut c, raid, Some(b_id)).await;
    let powers = |m: &S2C| matches!(m, S2C::Permissions(p) if p.may_in(raid, Perm::Kick));
    let S2C::Permissions(p) = b.until(powers).await else { unreachable!() };
    assert!(p.may_in(raid, Perm::ManageChannels) && !p.may_in(lobby, Perm::Kick));
    a.send(C2S::JoinChannel { channel: raid, password: None }).await;
    c.until(|m| *m == S2C::ClientMoved { id: a_id, channel: raid }).await;
    let heard = c.sync().await;
    assert!(heard.iter().all(|m| !matches!(m, S2C::Channels(_))), "{heard:?}");

    drop(b);
    until_owned_by(&mut a, raid, Some(c_id)).await;
}

#[tokio::test]
async fn joining_a_lobby_opens_a_room_of_your_own() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut ana, wa) = TestClient::connect(addr, "ana", None).await;
    let (mut ben, wb) = TestClient::connect(addr, "ben", None).await;
    let (ana_id, ben_id) = (welcomed_id(&wa), welcomed_id(&wb));
    let S2C::Welcome { channels, .. } = &wa else { unreachable!() };
    let [lobby, rooms] = ["Lobby", "Create a room"].map(|name| id_of(channels, name));

    ana.send(C2S::JoinChannel { channel: rooms, password: None }).await;
    let list = ben.channels().await;
    let room = list.iter().find(|c| c.parent == Some(rooms)).expect("a room in the lobby");
    assert_eq!((room.name.as_str(), room.temporary, room.owner), ("ana's room", true, Some(ana_id)));
    let room = room.id;
    assert_eq!(
        ben.until(|m| matches!(m, S2C::ClientMoved { .. })).await,
        S2C::ClientMoved { id: ana_id, channel: room }
    );
    let S2C::Permissions(p) = ana.until(|m| matches!(m, S2C::Permissions(p) if p.sees(room))).await else {
        unreachable!()
    };
    assert!(p.may_in(room, Perm::Kick) && p.may_in(room, Perm::ManageChannels) && !p.may_in(rooms, Perm::Kick));

    ben.send(C2S::JoinChannel { channel: rooms, password: None }).await;
    let S2C::ClientMoved { channel: bens, .. } =
        ana.until(|m| matches!(m, S2C::ClientMoved { id, .. } if *id == ben_id)).await
    else {
        unreachable!()
    };
    assert_ne!(bens, room);
    let S2C::Welcome { clients, channels, .. } = TestClient::connect(addr, "cid", None).await.1 else { unreachable!() };
    assert!(clients.iter().all(|c| c.channel != rooms), "{clients:?}");
    assert_eq!(channels.iter().find(|c| c.id == bens).unwrap().name, "ben's room");

    ana.send(C2S::JoinChannel { channel: lobby, password: None }).await;
    let gone = |m: &S2C| matches!(m, S2C::Channels(l) if l.iter().all(|c| c.id != room));
    let S2C::Channels(list) = ben.until(gone).await else { unreachable!() };
    assert!(list.iter().any(|c| c.id == bens), "Ben's stays");
}

#[tokio::test]
async fn a_full_server_opens_no_room() {
    let mut cfg = String::from("[[channels]]\nname = \"Lobby\"\n\n[[channels]]\nname = \"Rooms\"\nkind = \"lobby\"\n");
    for i in 2..MAX_CHANNELS {
        writeln!(cfg, "\n[[channels]]\nname = \"Channel {i}\"").unwrap();
    }
    let addr = start(&cfg).await;
    let (mut ana, wa) = TestClient::connect(addr, "ana", None).await;
    let S2C::Welcome { channels, .. } = &wa else { unreachable!() };
    assert_eq!(channels.len(), MAX_CHANNELS);
    ana.send(C2S::JoinChannel { channel: id_of(channels, "Rooms"), password: None }).await;
    let refusal = error(&mut ana).await;
    assert!(refusal.contains("no room for you there") && refusal.contains("maximum of 200 channels"), "{refusal}");
    let after = ana.sync().await;
    assert!(after.iter().all(|m| !matches!(m, S2C::ClientMoved { .. } | S2C::Channels(_))), "{after:?}");
}
