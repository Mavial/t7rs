//! Renders every screen and dialog to PNG with the real CPU renderer, for
//! reviewing the UI without a server or clicking through it.
//!
//! `cargo test -p t7-client snapshots -- --ignored` writes to
//! `target/snapshots`, or to `$T7_SNAPSHOT_DIR` when set.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender};
use t7_proto::perms::{ChannelPerms, Member, Override, Perm, Permissions, Perms, RoleId, Roles, Setting};
use t7_proto::{ChannelInfo, ChannelKind, ClientInfo, ClientStatus, Identity, ServerInfo, Speech};

use egui::{Color32, ColorImage, TextureOptions, vec2};
use t7_proto::tls::Fingerprint;
use t7_proto::{BanInfo, S2C};

use super::channel_dialog::FormTab;
use super::chat::{Line, LineKind, Speaker, TabKind};
use super::server_settings::{Pick, SettingsTab};
use super::sharing::Watching;
use super::shelf::Card;
use super::voice::SettingsPage;
use super::*;
use crate::settings::Recent;

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
    ClientInfo {
        id,
        nickname: name.into(),
        channel,
        status,
        role: None,
        identity,
        speech: Speech::Allowed,
        sharing: false,
    }
}

fn view() -> ServerView {
    let mut lobby = channel(1, None, "Lobby", 0);
    lobby.default = true;
    lobby.topic = "Hang out here".into();
    let mut games = channel(2, None, "Games", 1);
    games.topic = "Ranked tonight at 9".into();
    games.moderators.push(Identity([3; 32]));
    let mut ranked = channel(3, Some(2), "Ranked", 0);
    ranked.max_clients = Some(5);
    let mut duo = channel(4, Some(3), "Duo queue", 0);
    duo.has_password = true;
    let mut afk = channel(5, None, "AFK", 3);
    afk.temporary = true;
    afk.owner = Some(7);
    let music = channel(6, None, "Music", 4);
    let mut staff = channel(7, None, "Staff", 5);
    staff.kind = ChannelKind::Category;
    let view_only = Perms::of(&[Perm::View]);
    staff.overrides =
        vec![Override::denying(RoleId::EVERYONE, view_only), Override::allowing(RoleId::MODERATOR, view_only)];
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
    marta.sharing = true;
    let mut kenji = client(3, "Kenji", 2, muted);
    kenji.role = Some("Moderator".into());
    let clients = vec![
        me,
        marta,
        kenji,
        ClientInfo { sharing: true, ..client(4, "Ola", 3, deaf) },
        client(5, "Priya", 3, ClientStatus::default()),
        client(6, "Dmitri", 11, ClientStatus::default()),
        client(10, "Mei", 11, ClientStatus::default()),
        client(7, "Lucas", 5, away),
        client(8, "Ines", 1, ClientStatus::default()),
        ClientInfo { role: Some("Moderator".into()), ..client(9, "Jonas", 8, ClientStatus::default()) },
    ];
    let info = ServerInfo { name: "Friday Night".into(), welcome: String::new(), max_clients: 32 };
    let channels = vec![lobby, games, ranked, duo, afk, music, staff, office, meeting, rooms, room];
    let everything = ChannelPerms { perms: Perms::ALL, rename: true };
    let everywhere = channels.iter().map(|c| (c.id, everything)).collect();
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
    let time = chrono::NaiveTime::parse_from_str(time, "%H:%M").expect("a clock time");
    Line::new(chrono::Local::now().date_naive().and_time(time), kind, text.into())
}

fn poke(from: u32, name: &str, message: &str, time: &str) -> PokeAlert {
    PokeAlert { from, name: name.into(), message: message.into(), time: time.into() }
}

fn msg(from: u32, name: &str) -> LineKind {
    LineKind::Msg { from: Speaker::Client(from), name: name.into() }
}

fn recents() -> Vec<Recent> {
    vec![
        Recent { address: "friday.example.com".into(), server_name: "Friday Night".into() },
        Recent { address: "10.0.0.12:9988".into(), server_name: "LAN party".into() },
    ]
}

fn fake_share_sources() -> crate::screen::PickerSources {
    use crate::screen::{AppWindows, Handle, PickerSources, Source, SourceInfo};
    let source = |id, name: &str| SourceInfo { source: Source::Window(Handle::fake(id)), name: name.into() };
    let icon = |[r, g, b]: [u8; 3]| {
        let pixels = (0..32 * 32)
            .map(|i| {
                let (x, y) = (i % 32, i / 32);
                let inside = (4..28).contains(&x) && (4..28).contains(&y);
                if inside { Color32::from_rgb(r, g, b) } else { Color32::TRANSPARENT }
            })
            .collect();
        Some(ColorImage::new([32, 32], pixels))
    };
    let app = |name: &str, tint: Option<[u8; 3]>, windows| AppWindows {
        name: name.into(),
        icon: tint.and_then(icon),
        windows,
    };
    PickerSources {
        screens: vec![
            SourceInfo { source: Source::Monitor(Handle::fake(1)), name: "Screen 1 (2560 × 1440)".into() },
            SourceInfo { source: Source::Monitor(Handle::fake(2)), name: "Screen 2 (1920 × 1080)".into() },
        ],
        apps: vec![
            app("Discord", Some([88, 101, 242]), vec![source(10, "#general - Discord")]),
            app(
                "Google Chrome",
                Some([234, 67, 53]),
                vec![source(11, "Patch notes 14.3 - Google Chrome"), source(12, "Tournament bracket - Google Chrome")],
            ),
            app("League of Legends", Some([200, 160, 60]), vec![source(13, "League of Legends")]),
            app("Spotify", Some([30, 215, 96]), vec![source(14, "Daft Punk - Something About Us")]),
            app("Untitled tool", None, vec![source(15, "Untitled tool")]),
        ],
    }
}

fn share_picker(ctx: &egui::Context) -> App {
    let mut app = connected(ctx);
    app.share_sources = fake_share_sources;
    app
}

