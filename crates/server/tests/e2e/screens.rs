use std::net::SocketAddr;
use std::time::Duration;

use t7_proto::perms::{Perm, RoleId};
use t7_proto::*;
use t7_server::config::DEFAULT_CONFIG;

use crate::harness::{Got, HOME, TestClient, Voice, error, id_of, set, start, start_owned};

fn frame(frame: u32, payload: &[u8]) -> voice::ToServer<'_> {
    voice::ToServer::Video { frame, part: 0, parts: 1, key: true, payload }
}

fn got_frame(from: ClientId, frame: u32, payload: &[u8]) -> Got {
    Got::Video { from, frame, part: 0, parts: 1, key: true, payload: payload.to_vec() }
}

async fn connect_with_voice(addr: SocketAddr, nick: &str) -> (TestClient, Voice, Vec<ChannelInfo>) {
    let (c, welcome) = TestClient::connect(addr, nick, None).await;
    let v = Voice::open(addr, &welcome).await;
    let S2C::Welcome { channels, .. } = welcome else { unreachable!() };
    (c, v, channels)
}

fn watch(sharer: ClientId) -> C2S {
    C2S::Watch { sharer, on: true }
}

async fn share_and_watch(sharer: &mut TestClient, sharer_id: ClientId, viewer: &mut TestClient) {
    sharer.send(C2S::SetSharing(true)).await;
    sharer.until(|m| *m == S2C::ClientSharing { id: sharer_id, sharing: true }).await;
    viewer.send(watch(sharer_id)).await;
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

    away.send(watch(vs.id)).await;
    away.sync().await;
    assert!(!sharer.sync().await.contains(&S2C::SendKeyframe), "watched from another channel");
    viewer.send(watch(vs.id)).await;
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

    viewer.send(watch(vs.id)).await;
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

    viewer.send(watch(vs.id)).await;
    viewer.sync().await;
    vs.send(frame(1, &[4])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(vs.id, 1, &[4])));
}

#[tokio::test]
async fn keyframe_requests_are_coalesced_per_sharer() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut sharer, vs, _) = connect_with_voice(addr, "sharer").await;
    let (mut viewer, _vv, _) = connect_with_voice(addr, "viewer").await;
    let (mut idle, _vi, _) = connect_with_voice(addr, "idle").await;
    sharer.send(C2S::SetSharing(true)).await;
    sharer.sync().await;

    viewer.send(watch(vs.id)).await;
    viewer.send(C2S::NeedKeyframe { from: vs.id }).await;
    viewer.send(C2S::Watch { sharer: vs.id, on: false }).await;
    viewer.send(watch(vs.id)).await;
    viewer.sync().await;
    let asked = sharer.sync().await.into_iter().filter(|m| *m == S2C::SendKeyframe).count();
    assert_eq!(asked, 1, "three asks inside the interval");

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

#[tokio::test]
async fn a_viewer_watches_several_screens_and_stops_each_on_its_own() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut ann, mut va, _) = connect_with_voice(addr, "ann").await;
    let (mut bob, mut vb, _) = connect_with_voice(addr, "bob").await;
    let (mut viewer, mut vv, _) = connect_with_voice(addr, "viewer").await;
    share_and_watch(&mut ann, va.id, &mut viewer).await;
    share_and_watch(&mut bob, vb.id, &mut viewer).await;
    for sharer in [&mut ann, &mut bob] {
        assert!(sharer.sync().await.contains(&S2C::SendKeyframe), "each sharer is asked for a keyframe");
    }

    va.send(frame(0, &[1])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(va.id, 0, &[1])));
    vb.send(frame(0, &[2])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(vb.id, 0, &[2])));

    viewer.send(C2S::Watch { sharer: va.id, on: false }).await;
    viewer.sync().await;
    va.send(frame(1, &[3])).await;
    assert_eq!(vv.recv_owned().await, None, "stopped watching ann");
    vb.send(frame(1, &[4])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(vb.id, 1, &[4])), "still watching bob");

    viewer.send(watch(vv.id)).await;
    viewer.sync().await;
    vb.send(frame(2, &[5])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(vb.id, 2, &[5])), "watching yourself changes nothing");
}

