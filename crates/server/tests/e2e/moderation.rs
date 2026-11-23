//! Kicks, bans, server mutes, and moving people.

use std::net::Ipv4Addr;
use std::time::Duration;

use t7_proto::perms::{Perm, Perms, RoleChange, RoleId};
use t7_proto::*;
use t7_server::config::DEFAULT_CONFIG;

use crate::harness::{
    ADMIN_IP, ELSEWHERE, Got, HOME, PRIVATE, TestClient, Voice, assign, claim, data_dir, error, id_of, key, rejected,
    set, settings, start_in, start_owned, welcomed_id,
};

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

#[tokio::test]
async fn voice_is_dropped_from_those_who_may_not_speak_or_are_server_muted() {
    let (addr, mut owner, _, channels) = start_owned("speak", HOME).await;
    let [lobby, hangout] = ["Lobby", "Hangout"].map(|name| id_of(&channels, name));
    let (_talker, wt) = TestClient::connect(addr, "talker", None).await;
    let (_loud, wl) = TestClient::connect(addr, "loud", None).await;
    let (mut quiet, wq) = TestClient::connect(addr, "quiet", None).await;
    let (mut ear, we) = TestClient::connect(addr, "ear", None).await;
    let (loud_id, quiet_id) = (welcomed_id(&wl), welcomed_id(&wq));
    let mut v = Vec::new();
    for welcome in [&wt, &wl, &wq, &we] {
        v.push(Voice::open(addr, welcome).await);
    }
    let [vt, vl, vq, ve] = &mut v[..] else { unreachable!() };
    vq.send(voice::ToServer::Audio { seq: 1, payload: &[1] }).await;
    assert_eq!(ve.recv_owned().await, Some(Got::Audio(vq.id, 1, vec![1])), "everyone may speak at first");

    owner.change_roles(assign("talker", RoleId::MODERATOR, true)).await;
    owner.change_roles(assign("loud", RoleId::MODERATOR, true)).await;
    owner.send(set(lobby, RoleId::MODERATOR, &[Perm::Speak], &[])).await;
    owner.send(set(lobby, RoleId::EVERYONE, &[], &[Perm::Speak])).await;
    let silenced = S2C::ClientSpeech { id: quiet_id, speech: Speech::NotHere };
    for c in [&mut ear, &mut quiet] {
        c.until(|m| *m == silenced).await;
    }
    vq.send(voice::ToServer::Audio { seq: 2, payload: &[2] }).await;
    assert_eq!(ve.recv_owned().await, None, "Speak-denied voice was forwarded");
    vt.send(voice::ToServer::Audio { seq: 1, payload: &[3] }).await;
    assert_eq!(ve.recv_owned().await, Some(Got::Audio(vt.id, 1, vec![3])));
    let S2C::Welcome { clients, .. } = TestClient::connect(addr, "late", None).await.1 else { unreachable!() };
    assert_eq!(clients.iter().find(|c| c.id == quiet_id).unwrap().speech, Speech::NotHere);

    vl.send(voice::ToServer::Audio { seq: 1, payload: &[4] }).await;
    assert_eq!(ve.recv_owned().await, Some(Got::Audio(vl.id, 1, vec![4])));
    owner.send(C2S::ServerMute { target: loud_id, muted: true }).await;
    ear.until(|m| *m == S2C::ClientSpeech { id: loud_id, speech: Speech::ServerMuted }).await;
    vl.send(voice::ToServer::Audio { seq: 2, payload: &[5] }).await;
    assert_eq!(ve.recv_owned().await, None, "server-muted voice was forwarded");
    vt.send(voice::ToServer::Audio { seq: 2, payload: &[6] }).await;
    assert_eq!(ve.recv_owned().await, Some(Got::Audio(vt.id, 2, vec![6])));

    quiet.send(C2S::JoinChannel { channel: hangout, password: None }).await;
    ear.until(|m| *m == S2C::ClientSpeech { id: quiet_id, speech: Speech::Allowed }).await;
}