fn connected(ctx: &egui::Context) -> App {
    let mut settings = Settings { nickname: "Santi".into(), recent: recents(), ..Default::default() };
    settings.user_volumes.insert("Marta".into(), 0.6);
    let mut app = App::new(ctx, settings, None, Arc::new(|| {}));
    app.view = Some(view());
    let login = Login { address: "friday.example.com".into(), password: String::new() };
    app.conn = Conn::Connected { server: ServerKey::of("friday.example.com:9988"), login };
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
        line("20:15", msg(3, "Kenji"), "can someone check if Ola is still around?"),
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

fn fake_still(ctx: &egui::Context, sharer: u32, tint: [u8; 3]) -> egui::TextureHandle {
    let [w, h] = shelf::still_size(1.0);
    let shade = move |k: u8| Color32::from_rgb(tint[0] + k, tint[1] + k, tint[2] + k);
    let pixel = move |x: usize, y: usize| {
        let (x, y) = (x * 320 / w, y * 180 / h);
        let (left, right) = ((12..90).contains(&x), (100..308).contains(&x));
        let body = (30..168).contains(&y);
        let text = right && body && (y - 30) % 14 < 5 && x < 130 + (y * 37) % 170;
        if y < 18 {
            shade(28)
        } else if text {
            shade(60)
        } else if body && (left || right) {
            shade(14)
        } else {
            shade(0)
        }
    };
    let pixels = (0..h).flat_map(|y| (0..w).map(move |x| pixel(x, y))).collect();
    ctx.load_texture(format!("still {sharer}"), ColorImage::new([w, h], pixels), TextureOptions::LINEAR)
}

fn three_sharing(ctx: &egui::Context) -> App {
    let mut app = connected(ctx);
    let view = app.view.as_mut().unwrap();
    view.apply(&S2C::ClientSharing { id: 3, sharing: true });
    view.apply(&S2C::ClientMoved { id: 5, channel: 2 });
    view.apply(&S2C::ClientSharing { id: 5, sharing: true });
    let tints = [(2, [0x14, 0x1c, 0x2a]), (3, [0x26, 0x20, 0x12]), (5, [0x22, 0x14, 0x2a])];
    app.shelf.cards = (tints.into_iter())
        .map(|(sharer, tint)| Card { sharer, still: Some(fake_still(ctx, sharer, tint)), asked: Some(Instant::now()) })
        .collect();
    app
}

fn watching(sharers: &[u32]) -> Vec<Watching> {
    sharers.iter().map(|&sharer| Watching { sharer, frames: Arc::default() }).collect()
}

fn disconnected(ctx: &egui::Context) -> App {
    let settings = Settings { nickname: "Santi".into(), recent: recents(), ..Default::default() };
    App::new(ctx, settings, None, Arc::new(|| {}))
}

fn settings_on(ctx: &egui::Context, page: SettingsPage) -> App {
    let mut app = connected(ctx);
    app.settings_dialog = Some(SettingsDialog {
        page,
        packs: vec![("Custom folder".into(), "custom".into())],
        inputs: vec!["Microphone (USB)".into()],
        outputs: vec!["Headphones".into()],
        capture: None,
        binding_error: None,
    });
    app
}

type Scene = (&'static str, (u32, u32), fn(&egui::Context) -> App);

fn attachment_thread(ctx: &egui::Context) -> App {
    use t7_proto::files::{Attachment, ChannelFile, Cursor, Event};
    let mut capture = Canvas::new((860, 480), connected, 1.0);
    capture.app.settings.status.muted = true;
    capture.app.drive_audio(&capture.ctx);
    capture.app.tabs[1].lines.retain(|line| !matches!(line.kind, LineKind::Error));
    for frame in 0..3 {
        capture.draw(capture.input(frame));
    }
    let pixels = capture.px.iter().map(|p| Color32::from_rgb(p[0], p[1], p[2])).collect();
    let screenshot =
        ctx.load_texture("channel screenshot", ColorImage::new([860, 480], pixels), TextureOptions::LINEAR);
    let mut app = connected(ctx);
    app.settings.status.muted = true;
    app.drive_audio(ctx);
    let channel = app.view.as_ref().unwrap().my_channel().unwrap();
    app.file_event(Event::Access { readable: vec![channel], can_post_here: true });
    app.files_joined(channel);
    app.tabs[1].lines.clear();
    app.shelf.folded.insert(channel);
    for (id, nickname, name, caption, size) in [
        (1, "Marta", "channel-layout.png", "Here’s the channel layout.", 106_500),
        (2, "Kenji", "lobby-recording.mp4", "And here’s the recording.", 5_830_000),
        (3, "Kenji", "session-notes.pdf", "", 48_900),
    ] {
        let file = ChannelFile {
            channel,
            attachment: Attachment {
                key: Cursor { id: [id; 16], posted_ms: 0 },
                author: Identity([if id == 1 { 1 } else { 2 }; 32]),
                nickname: nickname.into(),
                name: name.into(),
                caption: caption.into(),
                size,
            },
        };
        if id <= 2 {
            app.files.seed_thumbnail(&file, screenshot.clone());
        }
        app.tabs[1].lines.push(line("20:16", LineKind::File(file), ""));
    }
    app
}

const SCENES: &[Scene] = &[
    ("chat-attachments", (1100, 820), attachment_thread),
    ("01-empty", (1100, 720), disconnected),
    ("02-connected", (1100, 720), connected),
    ("03-connected-small", (760, 480), connected),
    ("04-connect-dialog", (1100, 720), |ctx| {
        let mut app = disconnected(ctx);
        app.open_connect_form();
        app.last_error = Some("Couldn't reach friday.example.com: connection refused".into());
        app
    }),
    ("05-voice-settings", (1100, 720), |ctx| settings_on(ctx, SettingsPage::Voice)),
    ("40-settings-cleanup", (1100, 720), |ctx| settings_on(ctx, SettingsPage::Cleanup)),
    ("41-settings-notifications", (1100, 720), |ctx| settings_on(ctx, SettingsPage::Notifications)),
    ("42-settings-sharing", (1100, 720), |ctx| settings_on(ctx, SettingsPage::Sharing)),
    ("43-settings-about", (1100, 720), |ctx| settings_on(ctx, SettingsPage::About)),
    ("44-settings-push-to-talk", (1100, 720), |ctx| {
        let mut app = settings_on(ctx, SettingsPage::Voice);
        app.settings.audio.mode = audio::TxMode::PushToTalk;
        app
    }),
    ("45-settings-small", (760, 480), |ctx| settings_on(ctx, SettingsPage::Voice)),
    ("46-key-bindings", (1100, 720), |ctx| settings_on(ctx, SettingsPage::KeyBindings)),
    ("47-key-bindings-small", (760, 480), |ctx| settings_on(ctx, SettingsPage::KeyBindings)),
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
        settings.bans = Some(vec![
            BanInfo {
                id: 1,
                nickname: "Troll".into(),
                identity: Identity([9; 32]),
                ip: None,
                reason: "Spamming the lobby".into(),
                banned_by: "Santi".into(),
                created_unix: 1_763_000_000,
                expires_unix: Some(1_763_600_000),
            },
            BanInfo {
                id: 2,
                nickname: "griefer42".into(),
                identity: Identity([11; 32]),
                ip: Some("203.0.113.7".parse().unwrap()),
                reason: String::new(),
                banned_by: "Marta".into(),
                created_unix: 1_762_050_000,
                expires_unix: None,
            },
        ]);
        app
    }),
    ("46-no-bans", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_server_settings();
        let settings = app.server_settings.as_mut().unwrap();
        settings.tab = SettingsTab::Bans;
        settings.bans = Some(Vec::new());
        app
    }),
    ("11-poke", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_poke(2);
        app
    }),
    ("12-poked", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.pokes.push_back(poke(4, "Ola", "come help in Ranked, we're one short", "20:21"));
        app
    }),
    ("47-poked-several", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.pokes.extend([
            poke(5, "Priya", "", "20:12"),
            poke(3, "Kenji", "", "20:17"),
            poke(4, "Ola", "come help in Ranked, we're one short", "20:19"),
            poke(2, "Marta", "we're starting!", "20:21"),
        ]);
        app
    }),
    ("13-delete-channel", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_delete_channel(3);
        app
    }),
    ("48-delete-empty-channel", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_delete_channel(6);
        app
    }),
    ("49-kick-from-server", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_kick(3, true);
        app
    }),
    ("50-permissions-changed", (1100, 760), |ctx| {
        let mut app = connected(ctx);
        app.open_edit_channel(8);
        let Some(ChannelDialog::Form(form)) = &mut app.channel_dialog else { unreachable!() };
        form.tab = FormTab::Permissions;
        let mut everyone = Override::inheriting(RoleId::EVERYONE);
        everyone.set(Perm::Speak, Setting::Deny);
        everyone.set(Perm::MoveMembers, Setting::Allow);
        form.overrides = vec![everyone];
        app
    }),
    ("51-create-channel-filled", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_create_channel(Some(2));
        let Some(ChannelDialog::Form(form)) = &mut app.channel_dialog else { unreachable!() };
        form.name = "Squad".into();
        form.limited = true;
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
        app.settings.folded.insert(ServerKey::of("friday.example.com:9988"), ["Staff".to_owned()].into());
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
    ("52-create-lobby-no-longer-offered", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.open_create_channel(Some(2));
        let Some(ChannelDialog::Form(form)) = &mut app.channel_dialog else { unreachable!() };
        form.kind = ChannelKind::Lobby;
        let view = app.view.as_mut().unwrap();
        let member = ChannelPerms { perms: Perms::of(&[Perm::View, Perm::Join, Perm::Speak]), rename: false };
        let channels = view.channels.keys().map(|&c| (c, member)).collect();
        view.apply(&S2C::Permissions(Permissions { server: Perms::of(&[Perm::View]), channels }));
        app
    }),
    ("26-cant-speak", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        for id in [1, 5] {
            app.apply(&S2C::ClientSpeech { id, speech: Speech::NotHere });
        }
        app
    }),
    ("27-voice-settings-cant-speak", (1100, 720), |ctx| {
        let mut app = settings_on(ctx, SettingsPage::Voice);
        app.apply(&S2C::ClientSpeech { id: 1, speech: Speech::NotHere });
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
        let moderator =
            ChannelPerms { perms: Perms::of(&[Perm::View, Perm::Join, Perm::Speak, Perm::Kick]), rename: true };
        let channels = view.channels.keys().map(|&c| (c, moderator)).collect();
        view.apply(&S2C::Permissions(Permissions { server: Perms::of(&[Perm::View]), channels }));
        app.open_edit_channel(3);
        app
    }),
    ("36-private-chat", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.active_tab = 2;
        app.tabs[2].unread = 0;
        app.tabs[2].lines = vec![
            line("20:12", msg(2, "Marta"), "can you bring the spare headset tomorrow?"),
            line("20:13", msg(1, "Santi"), "yep"),
            line("20:19", msg(2, "Marta"), "thanks!"),
        ];
        app
    }),
    ("37-empty-channel", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.apply(&S2C::ClientMoved { id: 1, channel: 6 });
        app.tabs[1].title = "Music".into();
        app.tabs[1].lines = vec![line("20:20", LineKind::System, "You joined Music")];
        app
    }),
    ("38-kicked", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        let Conn::Connected { server, login } = std::mem::replace(&mut app.conn, Conn::Disconnected) else {
            unreachable!()
        };
        let why = Lost { headline: "Kicked from the server".into(), detail: "by Marta: \"take a break\"".into() };
        app.conn = Conn::Lost { server, login, why, last: Box::new(app.view.take().unwrap()) };
        app
    }),
    ("39-first-run", (1100, 720), |ctx| {
        let settings = Settings { nickname: "Santi".into(), ..Default::default() };
        App::new(ctx, settings, None, Arc::new(|| {}))
    }),
    ("33-update-ready", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.updater = crate::update::Updater::showing(crate::update::Status::Ready("0.7.0".into()));
        app
    }),
    ("16-connecting", (1100, 720), |ctx| {
        let mut app = disconnected(ctx);
        app.conn = Conn::Connecting(Login { address: "friday.example.com".into(), password: String::new() });
        app
    }),
    ("53-shelf-one", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        let still = Some(fake_still(ctx, 2, [0x14, 0x1c, 0x2a]));
        app.shelf.cards = vec![Card { sharer: 2, still, asked: Some(Instant::now()) }];
        app
    }),
    ("54-shelf-three", (1100, 720), three_sharing),
    ("55-shelf-watching-two", (1100, 720), |ctx| {
        let mut app = three_sharing(ctx);
        app.watching = watching(&[2, 3]);
        app
    }),
    ("56-shelf-folded", (1100, 720), |ctx| {
        let mut app = three_sharing(ctx);
        app.watching = watching(&[3]);
        app.shelf.folded.insert(2);
        app
    }),
    ("57-shelf-small", (760, 480), three_sharing),
    ("58-shelf-one-watching", (1100, 720), |ctx| {
        let mut app = connected(ctx);
        app.shelf.cards = vec![Card { sharer: 2, still: None, asked: Some(Instant::now()) }];
        app.watching = watching(&[2]);
        app
    }),
];

