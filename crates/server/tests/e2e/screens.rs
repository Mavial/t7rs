//! Screen sharing: who receives a share's video, and when a share or a watch ends.

use std::net::SocketAddr;
use std::time::Duration;

use t7_proto::perms::{Perm, RoleId};
use t7_proto::*;
use t7_server::config::DEFAULT_CONFIG;

use crate::harness::{Got, HOME, TestClient, Voice, error, id_of, set, start, start_owned};

/// One whole access unit in a single datagram.
fn frame(frame: u32, payload: &[u8]) -> voice::ToServer<'_> {
    voice::ToServer::Video { frame, part: 0, parts: 1, key: true, payload }
}

fn got_frame(from: ClientId, frame: u32, payload: &[u8]) -> Got {
    Got::Video { from, frame, part: 0, parts: 1, key: true, payload: payload.to_vec() }
}

/// Connects `nick` with a voice link; also returns the channels it was welcomed with.
async fn connect_with_voice(addr: SocketAddr, nick: &str) -> (TestClient, Voice, Vec<ChannelInfo>) {
    let (c, welcome) = TestClient::connect(addr, nick, None).await;
    let v = Voice::open(addr, &welcome).await;
    let S2C::Welcome { channels, .. } = welcome else { unreachable!() };
    (c, v, channels)
}

/// Starts `sharer`'s share and has `viewer` watch it, once the server has done both.
async fn share_and_watch(sharer: &mut TestClient, sharer_id: ClientId, viewer: &mut TestClient) {
    sharer.send(C2S::SetSharing(true)).await;
    sharer.until(|m| *m == S2C::ClientSharing { id: sharer_id, sharing: true }).await;
    viewer.send(C2S::Watch(Some(sharer_id))).await;
    viewer.sync().await;
}

#[tokio::test]
async fn screens_reach_only_those_watching_in_the_sharers_channel() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut sharer, mut vs, channels) = connect_with_voice(addr, "sharer").await;
    let (mut viewer, mut vv, _) = connect_with_voice(addr, "viewer").await;
    let (mut idle, mut vi, _) = connect_with_voice(addr, "idle").await;
    let (mut away, mut va, _) = connect_with_voice(addr, "away").await;
    away.send(C2S::JoinChannel { channel: id_of(&channels, "Hangout"), password: None }).await;
    away.sync().await;

    sharer.send(C2S::SetSharing(true)).await;
    sharer.send(C2S::SetSharing(true)).await;
    let started = S2C::ClientSharing { id: vs.id, sharing: true };
    for c in [&mut sharer, &mut viewer, &mut idle, &mut away] {
        let seen = c.sync().await;
        assert_eq!(seen.iter().filter(|m| **m == started).count(), 1, "the repeat is no change: {seen:?}");
    }

    away.send(C2S::Watch(Some(vs.id))).await;
    away.sync().await;
    assert!(!sharer.sync().await.contains(&S2C::SendKeyframe), "watched from another channel");
    viewer.send(C2S::Watch(Some(vs.id))).await;
    sharer.until(|m| *m == S2C::SendKeyframe).await;

    vs.send(voice::ToServer::Video { frame: 7, part: 1, parts: 3, key: false, payload: &[1, 2, 3] }).await;
    let forwarded = Got::Video { from: vs.id, frame: 7, part: 1, parts: 3, key: false, payload: vec![1, 2, 3] };
    assert_eq!(vv.recv_owned().await, Some(forwarded));
    assert_eq!(vi.recv_owned().await, None, "not watching");
    assert_eq!(va.recv_owned().await, None, "in another channel");
    assert_eq!(vs.recv_owned().await, None, "the sharer");

    viewer.send(C2S::SetStatus(ClientStatus { deafened: true, ..Default::default() })).await;
    viewer.sync().await;
    vs.send(frame(8, &[4])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(vs.id, 8, &[4])), "sound off doesn't stop video");

    vi.send(frame(0, &[5])).await;
    assert_eq!(vv.recv_owned().await, None, "from someone not sharing");
}

#[tokio::test]
async fn moving_ends_a_share_and_what_the_mover_watches() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut sharer, mut vs, channels) = connect_with_voice(addr, "sharer").await;
    let (mut viewer, mut vv, _) = connect_with_voice(addr, "viewer").await;
    let [lobby, hangout] = ["Lobby", "Hangout"].map(|name| id_of(&channels, name));
    share_and_watch(&mut sharer, vs.id, &mut viewer).await;
    vs.send(frame(0, &[1])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(vs.id, 0, &[1])));

    viewer.send(C2S::JoinChannel { channel: hangout, password: None }).await;
    viewer.send(C2S::JoinChannel { channel: lobby, password: None }).await;
    viewer.sync().await;
    vs.send(frame(1, &[2])).await;
    assert_eq!(vv.recv_owned().await, None, "the viewer moved away and back");

    viewer.send(C2S::Watch(Some(vs.id))).await;
    viewer.sync().await;
    vs.send(frame(2, &[3])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(vs.id, 2, &[3])));

    sharer.send(C2S::JoinChannel { channel: hangout, password: None }).await;
    sharer.sync().await;
    let mut seen = viewer.sync().await;
    seen.retain(|m| matches!(m, S2C::ClientSharing { .. } | S2C::ClientMoved { .. }));
    let ended = S2C::ClientSharing { id: vs.id, sharing: false };
    assert_eq!(seen, [ended, S2C::ClientMoved { id: vs.id, channel: hangout }], "the share ends first");
    viewer.send(C2S::JoinChannel { channel: hangout, password: None }).await;
    sharer.send(C2S::SetSharing(true)).await;
    viewer.until(|m| *m == S2C::ClientSharing { id: vs.id, sharing: true }).await;
    vs.send(frame(0, &[4])).await;
    assert_eq!(vv.recv_owned().await, None, "watching ended with the share");
}