#[tokio::test]
async fn a_server_mute_holds_across_reconnects_until_lifted_or_a_restart() {
    let dir = data_dir("server-mute");
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut owner, owner_id, _) = claim(addr, &dir, HOME).await;
    let (mut moder, wm) = TestClient::connect(addr, "mod", None).await;
    let (user, wu) = TestClient::connect(addr, "user", None).await;
    let user_id = welcomed_id(&wu);
    owner.change_roles(assign("mod", RoleId::MODERATOR, true)).await;
    moder.until(|m| matches!(m, S2C::Permissions(p) if p.server.has(Perm::MuteMembers))).await;

    moder.send(C2S::ServerMute { target: owner_id, muted: true }).await;
    assert!(error(&mut moder).await.contains("their role isn't below yours"));
    moder.send(C2S::ServerMute { target: user_id, muted: true }).await;
    owner.until(|m| *m == S2C::ClientSpeech { id: user_id, speech: Speech::ServerMuted }).await;

    drop(user);
    owner.until(|m| matches!(m, S2C::ClientLeft { id, .. } if *id == user_id)).await;
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let user_id = welcomed_id(&wu);
    let S2C::Welcome { clients, .. } = &wu else { unreachable!() };
    assert_eq!(clients.iter().find(|c| c.id == user_id).unwrap().speech, Speech::ServerMuted);
    let (mut vu, mut vm) = (Voice::open(addr, &wu).await, Voice::open(addr, &wm).await);
    vu.send(voice::ToServer::Audio { seq: 1, payload: &[1] }).await;
    assert_eq!(vm.recv_owned().await, None, "server-muted voice was forwarded");

    user.send(C2S::ServerMute { target: user_id, muted: false }).await;
    assert!(error(&mut user).await.contains("can't unmute"));
    moder.send(C2S::ServerMute { target: user_id, muted: false }).await;
    user.until(|m| *m == S2C::ClientSpeech { id: user_id, speech: Speech::Allowed }).await;
    vu.send(voice::ToServer::Audio { seq: 2, payload: &[2] }).await;
    assert_eq!(vm.recv_owned().await, Some(Got::Audio(user_id, 2, vec![2])));

    moder.send(C2S::ServerMute { target: user_id, muted: true }).await;
    user.until(|m| *m == S2C::ClientSpeech { id: user_id, speech: Speech::ServerMuted }).await;
    task.abort();
    drop((owner, moder, user));
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (_, wu) = TestClient::connect(addr, "user", None).await;
    let S2C::Welcome { your_id, clients, .. } = &wu else { panic!("not welcomed: {wu:?}") };
    assert_eq!(clients.iter().find(|c| c.id == *your_id).unwrap().speech, Speech::Allowed);
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn people_move_only_those_below_them_into_channels_they_can_see() {
    let dir = data_dir("move-members");
    let (addr, task) = start_in(PRIVATE, Some(dir.clone())).await;
    let (mut owner, _, _) = claim(addr, &dir, HOME).await;
    let S2C::Welcome { channels, .. } = TestClient::connect(addr, "admin", None).await.1 else { unreachable!() };
    let [lobby, office, afk] = ["Lobby", "Office", "AFK"].map(|name| id_of(&channels, name));
    let (mut moder, _) = TestClient::connect(addr, "mod", None).await;
    let (mut mover, _) = TestClient::connect(addr, "mover", None).await;
    let (_boss, wb) = TestClient::connect(addr, "boss", None).await;
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let (boss_id, user_id) = (welcomed_id(&wb), welcomed_id(&wu));
    owner.change_roles(assign("mod", RoleId::MODERATOR, true)).await;
    owner.change_roles(assign("boss", RoleId::ADMIN, true)).await;
    let roles = owner.change_roles(RoleChange::Create { name: "Mover".into() }).await;
    let movers = roles.ranked.last().unwrap().id;
    let perms = Perms::of(&[Perm::View, Perm::Join, Perm::MoveMembers]);
    owner.change_roles(RoleChange::SetPermissions { id: movers, perms }).await;
    owner.change_roles(assign("mover", movers, true)).await;
    moder.until(|m| matches!(m, S2C::Permissions(p) if p.sees(office))).await;

    moder.send(C2S::MoveMember { target: user_id, channel: afk }).await;
    user.until(|m| *m == S2C::ClientMoved { id: user_id, channel: afk }).await;

    moder.send(C2S::MoveMember { target: user_id, channel: office }).await;
    assert_eq!(error(&mut moder).await, "user can't see Office");
    moder.send(C2S::MoveMember { target: boss_id, channel: afk }).await;
    assert!(error(&mut moder).await.contains("can't move boss: their role isn't below yours"));
    user.send(C2S::MoveMember { target: boss_id, channel: afk }).await;
    assert_eq!(error(&mut user).await, "you can't move boss");

    let locked = ChannelSettings { password: PasswordChange::Set("pw".into()), ..settings("Lobby") };
    owner.send(C2S::EditChannel { id: lobby, settings: locked }).await;
    mover.until(|m| matches!(m, S2C::Channels(l) if l.iter().any(|c| c.id == lobby && c.has_password))).await;
    mover.send(C2S::MoveMember { target: user_id, channel: lobby }).await;
    assert_eq!(error(&mut mover).await, "Lobby has a password");
    moder.send(C2S::MoveMember { target: user_id, channel: lobby }).await;
    user.until(|m| *m == S2C::ClientMoved { id: user_id, channel: lobby }).await;

    let den = C2S::CreateChannel { parent: None, settings: settings("Den"), temporary: true, kind: ChannelKind::Voice };
    user.send(den).await;
    moder.until(|m| matches!(m, S2C::ClientMoved { id, channel } if *id == user_id && *channel != lobby)).await;
    moder.send(C2S::MoveMember { target: user_id, channel: afk }).await;
    user.until(|m| *m == S2C::ClientMoved { id: user_id, channel: afk }).await;
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}