#[derive(Clone, Copy)]
struct Step {
    x: f32,
    y: f32,
    button: Option<(egui::PointerButton, bool)>,
}

const fn hover(x: f32, y: f32) -> Step {
    Step { x, y, button: None }
}

const fn press(x: f32, y: f32, button: egui::PointerButton) -> Step {
    Step { x, y, button: Some((button, true)) }
}

const fn release(x: f32, y: f32, button: egui::PointerButton) -> Step {
    Step { x, y, button: Some((button, false)) }
}

const LEFT: egui::PointerButton = egui::PointerButton::Primary;
const RIGHT: egui::PointerButton = egui::PointerButton::Secondary;

struct Menu {
    name: &'static str,
    size: (u32, u32),
    make: fn(&egui::Context) -> App,
    steps: &'static [Step],
}

const MENUS: &[Menu] = &[
    Menu {
        name: "19-user-roles",
        size: (1100, 720),
        make: connected,
        steps: &[press(80.0, 181.0, RIGHT), release(80.0, 181.0, RIGHT), hover(150.0, 424.0)],
    },
    Menu {
        name: "29-user-move-to",
        size: (1100, 720),
        make: connected,
        steps: &[press(80.0, 295.0, RIGHT), release(80.0, 295.0, RIGHT), hover(150.0, 482.0)],
    },
    Menu {
        name: "30-server-menu",
        size: (1100, 720),
        make: connected,
        steps: &[press(256.0, 26.0, LEFT), release(256.0, 26.0, LEFT), hover(200.0, 120.0)],
    },
    Menu {
        name: "31-your-menu",
        size: (1100, 720),
        make: connected,
        steps: &[press(60.0, 692.0, LEFT), release(60.0, 692.0, LEFT), hover(100.0, 640.0)],
    },
    Menu {
        name: "32-channel-menu",
        size: (1100, 720),
        make: connected,
        steps: &[press(80.0, 238.0, RIGHT), release(80.0, 238.0, RIGHT), hover(150.0, 260.0)],
    },
    Menu {
        name: "59-share-picker",
        size: (1100, 720),
        make: share_picker,
        steps: &[
            press(256.0, 692.0, LEFT),
            release(256.0, 692.0, LEFT),
            hover(400.0, 300.0),
            hover(400.0, 300.0),
            hover(330.0, 540.0),
        ],
    },
    Menu {
        name: "60-share-picker-expanded",
        size: (1100, 720),
        make: share_picker,
        steps: &[
            press(256.0, 692.0, LEFT),
            release(256.0, 692.0, LEFT),
            hover(400.0, 300.0),
            hover(400.0, 300.0),
            press(330.0, 590.0, LEFT),
            release(330.0, 590.0, LEFT),
            hover(330.0, 630.0),
        ],
    },
    Menu { name: "34-hover-channel", size: (1100, 720), make: connected, steps: &[hover(120.0, 529.0)] },
    Menu {
        name: "35-drag-member",
        size: (1100, 720),
        make: connected,
        steps: &[press(90.0, 296.0, LEFT), hover(90.0, 250.0), hover(90.0, 150.0), hover(90.0, 67.0)],
    },
];

