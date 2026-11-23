//! Channel overrides: inheritance, who may set them, and listed moderators.

use t7_proto::perms::{self, Perm, Perms, RoleChange, RoleId};
use t7_proto::*;
use t7_server::config::DEFAULT_CONFIG;

use crate::harness::{
    HOME, TestClient, assign, claim, data_dir, error, id_of, key, new_channel, overrides_in, set, settings, start_in,
    start_owned, welcomed_id,
};

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
async fn deleting_a_role_drops_its_overrides_on_temporary_channels_from_editors_views() {
    let (_, mut owner, _, _) = start_owned("forget-room-override", HOME).await;
    let roles = owner.change_roles(RoleChange::Create { name: "DJ".into() }).await;
    let dj = roles.ranked.last().unwrap().id;
    owner
        .send(C2S::CreateChannel {
            parent: None,
            settings: settings("Raid"),
            temporary: true,
            kind: ChannelKind::Voice,
        })
        .await;
    let raid = id_of(&owner.channels().await, "Raid");
    owner.send(set(raid, dj, &[Perm::Speak], &[])).await;
    assert_eq!(owner.overrides(raid).await, [perms::Override::allowing(dj, Perms::of(&[Perm::Speak]))]);

    owner.sync().await;
    owner.send(C2S::ChangeRoles(RoleChange::Delete { id: dj })).await;
    let arrived = owner.sync().await;
    let latest = arrived.iter().rev().find_map(|m| match m {
        S2C::Channels(list) => Some(list),
        _ => None,
    });
    assert_eq!(overrides_in(latest.expect("a fresh channel list"), raid), []);
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

#[tokio::test]
async fn listed_moderators_rename_their_channels_and_do_nothing_more() {
    let (addr, mut owner, _, channels) = start_owned("moderator-rename", HOME).await;
    let [gaming, squad, hangout] = ["Gaming", "Squad 1", "Hangout"].map(|name| id_of(&channels, name));
    let (mut listed, _) = TestClient::connect(addr, "listed", None).await;
    let (mut holder, _) = TestClient::connect(addr, "holder", None).await;
    owner.send(C2S::SetModerator { channel: gaming, identity: key("listed").identity(), moderator: true }).await;
    owner.channels().await;
    owner.change_roles(assign("holder", RoleId::MODERATOR, true)).await;

    let games = ChannelSettings { topic: "Squad up, again".into(), ..settings("Games") };
    listed.send(C2S::EditChannel { id: gaming, settings: games }).await;
    let renamed = owner.until(|m| matches!(m, S2C::Channels(l) if l.iter().any(|c| c.name == "Games"))).await;
    let S2C::Channels(list) = renamed else { unreachable!() };
    assert_eq!(list.iter().find(|c| c.id == gaming).unwrap().topic, "Squad up, again");
    let squad_a = ChannelSettings { max_clients: Some(5), ..settings("Squad A") };
    listed.send(C2S::EditChannel { id: squad, settings: squad_a }).await;
    owner.until(|m| matches!(m, S2C::Channels(l) if l.iter().any(|c| c.name == "Squad A"))).await;

    let limited = ChannelSettings { max_clients: Some(3), ..settings("Games") };
    listed.send(C2S::EditChannel { id: gaming, settings: limited }).await;
    assert!(error(&mut listed).await.contains("only change this channel's name and topic"));
    let locked = ChannelSettings { password: PasswordChange::Set("pw".into()), ..settings("Games") };
    listed.send(C2S::EditChannel { id: gaming, settings: locked }).await;
    assert!(error(&mut listed).await.contains("only change this channel's name and topic"));
    listed.send(C2S::DeleteChannel { id: squad }).await;
    assert!(error(&mut listed).await.contains("can't delete"));
    listed.send(C2S::MoveChannel { id: squad, parent: None, position: 0 }).await;
    assert!(error(&mut listed).await.contains("can't move this channel"));
    listed.send(set(gaming, RoleId::EVERYONE, &[], &[Perm::Speak])).await;
    assert!(error(&mut listed).await.contains("can't change this channel's permissions"));
    listed.send(new_channel("Ranked", Some(gaming), ChannelKind::Voice)).await;
    assert!(error(&mut listed).await.contains("can't create permanent channels"));
    listed.send(C2S::EditChannel { id: hangout, settings: settings("Mine") }).await;
    assert_eq!(error(&mut listed).await, "you can't edit this channel", "elsewhere");

    holder.send(C2S::EditChannel { id: gaming, settings: settings("Holder's") }).await;
    assert_eq!(error(&mut holder).await, "you can't edit this channel");
}