#[tokio::test]
async fn a_viewer_watches_at_most_four_screens() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut viewer, mut vv, _) = connect_with_voice(addr, "viewer").await;
    let mut sharers = Vec::new();
    for i in 0..=MAX_WATCHED {
        let (mut c, v, _) = connect_with_voice(addr, &format!("sharer{i}")).await;
        c.send(C2S::SetSharing(true)).await;
        viewer.until(|m| *m == S2C::ClientSharing { id: v.id, sharing: true }).await;
        sharers.push((c, v));
    }
    assert_eq!(MAX_WATCHED, 4);
    for (_, v) in &sharers[..MAX_WATCHED] {
        viewer.send(watch(v.id)).await;
    }
    let (fifth, vf) = sharers.last_mut().unwrap();
    viewer.send(watch(vf.id)).await;
    assert_eq!(error(&mut viewer).await, "you can watch at most 4 screens at once");
    assert!(!fifth.sync().await.contains(&S2C::SendKeyframe), "the fifth is not asked for a keyframe");
    vf.send(frame(0, &[5])).await;
    assert_eq!(vv.recv_owned().await, None, "over the limit");
    for (_, v) in &mut sharers[..MAX_WATCHED] {
        v.send(frame(0, &[1])).await;
        assert_eq!(vv.recv_owned().await, Some(got_frame(v.id, 0, &[1])));
    }

    let first = sharers[0].1.id;
    viewer.send(C2S::Watch { sharer: first, on: false }).await;
    let (_, vf) = sharers.last_mut().unwrap();
    viewer.send(watch(vf.id)).await;
    viewer.sync().await;
    vf.send(frame(1, &[6])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(vf.id, 1, &[6])), "room for one after stopping one");
}

#[tokio::test]
async fn leaving_the_channel_ends_every_watch() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut ann, mut va, channels) = connect_with_voice(addr, "ann").await;
    let (mut bob, mut vb, _) = connect_with_voice(addr, "bob").await;
    let (mut viewer, mut vv, _) = connect_with_voice(addr, "viewer").await;
    let [lobby, hangout] = ["Lobby", "Hangout"].map(|name| id_of(&channels, name));
    share_and_watch(&mut ann, va.id, &mut viewer).await;
    share_and_watch(&mut bob, vb.id, &mut viewer).await;

    viewer.send(C2S::JoinChannel { channel: hangout, password: None }).await;
    viewer.send(C2S::JoinChannel { channel: lobby, password: None }).await;
    viewer.sync().await;
    for (v, payload) in [(&mut va, 1), (&mut vb, 2)] {
        v.send(frame(1, &[payload])).await;
        assert_eq!(vv.recv_owned().await, None, "the viewer left and came back");
    }
}

#[tokio::test]
async fn a_protocol_7_viewer_watches_one_screen_at_a_time() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut ann, mut va, _) = connect_with_voice(addr, "ann").await;
    let (mut bob, mut vb, _) = connect_with_voice(addr, "bob").await;
    let (mut viewer, welcome) = TestClient::connect_as(addr, "old", None, 7).await;
    let mut vv = Voice::open(addr, &welcome).await;
    for (c, id) in [(&mut ann, va.id), (&mut bob, vb.id)] {
        c.send(C2S::SetSharing(true)).await;
        viewer.until(|m| *m == S2C::ClientSharing { id, sharing: true }).await;
    }

    viewer.send(C2S::WatchOnly(Some(va.id))).await;
    viewer.send(C2S::WatchOnly(Some(vb.id))).await;
    viewer.sync().await;
    va.send(frame(0, &[1])).await;
    assert_eq!(vv.recv_owned().await, None, "watching bob instead");
    vb.send(frame(0, &[2])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(vb.id, 0, &[2])));

    viewer.send(C2S::WatchOnly(None)).await;
    viewer.sync().await;
    vb.send(frame(1, &[3])).await;
    assert_eq!(vv.recv_owned().await, None, "watching nobody");
}

fn preview(sharer: ClientId) -> C2S {
    C2S::Preview { sharer }
}