const FRAMES_BEFORE_STEPS: usize = 2;

const FRAMES_PER_SECOND: f64 = 4.0;

struct Canvas {
    ctx: egui::Context,
    app: App,
    renderer: EguiSoftwareRender,
    px: Vec<[u8; 4]>,
    size: (u32, u32),
    scale: f32,
    copied_text: Option<String>,
}

impl Canvas {
    fn new((w, h): (u32, u32), make: impl FnOnce(&egui::Context) -> App, scale: f32) -> Self {
        let ctx = egui::Context::default();
        let app = make(&ctx);
        let renderer = EguiSoftwareRender::new(ColorFieldOrder::Rgba).with_caching(false);
        Self { ctx, app, renderer, px: vec![[0u8; 4]; (w * h) as usize], size: (w, h), scale, copied_text: None }
    }

    fn input(&self, frame: usize) -> egui::RawInput {
        let (w, h, scale) = (self.size.0 as f32, self.size.1 as f32, self.scale);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(w / scale, h / scale));
        let time = Some(frame as f64 / FRAMES_PER_SECOND);
        let mut raw = egui::RawInput { screen_rect: Some(screen), time, ..Default::default() };
        raw.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(scale);
        raw
    }

    fn draw(&mut self, raw: egui::RawInput) {
        self.draw_observing(raw, |_| {});
    }

    fn draw_observing(&mut self, mut raw: egui::RawInput, inspect: impl FnOnce(&[egui::epaint::ClippedShape])) {
        let app = &mut self.app;
        app.prepare_input(&self.ctx, &mut raw, || None);
        let out = self.ctx.run_ui(raw, |ui| app.ui(ui));
        for command in out.platform_output.commands {
            if let egui::OutputCommand::CopyText(text) = command {
                self.copied_text = Some(text);
            }
        }
        inspect(&out.shapes);
        let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        let mut target = BufferMutRef::new(&mut self.px, self.size.0 as usize, self.size.1 as usize);
        let mut delta = out.textures_delta;
        self.renderer.render(&mut target, &prims, &delta, out.pixels_per_point);
        delta.clear();
    }
}

fn render(
    name: &str,
    size: (u32, u32),
    make: fn(&egui::Context) -> App,
    steps: &[Step],
    scale: f32,
    dir: &std::path::Path,
) {
    let mut canvas = Canvas::new(size, make, scale);
    // Enough frames for layout to settle and fades to finish.
    for frame in 0..6 + steps.len() {
        let mut raw = canvas.input(frame);
        if let Some(step) = frame.checked_sub(FRAMES_BEFORE_STEPS).and_then(|step| steps.get(step)) {
            let pos = egui::pos2(step.x, step.y);
            raw.events.push(egui::Event::PointerMoved(pos));
            if let Some((button, pressed)) = step.button {
                let modifiers = egui::Modifiers::default();
                raw.events.push(egui::Event::PointerButton { pos, button, pressed, modifiers });
            }
        }
        canvas.draw(raw);
    }
    save_canvas(&canvas, dir, name);
}

fn save_canvas(canvas: &Canvas, dir: &std::path::Path, name: &str) {
    let (w, h) = canvas.size;
    let px = &canvas.px;
    let file = std::fs::File::create(dir.join(format!("{name}.png"))).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    let mut writer = enc.write_header().expect("png header");
    let mut bytes: Vec<u8> = px.iter().flatten().copied().collect();
    bytes.chunks_mut(4).for_each(|p| p[3] = 255);
    writer.write_image_data(&bytes).expect("png data");
}

