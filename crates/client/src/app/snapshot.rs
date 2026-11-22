//! Renders every screen and dialog to PNG with the real CPU renderer, for
//! reviewing the UI without a server or clicking through it.
//!
//! `cargo test -p t7-client snapshots -- --ignored` writes to
//! `target/snapshots`, or to `$T7_SNAPSHOT_DIR` when set.

use std::path::PathBuf;

use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender};
use t7_proto::perms::{Member, Override, Perm, Permissions, Perms, RoleId, Roles};
use t7_proto::{ChannelInfo, ChannelKind, ClientInfo, ClientStatus, Identity, ServerInfo, Speech};

use super::*;

fn channel(id: u32, parent: Option<u32>, name: &str, order: u32) -> ChannelInfo {
    ChannelInfo {
        id,
        parent,
        name: name.into(),
        topic: String::new(),
        max_clients: None,
        order,
        temporary: false,
        has_password: false,
        default: false,
        owner: None,
        moderators: Vec::new(),
        kind: ChannelKind::Voice,
        overrides: Vec::new(),
    }
}

fn client(id: u32, name: &str, channel: u32, status: ClientStatus) -> ClientInfo {
    let identity = Identity([id as u8; 32]);
    ClientInfo { id, nickname: name.into(), channel, status, role: None, identity, speech: Speech::Allowed }
}

fn view() -> ServerView {
    let mut lobby = channel(1, None, "Lobby", 0);
    lobby.default = true;
    lobby.topic = "Hang out here".into();
    let mut games = channel(2, None, "Games", 1);
    games.moderators.push(Identity([3; 32]));
    let mut ranked = channel(3, Some(2), "Ranked", 0);
    ranked.max_clients = Some(5);
    let mut duo = channel(4, Some(3), "Duo queue", 0);
    duo.has_password = true;
    let mut afk = channel(5, None, "AFK", 3);
    afk.temporary = true;
    afk.owner = Some(7);
    let music = channel(6, None, "Music", 4);
    // Only moderators see it.
    let mut staff = channel(7, None, "Staff", 5);
    staff.kind = ChannelKind::Category;
    let view_only = Perms::of(&[Perm::View]);
    staff.overrides = vec![
        Override::new(RoleId::EVERYONE, Perms::NONE, view_only),
        Override::new(RoleId::MODERATOR, view_only, Perms::NONE),
    ];
    let office = channel(8, Some(7), "Office", 0);
    let meeting = channel(9, Some(7), "Meeting room", 1);
    let mut rooms = channel(10, None, "Create a room", 2);
    rooms.kind = ChannelKind::Lobby;
    rooms.topic = "Join to open a room of your own".into();
    let mut room = channel(11, Some(10), "Dmitri's room", 0);
    (room.temporary, room.owner) = (true, Some(6));
    let muted = ClientStatus { muted: true, ..Default::default() };
    let deaf = ClientStatus { muted: true, deafened: true, ..Default::default() };
    let away = ClientStatus { away: true, ..Default::default() };
    let mut me = client(1, "Santi", 2, ClientStatus::default());
    me.role = Some("Owner".into());
    let mut marta = client(2, "Marta", 2, ClientStatus::default());
    marta.role = Some("Admin".into());
    let mut kenji = client(3, "Kenji", 2, muted);
    kenji.role = Some("Moderator".into());
    let clients = vec![
        me,
        marta,
        kenji,
        client(4, "Ola", 3, deaf),
        client(5, "Priya", 3, ClientStatus::default()),
        client(6, "Dmitri", 11, ClientStatus::default()),
        client(10, "Mei", 11, ClientStatus::default()),
        client(7, "Lucas", 5, away),
        client(8, "Ines", 1, ClientStatus::default()),
        ClientInfo { role: Some("Moderator".into()), ..client(9, "Jonas", 8, ClientStatus::default()) },
    ];
    let info = ServerInfo { name: "Friday Night".into(), welcome: String::new(), max_clients: 32 };
    let channels = vec![lobby, games, ranked, duo, afk, music, staff, office, meeting, rooms, room];
    let everywhere = channels.iter().map(|c| (c.id, Perms::ALL | Perms::of(&[Perm::Rename]))).collect();
    let mut view =
        ServerView::new(1, info, channels, clients, Permissions { server: Perms::ALL, channels: everywhere });
    let mut roles = Roles { owner: Some(Identity([1; 32])), ..Roles::default() };
    for (id, nickname, held) in [
        (1, "Santi", &[][..]),
        (2, "Marta", &[RoleId::ADMIN][..]),
        (3, "Kenji", &[RoleId::MODERATOR][..]),
        (9, "Jonas", &[RoleId::MODERATOR][..]),
    ] {
        roles.members.insert(Identity([id; 32]), Member { nickname: nickname.into(), roles: held.to_vec() });
    }
    view.roles = Some(roles);
    view
}