#[tokio::test]
async fn a_stopped_share_reaches_nobody() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut sharer, mut vs, _) = connect_with_voice(addr, "sharer").await;
    let (mut viewer, mut vv, _) = connect_with_voice(addr, "viewer").await;
    share_and_watch(&mut sharer, vs.id, &mut viewer).await;
    vs.send(frame(0, &[1])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(vs.id, 0, &[1])));

    sharer.send(C2S::SetSharing(false)).await;
    viewer.until(|m| *m == S2C::ClientSharing { id: vs.id, sharing: false }).await;
    vs.send(frame(1, &[2])).await;
    assert_eq!(vv.recv_owned().await, None, "stopped");

    sharer.send(C2S::SetSharing(true)).await;
    viewer.until(|m| *m == S2C::ClientSharing { id: vs.id, sharing: true }).await;
    vs.send(frame(0, &[3])).await;
    assert_eq!(vv.recv_owned().await, None, "watching ended with the share");

    viewer.send(C2S::Watch(Some(vs.id))).await;
    viewer.sync().await;
    vs.send(frame(1, &[4])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(vs.id, 1, &[4])));
    viewer.send(C2S::Watch(Some(vv.id))).await;
    viewer.sync().await;
    vs.send(frame(2, &[5])).await;
    assert_eq!(vv.recv_owned().await, None, "watching yourself watches nobody");
}

#[tokio::test]
async fn keyframe_requests_are_coalesced_per_sharer() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut sharer, vs, _) = connect_with_voice(addr, "sharer").await;
    let (mut viewer, _vv, _) = connect_with_voice(addr, "viewer").await;
    let (mut idle, _vi, _) = connect_with_voice(addr, "idle").await;
    sharer.send(C2S::SetSharing(true)).await;
    sharer.sync().await;

    viewer.send(C2S::Watch(Some(vs.id))).await;
    viewer.send(C2S::NeedKeyframe { from: vs.id }).await;
    viewer.send(C2S::Watch(Some(vs.id))).await;
    viewer.sync().await;
    let asked = sharer.sync().await.into_iter().filter(|m| *m == S2C::SendKeyframe).count();
    assert_eq!(asked, 1, "three requests inside the interval");

    tokio::time::sleep(voice::KEYFRAME_INTERVAL + Duration::from_millis(50)).await;
    idle.send(C2S::NeedKeyframe { from: vs.id }).await;
    idle.sync().await;
    assert!(!sharer.sync().await.contains(&S2C::SendKeyframe), "asked by someone not watching");
    viewer.send(C2S::NeedKeyframe { from: vs.id }).await;
    sharer.until(|m| *m == S2C::SendKeyframe).await;
}

#[tokio::test]
async fn only_those_who_may_speak_share_their_screens() {
    let (addr, mut owner, _, channels) = start_owned("share-speak", HOME).await;
    let (mut loud, mut vl, _) = connect_with_voice(addr, "loud").await;
    let (mut viewer, mut vv, _) = connect_with_voice(addr, "viewer").await;
    let (mut quiet, vq, _) = connect_with_voice(addr, "quiet").await;
    share_and_watch(&mut loud, vl.id, &mut viewer).await;
    vl.send(frame(0, &[1])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(vl.id, 0, &[1])));

    owner.send(C2S::ServerMute { target: vl.id, muted: true }).await;
    viewer.until(|m| *m == S2C::ClientSpeech { id: vl.id, speech: Speech::ServerMuted }).await;
    vl.send(frame(1, &[2])).await;
    assert_eq!(vv.recv_owned().await, None, "a server-muted sharer's video was forwarded");

    owner.send(set(id_of(&channels, "Lobby"), RoleId::EVERYONE, &[], &[Perm::Speak])).await;
    quiet.until(|m| *m == S2C::ClientSpeech { id: vq.id, speech: Speech::NotHere }).await;
    quiet.send(C2S::SetSharing(true)).await;
    assert_eq!(error(&mut quiet).await, "you can't share your screen here");
    let seen = viewer.sync().await;
    assert!(!seen.iter().any(|m| matches!(m, S2C::ClientSharing { id, .. } if *id == vq.id)), "{seen:?}");
}