fn picked_by_t7_snapshot(name: &str, when_unset: &str) -> bool {
    name.contains(&std::env::var("T7_SNAPSHOT").unwrap_or_else(|_| when_unset.to_owned()))
}

#[test]
#[ignore = "writes PNGs for visual review"]
fn snapshots() {
    let dir = std::env::var_os("T7_SNAPSHOT_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/snapshots"), PathBuf::from);
    std::fs::create_dir_all(&dir).expect("snapshot dir");
    let scenes = SCENES.iter().map(|&(name, size, make)| (name, size, make, &[][..]));
    let menus = MENUS.iter().map(|m| (m.name, m.size, m.make, m.steps));
    for (name, size, make, steps) in scenes.chain(menus) {
        if picked_by_t7_snapshot(name, "") {
            render(name, size, make, steps, 1.0, &dir);
        }
    }
}

#[test]
#[ignore = "measures redraw time; run it in release"]
fn frame_time() {
    const FRAMES: usize = 260;
    const KEPT: usize = 200;
    let &(name, size, make) =
        SCENES.iter().find(|(name, ..)| picked_by_t7_snapshot(name, "02-connected")).expect("a scene");
    let mut canvas = Canvas::new(size, make, 1.0);
    let mut times: Vec<f64> = (0..FRAMES)
        .map(|frame| {
            let raw = canvas.input(frame);
            let started = Instant::now();
            canvas.draw(raw);
            started.elapsed().as_secs_f64() * 1000.0
        })
        .skip(FRAMES - KEPT)
        .collect();
    times.sort_by(f64::total_cmp);
    let at = |q: f64| times[((KEPT - 1) as f64 * q).round() as usize];
    println!(
        "frame_time {name} {}x{}, last {KEPT} of {FRAMES} frames: median {:.3} ms, p10 {:.3} ms, p90 {:.3} ms",
        size.0,
        size.1,
        at(0.5),
        at(0.1),
        at(0.9)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "drives two native clients and a local server through clipboard sharing"]
async fn clipboard_to_chat_and_files() {
    let root = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let path = root.path().to_owned();
    let server = tokio::spawn(async move {
        t7_server::serve(listener, &t7_server::config::Config::default(), Some(&path)).await.unwrap();
    });
    let client = |nick: &str, size| {
        Canvas::new(
            size,
            |ctx| {
                let mut settings = Settings::default();
                settings.status.muted = true;
                settings.status.deafened = true;
                let launch = Launch { nickname: Some(nick.into()), address: address.clone() };
                App::new(ctx, settings, Some(launch), Arc::new(|| {}))
            },
            1.0,
        )
    };
    let mut sender = client("Mira", (1100, 720));
    let mut viewer = client("Jules", (760, 480));
    let mut frame = 0;
    fn tick(canvas: &mut Canvas, frame: &mut usize, event: Option<egui::Event>) {
        let mut raw = canvas.input(*frame);
        *frame += 1;
        raw.events.extend(event);
        canvas.draw(raw);
    }
    fn click(canvas: &mut Canvas, frame: &mut usize, text: &str) {
        let mut pos = None;
        canvas.draw_observing(canvas.input(*frame), |shapes| {
            for shape in shapes {
                if let egui::Shape::Text(t) = &shape.shape
                    && t.galley.text() == text
                {
                    pos = Some(t.pos + t.galley.size() / 2.0);
                }
            }
        });
        click_at(canvas, frame, pos.unwrap_or_else(|| panic!("missing visible control: {text}")));
    }
    fn click_at(canvas: &mut Canvas, frame: &mut usize, pos: egui::Pos2) {
        tick(canvas, frame, Some(egui::Event::PointerMoved(pos)));
        for pressed in [true, false] {
            tick(
                canvas,
                frame,
                Some(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                }),
            );
        }
    }
    async fn until(canvas: &mut Canvas, frame: &mut usize, condition: impl Fn(&Canvas) -> bool) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !condition(canvas) {
                tick(canvas, frame, None);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("client did not reach the expected state");
    }
    until(&mut viewer, &mut frame, |c| c.app.files.can_post()).await;
    until(&mut sender, &mut frame, |c| c.app.files.can_post()).await;
    sender.app.focus_input = true;
    tick(&mut sender, &mut frame, None);
    let enter = |modifiers, pressed| egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers,
    };
    tick(&mut sender, &mut frame, Some(egui::Event::Paste("**Example**".into())));
    for pressed in [true, false] {
        tick(&mut sender, &mut frame, Some(enter(egui::Modifiers::SHIFT, pressed)));
    }
    let mut raw = sender.input(frame);
    frame += 1;
    raw.events.extend([egui::Event::Paste("```python".into()), enter(egui::Modifiers::NONE, true)]);
    sender.draw(raw);
    tick(&mut sender, &mut frame, Some(enter(egui::Modifiers::NONE, false)));
    assert_eq!(sender.app.input, "**Example**\n```python\n");
    let code = "for i in range(10):\n    print(i)";
    tick(&mut sender, &mut frame, Some(egui::Event::Paste(code.into())));
    tick(&mut sender, &mut frame, None);
    if let Some(dir) = std::env::var_os("T7_SNAPSHOT_DIR").map(PathBuf::from) {
        std::fs::create_dir_all(&dir).unwrap();
        save_canvas(&sender, &dir, "writing-python");
    }
    tick(&mut sender, &mut frame, Some(egui::Event::Paste("\n```".into())));
    for pressed in [true, false] {
        tick(&mut sender, &mut frame, Some(enter(egui::Modifiers::NONE, pressed)));
    }
    assert_eq!(sender.app.input, "");
    until(&mut viewer, &mut frame, |c| {
        c.app.tabs[1].lines.iter().any(|line| matches!(line.kind, LineKind::Msg { .. }))
    })
    .await;
    let mut copy = None;
    viewer.draw_observing(viewer.input(frame), |shapes| {
        let code_pos = shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == code => Some(text.pos),
                _ => None,
            })
            .expect("the receiver displays the indented code block");
        copy = shapes.iter().rev().find_map(|shape| match &shape.shape {
            egui::Shape::Rect(card) if card.fill == theme::SURFACE && card.rect.contains(code_pos) => {
                Some(card.rect.right_top() + vec2(-24.0, 21.0))
            }
            _ => None,
        });
    });
    click_at(&mut viewer, &mut frame, copy.expect("visible copy control on the code block"));
    assert_eq!(viewer.copied_text.as_deref(), Some("for i in range(10):\n    print(i)\n"));
    let green = Color32::from_rgb(21, 215, 93);
    let image = Arc::new(ColorImage::filled([640, 240], green));
    let has_green = |c: &Canvas| c.px.iter().filter(|p| p[..3] == [21, 215, 93]).count() > 1000;
    tick(&mut sender, &mut frame, Some(egui::Event::PasteImage(image.clone())));
    let blue = Arc::new(ColorImage::filled([80, 240], Color32::from_rgb(38, 102, 212)));
    tick(&mut sender, &mut frame, Some(egui::Event::PasteImage(blue)));
    until(&mut sender, &mut frame, |c| !c.app.files.preparing_image() && has_green(c)).await;
    let tile_bounds = |c: &Canvas, rgb: [u8; 3]| {
        let mut bounds = egui::Rect::NOTHING;
        let width = c.size.0 as usize;
        for (index, _) in c.px.iter().enumerate().filter(|(_, p)| p[..3] == rgb) {
            bounds.extend_with(egui::pos2((index % width) as f32, (index / width) as f32));
        }
        assert!(bounds.is_positive());
        bounds
    };
    let first = tile_bounds(&sender, [21, 215, 93]);
    let second = tile_bounds(&sender, [38, 102, 212]);
    assert!(second.left() > first.right() && (first.top() - second.top()).abs() < 2.0);
    let caption = sender.ctx.read_response(egui::Id::unique("chat-composer")).unwrap();
    assert!(caption.rect.top() - first.bottom() >= 12.0);
    if let Some(dir) = std::env::var_os("T7_SNAPSHOT_DIR").map(PathBuf::from) {
        std::fs::create_dir_all(&dir).unwrap();
        save_canvas(&sender, &dir, "chat-image-draft");
    }
    assert!(!has_green(&viewer));
    click_at(&mut sender, &mut frame, first.right_top() + vec2(-12.0, 12.0));
    tick(&mut sender, &mut frame, None);
    assert!(sender.app.files.has_staged() && !has_green(&sender));
    let remaining = tile_bounds(&sender, [38, 102, 212]);
    click_at(&mut sender, &mut frame, remaining.right_top() + vec2(-12.0, 12.0));
    tick(&mut sender, &mut frame, None);
    assert!(!sender.app.files.has_staged() && !has_green(&sender));
    sender.app.focus_input = true;
    tick(&mut sender, &mut frame, None);
    tick(&mut sender, &mut frame, Some(egui::Event::Paste("**Caption**\n\tsecond line".into())));
    tick(&mut sender, &mut frame, Some(egui::Event::PasteImage(image)));
    until(&mut sender, &mut frame, |c| c.app.files.has_staged() && !c.app.files.preparing_image()).await;
    let send = sender.ctx.read_response(egui::Id::unique("chat-send")).unwrap();
    click_at(&mut sender, &mut frame, send.rect.center());
    until(&mut viewer, &mut frame, has_green).await;
    assert!(viewer.app.tabs[1].lines.iter().any(
        |line| matches!(&line.kind, LineKind::File(file) if file.attachment.caption == "**Caption**\n\tsecond line")
    ));
    viewer.draw_observing(viewer.input(frame), |shapes| {
        assert!(
            !shapes
                .iter()
                .any(|s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.text() == "Clipboard image.png"))
        );
    });
    let inline = tile_bounds(&viewer, [21, 215, 93]);
    click_at(&mut viewer, &mut frame, inline.center());
    until(&mut viewer, &mut frame, |c| has_green(c) && tile_bounds(c, [21, 215, 93]).width() > inline.width() + 20.0)
        .await;
    click(&mut viewer, &mut frame, "Close");
    tick(&mut viewer, &mut frame, None);
    click(&mut viewer, &mut frame, "Files");
    assert!(viewer.app.files.drawer_open);
    tick(&mut viewer, &mut frame, None);
    let dir = std::env::var_os("T7_SNAPSHOT_DIR").map(PathBuf::from);
    if let Some(dir) = &dir {
        std::fs::create_dir_all(dir).unwrap();
        save_canvas(&viewer, dir, "files-small");
    }
    click(&mut viewer, &mut frame, "View all files");
    assert!(viewer.app.files.library_open);
    until(&mut viewer, &mut frame, |c| c.app.tabs[1].lines.iter().any(|l| matches!(l.kind, LineKind::File(_)))).await;
    for _ in 0..8 {
        tick(&mut viewer, &mut frame, None);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if let Some(dir) = &dir {
        save_canvas(&viewer, dir, "files-library");
    }
    click(&mut viewer, &mut frame, "Clipboard image.png");
    tick(&mut viewer, &mut frame, None);
    click(&mut viewer, &mut frame, "Show in chat");
    assert!(!viewer.app.files.library_open);
    if let Ok(url) = std::env::var("T7_TEST_IMAGE_URL") {
        sender.app.focus_input = true;
        tick(&mut sender, &mut frame, None);
        tick(&mut sender, &mut frame, Some(egui::Event::Paste(url.clone())));
        for pressed in [true, false] {
            tick(
                &mut sender,
                &mut frame,
                Some(egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: egui::Modifiers::default(),
                }),
            );
        }
        let uri = url.parse().expect("image URL");
        until(&mut viewer, &mut frame, |c| c.app.files.linked_preview_ready(&uri)).await;
    }
    server.abort();
}