fn line(time: &str, kind: LineKind, text: &str) -> Line {
    Line { time: time.into(), kind, text: text.into() }
}

fn msg(from: u32, name: &str) -> LineKind {
    LineKind::Msg { from, name: name.into() }
}

fn recents() -> Vec<Recent> {
    vec![
        Recent { address: "friday.example.com".into(), server_name: "Friday Night".into() },
        Recent { address: "10.0.0.12:9988".into(), server_name: "LAN party".into() },
    ]
}

fn connected(ctx: &egui::Context) -> App {
    let mut settings = Settings { nickname: "Santi".into(), recent: recents(), ..Default::default() };
    settings.user_volumes.insert("Marta".into(), 0.6);
    let mut app = App::new(ctx, settings, None);
    app.view = Some(view());
    app.conn = Conn::Connected { server: "friday.example.com:9988".into() };
    app.ping = Some(Duration::from_millis(23));
    app.tabs[1].title = "Games".into();
    app.active_tab = 1;
    app.tabs[0].unread = 3;
    app.tabs.push(Tab { kind: TabKind::Private(2), title: "Marta".into(), lines: Vec::new(), unread: 1 });
    app.tabs[1].lines = vec![
        line("20:14", LineKind::System, "You joined Games"),
        line("20:14", msg(2, "Marta"), "hey! ready for ranked?"),
        line("20:15", msg(1, "Santi"), "two minutes, grabbing a drink"),
        line("20:15", msg(3, "Kenji"), "my mic is acting up again, I'll type for now"),
        line("20:16", LineKind::System, "Ola joined Ranked"),
        line("20:17", LineKind::Error, "Couldn't open the microphone: device in use"),
        line(
            "20:18",
            msg(2, "Marta"),
            "long message to check wrapping: we should probably set up a channel for the tournament next \
             week and get everyone's availability sorted before Thursday",
        ),
    ];
    app
}

fn disconnected(ctx: &egui::Context) -> App {
    let settings = Settings { nickname: "Santi".into(), recent: recents(), ..Default::default() };
    App::new(ctx, settings, None)
}