#[tokio::test]
async fn a_preview_is_the_sharers_next_keyframe_once() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut sharer, mut vs, _) = connect_with_voice(addr, "sharer").await;
    let (mut viewer, mut vv, _) = connect_with_voice(addr, "viewer").await;
    let (_idle, mut vi, _) = connect_with_voice(addr, "idle").await;
    sharer.send(C2S::SetSharing(true)).await;
    viewer.until(|m| *m == S2C::ClientSharing { id: vs.id, sharing: true }).await;

    viewer.send(preview(vs.id)).await;
    sharer.until(|m| *m == S2C::SendKeyframe).await;
    vs.send(voice::ToServer::Video { frame: 0, part: 0, parts: 1, key: false, payload: &[1] }).await;
    assert_eq!(vv.recv_owned().await, None, "not a keyframe");
    for part in 0..2 {
        let payload = [2 + part as u8];
        vs.send(voice::ToServer::Video { frame: 1, part, parts: 2, key: true, payload: &payload }).await;
        let got = Got::Video { from: vs.id, frame: 1, part, parts: 2, key: true, payload: payload.to_vec() };
        assert_eq!(vv.recv_owned().await, Some(got), "part {part} of the keyframe");
    }
    assert_eq!(vi.recv_owned().await, None, "asked by someone else");

    vs.send(voice::ToServer::Video { frame: 2, part: 0, parts: 1, key: false, payload: &[4] }).await;
    assert_eq!(vv.recv_owned().await, None, "the frame after it");
    vs.send(frame(3, &[5])).await;
    assert_eq!(vv.recv_owned().await, None, "the next keyframe: one preview per ask");
}

#[tokio::test]
async fn only_someone_in_the_sharers_channel_gets_a_preview() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut sharer, mut vs, channels) = connect_with_voice(addr, "sharer").await;
    let (mut viewer, mut vv, _) = connect_with_voice(addr, "viewer").await;
    let (mut away, mut va, _) = connect_with_voice(addr, "away").await;
    let [lobby, hangout] = ["Lobby", "Hangout"].map(|name| id_of(&channels, name));
    away.send(C2S::JoinChannel { channel: hangout, password: None }).await;
    sharer.send(C2S::SetSharing(true)).await;
    away.until(|m| *m == S2C::ClientSharing { id: vs.id, sharing: true }).await;
    sharer.sync().await;

    away.send(preview(vs.id)).await;
    viewer.send(preview(vv.id)).await;
    away.sync().await;
    viewer.sync().await;
    assert!(!sharer.sync().await.contains(&S2C::SendKeyframe), "asked from another channel, or of yourself");
    vs.send(frame(0, &[1])).await;
    assert_eq!(va.recv_owned().await, None, "in another channel");
    assert_eq!(vv.recv_owned().await, None, "didn't ask");

    viewer.send(preview(vs.id)).await;
    sharer.until(|m| *m == S2C::SendKeyframe).await;
    viewer.send(C2S::JoinChannel { channel: hangout, password: None }).await;
    viewer.send(C2S::JoinChannel { channel: lobby, password: None }).await;
    viewer.sync().await;
    vs.send(frame(1, &[2])).await;
    assert_eq!(vv.recv_owned().await, None, "left the channel after asking");
}

#[tokio::test]
async fn previews_are_rate_limited() {
    let addr = start(DEFAULT_CONFIG).await;
    let (mut sharer, mut vs, _) = connect_with_voice(addr, "sharer").await;
    let (mut viewer, mut vv, _) = connect_with_voice(addr, "viewer").await;
    sharer.send(C2S::SetSharing(true)).await;
    viewer.until(|m| *m == S2C::ClientSharing { id: vs.id, sharing: true }).await;

    for n in 0..PREVIEW_BURST {
        viewer.send(preview(vs.id)).await;
        viewer.sync().await;
        vs.send(frame(n, &[n as u8])).await;
        assert_eq!(vv.recv_owned().await, Some(got_frame(vs.id, n, &[n as u8])), "ask {n} of the burst");
    }
    viewer.send(preview(vs.id)).await;
    viewer.sync().await;
    vs.send(frame(PREVIEW_BURST, &[9])).await;
    assert_eq!(vv.recv_owned().await, None, "one ask over the burst");

    tokio::time::sleep(PREVIEW_REFILL).await;
    viewer.send(preview(vs.id)).await;
    viewer.sync().await;
    vs.send(frame(PREVIEW_BURST + 1, &[10])).await;
    assert_eq!(vv.recv_owned().await, Some(got_frame(vs.id, PREVIEW_BURST + 1, &[10])), "one more after a while");
}