#[test]
fn composer_centers_single_line_text_with_send_button() {
    for scale in [1.0, 1.5] {
        let mut canvas = Canvas::new((1200, 800), connected, scale);
        canvas.app.files.limit = Some(1_000_000);
        canvas.app.focus_input = true;
        for (draft, visible) in [("", "Message #Games"), ("hello", "hello")] {
            canvas.app.input = draft.into();
            for frame in 0..3 {
                canvas.draw(canvas.input(frame));
            }
            let mut text_y = None;
            canvas.draw_observing(canvas.input(3), |shapes| {
                for shape in shapes {
                    if let egui::Shape::Text(text) = &shape.shape
                        && text.galley.text() == visible
                    {
                        text_y = Some(text.pos.y + text.galley.size().y / 2.0);
                    }
                }
            });
            let send = canvas.ctx.read_response(egui::Id::unique("chat-send")).unwrap();
            let offset = text_y.expect("visible composer text") - send.rect.center().y;
            assert!(offset.abs() <= 1.0, "{visible:?} at {scale}x: vertical offset {offset}");
        }
    }
}

#[test]
fn composer_grows_to_fit_wrapped_and_fenced_drafts() {
    for size in [(760, 480), (1100, 720)] {
        let mut canvas = Canvas::new(size, connected, 1.0);
        let mut frame = 0;
        let mut height = |draft: &str| {
            canvas.app.input = draft.to_owned();
            for _ in 0..5 {
                canvas.draw(canvas.input(frame));
                frame += 1;
            }
            let edit = canvas.ctx.read_response(egui::Id::unique("chat-composer")).unwrap();
            edit.interact_rect.height()
        };
        let empty = height("");
        let medium = height("```python\nfor i in range(10):\n    print(i)\n    print(i + 1)\n");
        let wrapped = height(&"a longer line of text ".repeat(16));
        let tall = height(&format!("```python\n{}", "    print(i)\n".repeat(60)));
        let taller = height(&format!("```python\n{}", "    print(i)\n".repeat(100)));
        assert!(medium > empty + 20.0 && wrapped > empty + 10.0, "{size:?}: {empty} {medium} {wrapped}");
        assert!(tall > medium && tall <= 242.0 && (tall - taller).abs() < 1.0, "{size:?}: {medium} {tall} {taller}");
        assert!((height("") - empty).abs() < 1.0);
    }
}

