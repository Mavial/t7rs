//! Managing channels with permissions, and what survives a restart.

use std::net::SocketAddr;

use t7_proto::perms::{Perm, RoleId};
use t7_proto::*;
use t7_server::config::{Config, DEFAULT_CONFIG};
use tokio::net::TcpStream;

use crate::harness::{
    HOME, PRIVATE, TestClient, assign, claim, data_dir, error, id_of, new_channel, set, settings, start_in,
    start_owned, welcomed_id,
};

/// The certificate fingerprint a server presents.
async fn fingerprint(addr: SocketAddr) -> tls::Fingerprint {
    tls::connect(TcpStream::connect(addr).await.unwrap(), None).await.unwrap().1
}

#[tokio::test]
async fn channels_are_managed_with_permissions() {
    let dir = data_dir("manage");
    let (addr, _) = start_in(DEFAULT_CONFIG, Some(dir.to_path_buf())).await;
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
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.to_path_buf())).await;
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

    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.to_path_buf())).await;
    assert_eq!(fingerprint(addr).await, first, "a restart must keep the certificate");
    let (_, welcome) = TestClient::connect(addr, "later", None).await;
    let S2C::Welcome { channels, .. } = welcome else { panic!("not welcomed") };
    assert!(channels.iter().any(|c| c.name == "Saved"), "channel lost on restart");
    assert_eq!(channels.len(), Channels::count(&cfg) + 1);
    task.abort();
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

fn names(list: &[ChannelInfo]) -> Vec<&str> {
    let mut names: Vec<&str> = list.iter().map(|c| c.name.as_str()).collect();
    names.sort_unstable();
    names
}

#[tokio::test]
async fn a_channel_hidden_from_everyone_hides_its_people_but_not_from_moderators() {
    let dir = data_dir("hidden");
    let (addr, task) = start_in(PRIVATE, Some(dir.to_path_buf())).await;
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
}

#[tokio::test]
async fn a_move_counts_only_the_channels_the_mover_sees() {
    let (addr, mut owner, _, _) = start_owned("hidden-sibling", HOME).await;
    owner.send(new_channel("Vault", None, ChannelKind::Voice)).await;
    let vault = id_of(&owner.channels().await, "Vault");
    owner.send(C2S::MoveChannel { id: vault, parent: None, position: 0 }).await;
    owner.send(set(vault, RoleId::ADMIN, &[], &[Perm::View, Perm::ManageChannels])).await;
    owner.sync().await;
    let (mut boss, _) = TestClient::connect(addr, "boss", None).await;
    owner.change_roles(assign("boss", RoleId::ADMIN, true)).await;
    let list = boss.channels().await;
    assert!(list.iter().all(|c| c.id != vault), "{list:?}");
    let top = |list: &[ChannelInfo]| {
        let mut top: Vec<&ChannelInfo> = list.iter().filter(|c| c.parent.is_none()).collect();
        top.sort_by_key(|c| c.order);
        top.into_iter().map(|c| c.name.clone()).take(3).collect::<Vec<_>>()
    };
    assert_eq!(top(&list), ["Lobby", "Hangout", "Gaming"]);

    boss.send(C2S::MoveChannel { id: id_of(&list, "Gaming"), parent: None, position: 1 }).await;
    assert_eq!(top(&boss.channels().await), ["Lobby", "Gaming", "Hangout"], "one place up");
}

#[tokio::test]
async fn people_who_lose_their_channel_land_in_an_open_one() {
    let (addr, mut owner, _, channels) = start_owned("landing", HOME).await;
    let [lobby, hangout, gaming] = ["Lobby", "Hangout", "Gaming"].map(|name| id_of(&channels, name));
    let one = ChannelSettings { max_clients: Some(1), ..settings("Lobby") };
    owner.send(C2S::EditChannel { id: lobby, settings: one }).await;
    owner.channels().await;
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let user_id = welcomed_id(&wu);
    let S2C::Welcome { clients, .. } = &wu else { unreachable!() };
    assert_eq!(clients.iter().find(|c| c.id == user_id).unwrap().channel, hangout, "the owner fills the Lobby");

    owner.send(set(hangout, RoleId::EVERYONE, &[], &[Perm::View])).await;
    let moved = user.until(|m| matches!(m, S2C::ClientMoved { id, .. } if *id == user_id)).await;
    assert_eq!(moved, S2C::ClientMoved { id: user_id, channel: gaming }, "hidden from them");

    owner.send(C2S::DeleteChannel { id: gaming }).await;
    let S2C::ClientMoved { channel, .. } =
        user.until(|m| matches!(m, S2C::ClientMoved { id, .. } if *id == user_id)).await
    else {
        unreachable!()
    };
    let S2C::Welcome { channels, .. } = TestClient::connect(addr, "cid", None).await.1 else { unreachable!() };
    assert_eq!(channels.iter().find(|c| c.id == channel).unwrap().name, "AFK", "deleted under them");
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