type Scene = (&'static str, (u32, u32), fn(&egui::Context) -> App);

const SCENES: &[Scene] = &[
    ("01-empty", (1100, 720), disconnected),
    ("02-connected", (1100, 720), connected),
    ("03-connected-small", (760, 480), connected),
    ("04-connect-dialog", (1100, 720), |ctx| {
        let mut app = disconnected(ctx);
        app.open_connect_form();
        app.last_error = Some("Couldn't reach friday.example.com: connection refused".into());
        app
    }),
    ("05-voice-settings", (1100, 820), |ctx| {
        let mut app = connected(ctx);
        app.audio_dialog = Some(AudioDialog {
            packs: vec![("Custom folder".into(), "custom".into())],
            inputs: vec!["Microphone (USB)".into()],
            outputs: vec!["Headphones".into()],
            binding: false,
        });
        app
    }),
    ("06-edit-channel", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_edit_channel(2);
        app
    }),
    ("07-create-channel", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_create_channel(Some(2));
        app
    }),
    ("08-kick", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_kick(3, false);
        app
    }),
    ("09-ban", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_ban(3);
        app
    }),
    ("10-bans", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_server_settings();
        let settings = app.server_settings.as_mut().unwrap();
        settings.tab = SettingsTab::Bans;
        settings.bans = Some(vec![BanInfo {
            id: 1,
            nickname: "Troll".into(),
            identity: Identity([9; 32]),
            ip: None,
            reason: "Spamming the lobby".into(),
            banned_by: "Santi".into(),
            created_unix: 1_763_000_000,
            expires_unix: Some(1_763_600_000),
        }]);
        app
    }),
    ("11-poke", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_poke(2);
        app
    }),
    ("12-poked", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.pokes.push_back(PokeAlert {
            from: "Marta".into(),
            message: "we're starting!".into(),
            time: "20:21".into(),
        });
        app
    }),
    ("13-delete-channel", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_delete_channel(3);
        app
    }),
    ("14-cert-change", (1100, 720), |ctx| {
        let mut app = disconnected(ctx);
        app.cert_change = Some(CertChange {
            address: "friday.example.com".into(),
            password: String::new(),
            pinned: Fingerprint([0xab; 32]),
            presented: Fingerprint([0x3c; 32]),
        });
        app
    }),
    ("15-claim-ownership", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.channel_dialog = Some(ChannelDialog::ClaimOwnership { token: String::new(), focused: false });
        app
    }),
    ("17-roles", (1100, 760), |ctx| {
        let mut app = connected(ctx);
        app.open_server_settings();
        app
    }),
    ("18-roles-everyone", (1100, 760), |ctx| {
        let mut app = connected(ctx);
        app.open_server_settings();
        app.server_settings.as_mut().unwrap().picked = Pick::Everyone;
        app
    }),
    ("20-channel-permissions", (1100, 760), |ctx| {
        let mut app = connected(ctx);
        app.open_edit_channel(8);
        let Some(ChannelDialog::Form(form)) = &mut app.channel_dialog else { unreachable!() };
        form.tab = FormTab::Permissions;
        app
    }),
    ("21-category-permissions", (1100, 760), |ctx| {
        let mut app = connected(ctx);
        app.open_edit_channel(7);
        let Some(ChannelDialog::Form(form)) = &mut app.channel_dialog else { unreachable!() };
        form.tab = FormTab::Permissions;
        app
    }),
    ("22-create-category", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_create_channel(None);
        let Some(ChannelDialog::Form(form)) = &mut app.channel_dialog else { unreachable!() };
        form.kind = ChannelKind::Category;
        app
    }),
    ("23-folded-category", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.settings.folded.insert("friday.example.com:9988".into(), ["Staff".to_owned()].into());
        app
    }),
    ("25-create-lobby", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_create_channel(Some(2));
        let Some(ChannelDialog::Form(form)) = &mut app.channel_dialog else { unreachable!() };
        form.kind = ChannelKind::Lobby;
        form.name = "Squads".into();
        app
    }),
    ("26-cant-speak", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        for id in [1, 5] {
            app.apply(&S2C::ClientSpeech { id, speech: Speech::NotHere });
        }
        app
    }),
    ("27-voice-settings-cant-speak", (1100, 820), |ctx| {
        let mut app = connected(ctx);
        app.apply(&S2C::ClientSpeech { id: 1, speech: Speech::NotHere });
        app.audio_dialog = Some(AudioDialog {
            packs: Vec::new(),
            inputs: vec!["Microphone (USB)".into()],
            outputs: vec!["Headphones".into()],
            binding: false,
        });
        app
    }),
    ("28-server-muted", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        for (id, speech) in [(1, Speech::ServerMuted), (5, Speech::ServerMuted), (8, Speech::NotHere)] {
            app.apply(&S2C::ClientSpeech { id, speech });
        }
        app
    }),
    ("24-moderator-edit", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        let view = app.view.as_mut().unwrap();
        let moderator = Perms::of(&[Perm::View, Perm::Join, Perm::Speak, Perm::Kick, Perm::Rename]);
        let channels = view.channels.keys().map(|&c| (c, moderator)).collect();
        view.apply(&S2C::Permissions(Permissions { server: Perms::of(&[Perm::View]), channels }));
        app.open_edit_channel(3);
        app
    }),
    ("16-connecting", (1100, 720), |ctx| {
        let mut app = disconnected(ctx);
        app.conn = Conn::Connecting { address: "friday.example.com".into(), password: String::new() };
        app
    }),
];