fn key_event(key: egui::Key, modifiers: egui::Modifiers, pressed: bool) -> egui::Event {
    egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers }
}

fn press_key(canvas: &mut Canvas, frame: &mut usize, key: egui::Key, modifiers: egui::Modifiers) {
    for pressed in [true, false] {
        let mut raw = canvas.input(*frame);
        *frame += 1;
        raw.events.push(key_event(key, modifiers, pressed));
        canvas.draw(raw);
    }
}

fn bind(canvas: &mut Canvas, action: Action, key: egui::Key, modifiers: egui::Modifiers) {
    use crate::bindings::{Binding, Chord};
    let chord = Chord::from_press(&key_event(key, modifiers, true)).unwrap().0;
    let settings = &mut canvas.app.settings;
    settings.bindings.set(action, &mut settings.audio.ptt_key, Some(Binding::Chord(chord))).unwrap();
}

#[test]
fn capture_consumes_shortcuts_and_saves_the_new_key() {
    use egui::{Key, Modifiers};
    let mut canvas = Canvas::new((760, 480), |ctx| settings_on(ctx, SettingsPage::KeyBindings), 1.0);
    for frame in 0..3 {
        canvas.draw(canvas.input(frame));
    }
    let label = canvas.app.settings.bindings.get(Action::ToggleAway, None).unwrap().label(&canvas.ctx);
    let mut pos = None;
    canvas.draw_observing(canvas.input(3), |shapes| {
        pos = shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == label => Some(text.pos + text.galley.size() / 2.0),
            _ => None,
        });
    });
    let pos = pos.expect("visible Away binding");
    for (frame, pressed) in [(4, true), (5, false)] {
        let mut raw = canvas.input(frame);
        raw.events.extend([
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            },
        ]);
        canvas.draw(raw);
    }
    assert!(canvas.app.capturing_binding());
    let mut frame = 6;
    let command_shift = Modifiers::COMMAND | Modifiers::SHIFT;
    press_key(&mut canvas, &mut frame, Key::M, command_shift);
    assert!(!canvas.app.settings.status.muted);
    assert!(canvas.app.capturing_binding(), "an overlapping binding is rejected");
    press_key(&mut canvas, &mut frame, Key::C, Modifiers::COMMAND);
    assert!(canvas.app.capturing_binding(), "the platform copy chord cannot be assigned here");
    press_key(&mut canvas, &mut frame, Key::Escape, Modifiers::NONE);
    assert!(!canvas.app.capturing_binding());
    assert!(canvas.app.settings_dialog.is_some(), "cancelling recording also closed Settings");
    assert_eq!(canvas.app.settings.bindings.get(Action::ToggleAway, None).unwrap().label(&canvas.ctx), label);
    for pressed in [true, false] {
        let mut raw = canvas.input(frame);
        frame += 1;
        raw.events.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        });
        canvas.draw(raw);
    }
    assert!(canvas.app.capturing_binding());
    let ctrl = Modifiers::CTRL | Modifiers::COMMAND;
    for event in [
        key_event(Key::ControlLeft, ctrl, true),
        egui::Event::ModifiersChanged(ctrl),
        key_event(Key::F6, ctrl, true),
        key_event(Key::F6, ctrl, false),
        key_event(Key::ControlLeft, Modifiers::NONE, false),
        egui::Event::ModifiersChanged(Modifiers::NONE),
    ] {
        let mut raw = canvas.input(frame);
        frame += 1;
        raw.events.push(event);
        canvas.draw(raw);
    }
    assert!(!canvas.app.capturing_binding());
    assert!(!canvas.app.settings.status.away);
    press_key(&mut canvas, &mut frame, Key::A, command_shift);
    assert!(!canvas.app.settings.status.away);
    press_key(&mut canvas, &mut frame, Key::F6, Modifiers::NONE);
    assert!(!canvas.app.settings.status.away);
    press_key(&mut canvas, &mut frame, Key::F6, ctrl);
    assert!(canvas.app.settings.status.away);
}

#[test]
fn only_the_top_dialog_uses_its_submit_binding() {
    use egui::{Key, Modifiers};
    let mut canvas = Canvas::new((760, 480), connected, 1.0);
    canvas.app.open_poke(2);
    canvas.app.open_edit_channel(2);
    bind(&mut canvas, Action::SendPoke, Key::F6, Modifiers::NONE);
    bind(&mut canvas, Action::EditChannel, Key::F6, Modifiers::NONE);
    bind(&mut canvas, Action::DismissDialog, Key::F7, Modifiers::NONE);
    for frame in 0..3 {
        canvas.draw(canvas.input(frame));
    }
    let mut frame = 3;
    press_key(&mut canvas, &mut frame, Key::Enter, Modifiers::NONE);
    press_key(&mut canvas, &mut frame, Key::Escape, Modifiers::NONE);
    assert!(canvas.app.channel_dialog.is_some() && canvas.app.poke_form.is_some());
    press_key(&mut canvas, &mut frame, Key::F7, Modifiers::NONE);
    assert!(canvas.app.channel_dialog.is_none() && canvas.app.poke_form.is_some());
    press_key(&mut canvas, &mut frame, Key::F6, Modifiers::NONE);
    assert!(canvas.app.poke_form.is_none());
}

