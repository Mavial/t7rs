//! Claiming the server, roles, and clients from before roles.

use t7_proto::perms::{self, Perm, Perms, RoleChange, RoleId};
use t7_proto::*;
use t7_server::config::DEFAULT_CONFIG;

use crate::harness::{
    HOME, TestClient, assign, claim, claim_token, data_dir, error, id_of, key, rejected, start, start_in, start_owned,
    welcomed_id,
};

fn may_in(welcome: &S2C, channel: ChannelId) -> Perms {
    let S2C::Welcome { permissions, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    permissions.channels.get(&channel).unwrap_or_else(|| panic!("no channel {channel}")).perms
}

#[tokio::test]
async fn the_first_to_claim_owns_the_server() {
    let dir = data_dir("claim");
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut alice, wa) = TestClient::connect(addr, "alice", None).await;
    let (mut bob, wb) = TestClient::connect(addr, "bob", None).await;
    let token = claim_token(&dir);
    let alice_id = welcomed_id(&wa);
    let S2C::Welcome { permissions, .. } = &wb else { unreachable!() };
    assert!(!permissions.server.has(Perm::ManageRoles));
    alice.until(|m| matches!(m, S2C::ClientJoined(c) if c.nickname == "bob")).await;

    bob.send(C2S::ClaimOwnership { token: "guess".into() }).await;
    assert!(error(&mut bob).await.contains("wrong"));

    alice.send(C2S::ClaimOwnership { token: token.clone() }).await;
    let S2C::Permissions(mine) = alice.until(|m| matches!(m, S2C::Permissions(_))).await else { unreachable!() };
    assert_eq!(mine.server, Perms::ALL);
    let S2C::Roles(roles) = alice.until(|m| matches!(m, S2C::Roles(_))).await else { unreachable!() };
    assert_eq!(roles.owner, Some(key("alice").identity()));
    assert_eq!(roles.members[&key("alice").identity()].nickname, "alice");
    let owner = S2C::ClientRole { id: alice_id, role: Some("Owner".into()) };
    assert_eq!(bob.until(|m| matches!(m, S2C::ClientRole { .. })).await, owner);

    bob.send(C2S::ClaimOwnership { token: token.clone() }).await;
    assert!(error(&mut bob).await.contains("already used"));
    let saved = std::fs::read_to_string(dir.join("roles.toml")).unwrap();
    assert!(!saved.contains("claim_token"), "{saved}");
    task.abort();
    drop((alice, bob));

    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (_, welcome) = TestClient::connect(addr, "alice", None).await;
    let S2C::Welcome { your_id, clients, permissions, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    assert_eq!(clients.iter().find(|c| c.id == your_id).unwrap().role.as_deref(), Some("Owner"));
    assert_eq!(permissions.server, Perms::ALL);
    let (mut bob, _) = TestClient::connect(addr, "bob", None).await;
    bob.send(C2S::ClaimOwnership { token }).await;
    assert!(error(&mut bob).await.contains("already used"));
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn clients_from_before_roles_are_told_to_update() {
    let addr = start(DEFAULT_CONFIG).await;
    let (_, reply) = TestClient::connect_as(addr, "old", None, 5).await;
    assert!(rejected(&reply).contains("please update your T7"), "{reply:?}");
}

#[tokio::test]
async fn installs_from_before_roles_keep_their_moderators() {
    let dir = data_dir("migrate");
    std::fs::write(dir.join("admin_token.txt"), "old-token").unwrap();
    let moderator = key("mod").identity().to_hex();
    let channels = format!(
        "[[channels]]\nname = \"Lobby\"\ndefault = true\n\n[[channels]]\nname = \"Gaming\"\nmoderators = \
         [\"{moderator}\"]\n\n  [[channels.children]]\n  name = \"Squad\"\n\n[[channels]]\nname = \"AFK\"\n"
    );
    std::fs::write(dir.join("channels.toml"), channels).unwrap();
    let cfg = format!("admin_token = \"old-token\"\n{DEFAULT_CONFIG}");
    let (addr, task) = start_in(&cfg, Some(dir.clone())).await;
    let (mut moder, wm) = TestClient::connect(addr, "mod", None).await;
    assert!(!dir.join("admin_token.txt").exists(), "the admin token is gone");
    assert!(!claim_token(&dir).is_empty(), "a claim token replaces it");

    let S2C::Welcome { channels, .. } = &wm else { unreachable!() };
    let [gaming, squad, afk] = ["Gaming", "Squad", "AFK"].map(|name| id_of(channels, name));
    assert_eq!(channels.iter().find(|c| c.id == gaming).unwrap().moderators, [key("mod").identity()]);
    for (channel, kicks) in [(gaming, true), (squad, true), (afk, false)] {
        assert_eq!(may_in(&wm, channel).has(Perm::Kick), kicks, "{channel}");
    }
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let user_id = welcomed_id(&wu);
    user.send(C2S::JoinChannel { channel: squad, password: None }).await;
    moder.until(|m| *m == S2C::ClientMoved { id: user_id, channel: squad }).await;
    moder.send(C2S::Kick { target: user_id, from_server: false, reason: String::new() }).await;
    assert!(
        matches!(user.until(|m| matches!(m, S2C::ClientKicked { .. })).await, S2C::ClientKicked { id, .. } if id == user_id)
    );

    moder.send(C2S::ClaimOwnership { token: "old-token".into() }).await;
    assert!(error(&mut moder).await.contains("wrong"));
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn people_act_only_on_those_below_their_role() {
    let (addr, mut owner, owner_id, channels) = start_owned("hierarchy", HOME).await;
    let hangout = id_of(&channels, "Hangout");
    let (mut admin, wa) = TestClient::connect(addr, "boss", None).await;
    let (mut moder, wm) = TestClient::connect(addr, "mod", None).await;
    let (mut user, wu) = TestClient::connect(addr, "user", None).await;
    let (admin_id, mod_id, user_id) = (welcomed_id(&wa), welcomed_id(&wm), welcomed_id(&wu));
    owner.change_roles(assign("boss", RoleId::ADMIN, true)).await;
    let roles = owner.change_roles(assign("mod", RoleId::MODERATOR, true)).await;
    assert_eq!(roles.members[&key("mod").identity()].roles, [RoleId::MODERATOR]);
    let retitled = S2C::ClientRole { id: mod_id, role: Some("Moderator".into()) };
    assert_eq!(user.until(|m| matches!(m, S2C::ClientRole { id, .. } if *id == mod_id)).await, retitled);
    let S2C::Permissions(mine) = moder.until(|m| matches!(m, S2C::Permissions(_))).await else { unreachable!() };
    assert!(mine.server.has(Perm::Kick) && !mine.server.has(Perm::Ban));
    for c in [&mut admin, &mut user] {
        c.send(C2S::JoinChannel { channel: hangout, password: None }).await;
    }
    moder.until(|m| *m == S2C::ClientMoved { id: admin_id, channel: hangout }).await;
    moder.until(|m| *m == S2C::ClientMoved { id: user_id, channel: hangout }).await;

    let kick = |target| C2S::Kick { target, from_server: false, reason: String::new() };
    moder.send(kick(admin_id)).await;
    assert!(error(&mut moder).await.contains("their role isn't below yours"));
    moder.send(C2S::Kick { target: owner_id, from_server: true, reason: String::new() }).await;
    assert!(error(&mut moder).await.contains("their role isn't below yours"));
    moder.send(C2S::Ban { target: user_id, duration_secs: None, reason: String::new(), include_ip: false }).await;
    assert!(error(&mut moder).await.contains("can't ban"));
    moder.send(kick(user_id)).await;
    user.until(|m| matches!(m, S2C::ClientKicked { id, .. } if *id == user_id)).await;
    moder.send(C2S::JoinChannel { channel: hangout, password: None }).await;
    admin.until(|m| *m == S2C::ClientMoved { id: mod_id, channel: hangout }).await;
    admin.send(kick(mod_id)).await;
    moder.until(|m| matches!(m, S2C::ClientKicked { id, .. } if *id == mod_id)).await;

    let refused = [
        assign("user", RoleId::ADMIN, true),
        assign("boss", RoleId::ADMIN, false),
        RoleChange::Rename { id: RoleId::ADMIN, name: "Boss".into() },
        RoleChange::Move { id: RoleId::MODERATOR, position: 0 },
    ];
    for change in refused {
        admin.send(C2S::ChangeRoles(change.clone())).await;
        assert!(error(&mut admin).await.contains("below your own"), "{change:?}");
    }
    let roles = admin.change_roles(RoleChange::Create { name: "DJ".into() }).await;
    let dj = roles.ranked.last().unwrap().id;
    let roles = admin.change_roles(assign("user", dj, true)).await;
    assert_eq!(roles.members[&key("user").identity()].roles, [dj]);
    moder.send(C2S::ChangeRoles(assign("user", dj, false))).await;
    assert!(error(&mut moder).await.contains("can't manage roles"));
    let (mut nobody, _) = TestClient::connect(addr, "nobody", None).await;
    nobody.send(C2S::SetModerator { channel: hangout, identity: key("boss").identity(), moderator: true }).await;
    assert!(error(&mut nobody).await.contains("can't choose who moderates"));
}

#[tokio::test]
async fn roles_and_who_holds_them_survive_a_restart() {
    let dir = data_dir("roles");
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut owner, _, _) = claim(addr, &dir, HOME).await;
    let (bob, _) = TestClient::connect(addr, "bob", None).await;
    let roles = owner.change_roles(RoleChange::Create { name: "DJ".into() }).await;
    let dj = roles.ranked.last().unwrap().id;
    let perms = Perms::of(&[Perm::View, Perm::Join, Perm::MuteMembers]);
    owner.change_roles(RoleChange::SetPermissions { id: dj, perms }).await;
    owner.change_roles(RoleChange::Move { id: dj, position: 0 }).await;
    owner.change_roles(assign("bob", RoleId::MODERATOR, true)).await;
    let roles = owner.change_roles(assign("bob", dj, true)).await;
    assert_eq!(roles.members[&key("bob").identity()].roles, [RoleId::MODERATOR, dj]);
    task.abort();
    drop((owner, bob));

    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut owner, _) = TestClient::connect(addr, "admin", None).await;
    let S2C::Roles(roles) = owner.until(|m| matches!(m, S2C::Roles(_))).await else { unreachable!() };
    assert_eq!(roles.ranked[0], perms::Role { id: dj, name: "DJ".into(), perms });
    let (bob, welcome) = TestClient::connect_from(addr, HOME, "Bobby", &key("bob")).await;
    let S2C::Welcome { your_id, clients, permissions, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    assert_eq!(clients.iter().find(|c| c.id == your_id).unwrap().role.as_deref(), Some("DJ"));
    assert!(permissions.server.has(Perm::MuteMembers) && permissions.server.has(Perm::Kick));
    let S2C::Roles(roles) = owner.until(|m| matches!(m, S2C::Roles(_))).await else { unreachable!() };
    assert_eq!(roles.members[&key("bob").identity()].nickname, "Bobby");

    drop(bob);
    owner.until(|m| matches!(m, S2C::ClientLeft { .. })).await;
    owner.change_roles(assign("bob", dj, false)).await;
    owner.send(C2S::ChangeRoles(assign("bob", dj, true))).await;
    assert!(error(&mut owner).await.contains("online"), "granting needs them online");
    task.abort();
    drop(owner);

    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut owner, _) = TestClient::connect(addr, "admin", None).await;
    let (_, welcome) = TestClient::connect(addr, "bob", None).await;
    let S2C::Welcome { your_id, clients, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    assert_eq!(clients.iter().find(|c| c.id == your_id).unwrap().role.as_deref(), Some("Moderator"));
    owner.change_roles(assign("bob", RoleId::MODERATOR, false)).await;
    let roles = owner.change_roles(RoleChange::Delete { id: dj }).await;
    assert!(roles.role(dj).is_none() && !roles.members.contains_key(&key("bob").identity()));
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_owner_reset_hands_the_server_to_the_next_claimer() {
    let dir = data_dir("owner-reset");
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (owner, _, _) = claim(addr, &dir, HOME).await;
    task.abort();
    drop(owner);

    let token = t7_server::reset_owner_claim(&dir).unwrap();
    assert_eq!(claim_token(&dir), token);
    let (addr, task) = start_in(DEFAULT_CONFIG, Some(dir.clone())).await;
    let (mut old, welcome) = TestClient::connect(addr, "admin", None).await;
    let S2C::Welcome { permissions, .. } = welcome else { panic!("not welcomed: {welcome:?}") };
    assert_eq!(permissions.server, Perms::ALL, "the owner stays until the token is redeemed");
    let (mut new, welcome) = TestClient::connect(addr, "newcomer", None).await;
    new.send(C2S::ClaimOwnership { token }).await;
    let S2C::Permissions(was) = old.until(|m| matches!(m, S2C::Permissions(_))).await else { unreachable!() };
    assert!(!was.server.has(Perm::ManageRoles), "the old owner is no longer one");
    let new_id = welcomed_id(&welcome);
    let owner = S2C::ClientRole { id: new_id, role: Some("Owner".into()) };
    assert_eq!(old.until(|m| matches!(m, S2C::ClientRole { id, .. } if *id == new_id)).await, owner);
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}