/// Where the pointer goes in a frame, and whether the right button goes down
/// (`Some(true)`) or up there.
type Step = (f32, f32, Option<bool>);

/// A scene that moves the pointer first, one step a frame, to open a menu.
type Menu = (&'static str, (u32, u32), fn(&egui::Context) -> App, &'static [Step]);

const MENUS: &[Menu] = &[
    (
        "19-user-roles",
        (1100, 720),
        connected,
        &[(80.0, 226.0, Some(true)), (80.0, 226.0, Some(false)), (150.0, 330.0, None)],
    ),
    (
        "29-user-move-to",
        (1100, 720),
        connected,
        &[(80.0, 340.0, Some(true)), (80.0, 340.0, Some(false)), (150.0, 240.0, None)],
    ),
];

fn render(
    name: &str,
    (w, h): (u32, u32),
    make: fn(&egui::Context) -> App,
    steps: &[Step],
    scale: f32,
    dir: &std::path::Path,
) {
    let ctx = egui::Context::default();
    let mut app = make(&ctx);
    let mut renderer = EguiSoftwareRender::new(ColorFieldOrder::Rgba).with_caching(false);
    let mut px = vec![[0u8; 4]; (w * h) as usize];
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(w as f32 / scale, h as f32 / scale));
    // Enough frames for layout to settle and fades to finish.
    // Steps start once there is a layout to point at, and frames come quickly
    // enough for a press and release to make a click.
    for frame in 0..6 + steps.len() {
        let time = Some(frame as f64 / 4.0);
        let mut raw = egui::RawInput { screen_rect: Some(screen), time, ..Default::default() };
        if let Some(&(x, y, button)) = frame.checked_sub(2).and_then(|step| steps.get(step)) {
            let pos = egui::pos2(x, y);
            raw.events.push(egui::Event::PointerMoved(pos));
            if let Some(pressed) = button {
                let (button, modifiers) = (egui::PointerButton::Secondary, egui::Modifiers::default());
                raw.events.push(egui::Event::PointerButton { pos, button, pressed, modifiers });
            }
        }
        raw.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(scale);
        let out = ctx.run_ui(raw, |ui| app.ui(ui));
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        let mut target = BufferMutRef::new(&mut px, w as usize, h as usize);
        let mut delta = out.textures_delta;
        renderer.render(&mut target, &prims, &delta, out.pixels_per_point);
        delta.clear();
    }
    let file = std::fs::File::create(dir.join(format!("{name}.png"))).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    let mut writer = enc.write_header().expect("png header");
    let mut bytes: Vec<u8> = px.iter().flatten().copied().collect();
    bytes.chunks_mut(4).for_each(|p| p[3] = 255);
    writer.write_image_data(&bytes).expect("png data");
}

#[test]
#[ignore = "writes PNGs for visual review"]
fn snapshots() {
    let dir = std::env::var_os("T7_SNAPSHOT_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/snapshots"), PathBuf::from);
    std::fs::create_dir_all(&dir).expect("snapshot dir");
    let only = std::env::var("T7_SNAPSHOT").ok();
    let scenes = SCENES.iter().map(|&(name, size, make)| (name, size, make, &[][..]));
    for (name, size, make, steps) in scenes.chain(MENUS.iter().copied()) {
        if only.as_deref().is_none_or(|o| name.contains(o)) {
            render(name, size, make, steps, 1.0, &dir);
        }
    }
}