#[test]
fn remapped_composer_keys_work_with_prose_and_code() {
    use egui::{Event, Key, Modifiers};
    let mut canvas = Canvas::new((760, 480), connected, 1.0);
    bind(&mut canvas, Action::ToggleAway, Key::E, Modifiers::NONE);
    for (action, key) in [
        (Action::SendChat, Key::F6),
        (Action::ChatNewline, Key::F7),
        (Action::ChatIndent, Key::F8),
        (Action::SendCode, Key::F6),
        (Action::CodeNewline, Key::F7),
        (Action::CodeIndent, Key::F8),
        (Action::CodeOutdent, Key::F9),
    ] {
        bind(&mut canvas, action, key, Modifiers::NONE);
    }
    canvas.app.focus_input = true;
    canvas.draw(canvas.input(0));
    let mut raw = canvas.input(1);
    raw.events.extend([key_event(Key::E, Modifiers::NONE, true), Event::Text("Example".into())]);
    canvas.draw(raw);
    assert!(!canvas.app.settings.status.away);
    let mut frame = 2;
    press_key(&mut canvas, &mut frame, Key::Enter, Modifiers::NONE);
    press_key(&mut canvas, &mut frame, Key::Enter, Modifiers::SHIFT);
    assert_eq!(canvas.app.input, "Example");
    press_key(&mut canvas, &mut frame, Key::F8, Modifiers::NONE);
    press_key(&mut canvas, &mut frame, Key::F7, Modifiers::NONE);
    assert_eq!(canvas.app.input, "Example\t\n");
    let mut raw = canvas.input(frame);
    frame += 1;
    raw.events.push(Event::Paste("```rust\n".into()));
    canvas.draw(raw);
    press_key(&mut canvas, &mut frame, Key::F8, Modifiers::NONE);
    assert_eq!(canvas.app.input, "Example\t\n```rust\n\t");
    press_key(&mut canvas, &mut frame, Key::F9, Modifiers::NONE);
    press_key(&mut canvas, &mut frame, Key::F7, Modifiers::NONE);
    assert_eq!(canvas.app.input, "Example\t\n```rust\n\n");
    press_key(&mut canvas, &mut frame, Key::Enter, Modifiers::COMMAND);
    assert_eq!(canvas.app.input, "Example\t\n```rust\n\n");
    press_key(&mut canvas, &mut frame, Key::F6, Modifiers::NONE);
    assert_eq!(canvas.app.input, "");

    let mut raw = canvas.input(frame);
    frame += 1;
    raw.events.push(Event::Text("Still here".into()));
    canvas.draw(raw);
    canvas.ctx.memory_mut(|m| m.request_focus(egui::Id::unique("chat-send")));
    press_key(&mut canvas, &mut frame, Key::Enter, Modifiers::NONE);
    assert_eq!(canvas.app.input, "Still here", "focused send button cannot bypass the remap");
    press_key(&mut canvas, &mut frame, Key::F6, Modifiers::NONE);
    assert_eq!(canvas.app.input, "");
}

#[test]
fn chat_paste_remap_keeps_platform_text_paste() {
    use egui::{Event, Key, Modifiers};
    let mut canvas = Canvas::new((760, 480), connected, 1.0);
    bind(&mut canvas, Action::ChatPaste, Key::F6, Modifiers::NONE);
    canvas.app.focus_input = true;
    canvas.draw(canvas.input(0));
    let mut raw = canvas.input(1);
    raw.events.push(Event::Paste("plain ".into()));
    canvas.draw(raw);
    assert_eq!(canvas.app.input, "plain ");

    let mut raw = canvas.input(2);
    raw.events.push(key_event(Key::F6, Modifiers::NONE, true));
    canvas.app.prepare_input(&canvas.ctx, &mut raw, || Some(Event::Paste("custom".into())));
    canvas.ctx.run_ui(raw, |ui| canvas.app.ui(ui)).drop_without_applying_deltas();
    assert_eq!(canvas.app.input, "plain custom");
}

#[cfg(windows)]
#[test]
fn background_shortcuts_toggle_without_a_frame_and_follow_the_settings() {
    use crate::hotkey::{Backend, Hotkeys, State};
    struct Accept;
    impl Backend for Accept {
        fn register(&mut self, _: i32, _: crate::bindings::Hotkey) -> Result<(), String> {
            Ok(())
        }
        fn unregister(&mut self, _: i32) {}
    }
    let mut canvas = Canvas::new((760, 480), connected, 1.0);
    canvas.app.hotkeys = Hotkeys::with_backend(Box::new(Accept));
    let mut frame = 0;
    let mut draw = |canvas: &mut Canvas| {
        canvas.draw(canvas.input(frame));
        frame += 1;
    };
    draw(&mut canvas);
    assert_eq!(canvas.app.hotkeys.state(Action::ToggleMic), &State::Off);
    assert!(!canvas.app.background_hotkey(1), "nothing is registered until it is opted in");

    canvas.app.settings.background.set(Action::ToggleMic, true);
    draw(&mut canvas);
    let (id, _) = (1..=2).find_map(|id| canvas.app.hotkeys.resolve(id).map(|found| (id, found))).unwrap();
    assert!(canvas.app.background_hotkey(id));
    let status = canvas.app.settings.status;
    assert!(status.muted && !status.deafened, "only the microphone toggles");
    assert!(canvas.app.background_hotkey(id));
    assert!(!canvas.app.settings.status.muted);
    assert!(!canvas.app.background_hotkey(id + 40), "an unknown registration is ignored");

    bind(&mut canvas, Action::ToggleMic, egui::Key::F11, egui::Modifiers::NONE);
    assert!(!canvas.app.background_hotkey(id), "a message for the old chord is dropped");
    draw(&mut canvas);
    assert!(matches!(
        canvas.app.hotkeys.state(Action::ToggleMic),
        State::Failed(why) if why.contains(Action::ToggleFullscreen.label())
    ));
    assert!(canvas.app.hotkeys.resolve(id).is_none());

    canvas.app.settings.background.set(Action::ToggleMic, false);
    draw(&mut canvas);
    assert_eq!(canvas.app.hotkeys.state(Action::ToggleMic), &State::Off);
}
