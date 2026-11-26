<img src="assets/icon.svg" width="64" height="64" alt="">

# T7

A lightweight, fast voice-chat server and client:
one server you host yourself, a tree of channels, and people in them. No accounts,
no cloud, no bloat.

> **Status:** voice works. Server, channel tree, presence, voice chat per channel
> (voice activation or push-to-talk), text chat (server, channel, private),
> mute / sound off / away, roles and channel permissions, rooms of your own, and
> screen sharing (Windows only for now).

## Quick start

```sh
cargo build --release

# Server: writes t7-server.toml with defaults on first run.
./target/release/t7-server

# Client: open the GUI, pick a nickname and enter the server's address.
./target/release/t7-client
# ...or connect straight away:
./target/release/t7-client 127.0.0.1 --nick Santiago
```

The server uses port **9988 on both TCP (control) and UDP (voice)**. Open both in
your firewall. When connecting you can leave the port off. If the client shows
"Connected · no voice", UDP is being blocked.

## Sharing the client with friends

Bake your server in as the default, so first-time users only pick a nickname:

```sh
T7_DEFAULT_SERVER=your.server.ip cargo build --release -p t7-client
```

On Windows the C runtime is linked statically (`.cargo/config.toml`), so the single
`t7-client.exe` runs without installing anything. To run several clients on one
machine with separate settings, set `T7_PROFILE=<name>`.

## Using the client

| Action | How |
| --- | --- |
| Connect | `Ctrl+S`, the + next to T7 at the top of the sidebar, or click a server you've been on. The first time, T7 opens on the connect form |
| Switch channel | Double-click a channel, drag yourself onto it, or right-click (or hover and click ⋯) → Join channel. Channels with a lock ask for a password |
| New channel | The ⋯ next to the server's name, or a channel's menu → Create sub-channel. Yours are temporary: they disappear once everyone has left |
| A room of your own | Double-click a lobby, like **Create a room**. T7 opens a temporary room for you in it, and you own it |
| Edit, rearrange, delete | Right-click a channel, or hover it and click ⋯. Those who manage channels can also drag a channel above, below or into another one. A room's owner can edit or delete the room, and a channel's moderators can rename it |
| Claim a server | ⋯ next to the server's name → Claim ownership…, with the claim token from the server's log. See [Roles & permissions](#roles--permissions) |
| Kick, ban, mute, move | Right-click a user. The menu shows what your role allows. Drag someone onto a channel to move them |
| Roles and the ban list | ⋯ next to the server's name → Server settings…, with a Roles tab and a Bans tab |
| Disconnect, reconnect | ⋯ next to the server's name → Disconnect. If the connection drops or you're kicked, the tree stays dimmed with why, and Reconnect |
| Private message | Double-click a user, or right-click → Send private message |
| Format a message | Type Markdown or paste formatted text into chat. By default, **Enter** sends and **Shift+Enter** adds a line. Start a line with three backticks and an optional language such as `python` to write a code block. Inside the block, **Enter** adds a line, **Tab** indents, **Shift+Tab** outdents, and the **↑** send button or **Ctrl+Enter** (**⌘+Enter** on Mac) sends the draft |
| Share an image | Paste its public HTTP(S) URL into chat, or paste an image into the channel message field. Check the thumbnail, optionally add a caption, then click the **↑** send button. T7 previews PNG, JPEG, GIF and WebP images up to 8 MiB and 4096 pixels per side. Larger images can still be attached as files |
| View shared media | Click an attached image to enlarge it. Click a video's play button to download the clip and open it in your system player. On Windows, clips up to 8 MiB can show a poster frame. Click the video's filename for save options |
| Upload or save a file | In channel chat, choose **+**, select files, optionally enter a caption and click the **↑** send button. Select a posted file and choose **Save file**. Linux uses Zenity for file dialogs |
| Find a file | **Files** opens the current channel's drawer beside chat. **All files** searches accessible channels by filename, uploader or channel name. Select a file to save it or show its post in chat |
| Set the upload limit | A server admin with the server-wide **Manage roles** permission can open **Server settings → Files** and set the maximum GB per file. The default is 4 GB (4,000,000,000 bytes) |
| Close a PM tab | Its ×, a middle-click, or right-click → Close |
| Mute mic / sound off | `Ctrl+Shift+M` / `Ctrl+Shift+D`, or the buttons next to your name, bottom-left. Sound off silences the other people, not notification sounds |
| Away, nickname | Click your name bottom-left, or `Ctrl+Shift+A` for away. A new nickname is used the next time you connect |
| Someone's volume | Right-click them: 0% (muted for you) to 200%. Remembered by nickname |
| Share your screen | The screen button next to your name: pick a screen or window, and a quality (720p 30, 1080p 30 or 1080p 60). Click it again to stop. Windows only for now |
| Watch screens | **Watch** under someone's still in the shelf over the chat, the red **LIVE** badge next to someone in your channel, or right-click them → Watch screen. Point at a still for a fresh one; **Hide** folds the shelf to one line in that channel. Up to 4 screens share one Screens window, side by side. Click one to make it large with the others in a strip below, and **Show all** to go back. Hover a screen for its name and × to stop watching it. Double-click or `F11` for full screen, `Esc` to leave it. Windows only for now |
| Poke | Right-click a user → Poke…, with an optional message. They get a card at the top right of the window, a sound and a flashing taskbar entry. More pokes stack in the same card, which has **Reply** and **Join** and stays until dismissed |
| Settings | Click your name bottom-left → Settings…, or `Ctrl+,`. **Voice** has devices, voice activation, and voice volume. **Sound cleanup** has noise suppression, echo cancellation, and automatic volume. **Notifications** has sounds and their own volume. **Screen sharing** controls quality. **Key bindings** has shortcuts and the hold-to-talk key. **About** shows the version and updates |

Open **Settings → Key bindings** and click an action's shortcut to change it.
Press and release the new shortcut. Use **Clear** to turn the shortcut off or
**Reset** to restore its default. T7 saves changes for the current profile.
Escape cancels recording by default. A main-window shortcut assigned to a
letter or other printable key leaves that key available while typing in a field.
The same shortcut can be used in different windows or forms when the actions
cannot fire together. Chat has separate bindings for prose and open code blocks.
Changing chat paste moves formatted text conversion and image attachments to
the new shortcut. Your system's text paste shortcut still inserts plain text.

A green ring around an avatar means that person is talking. On Windows the
push-to-talk key works while another app, like a game, has focus.

On Windows, click the globe next to a mute shortcut to make it work while
another app has focus or T7 is minimized. Both globes start off. Use a
combination with Ctrl or Alt, or a key like F6. T7 takes priority over shortcuts
in the focused app. If it can't register the shortcut, the row explains why.

Notification sounds play for connecting, people connecting to or leaving your
channel, switching channels, pokes, private messages and muting. The built-in
sounds are short chimes synthesized on the fly. Under Settings →
Notifications you can pick a sound pack instead:

- **Custom folder:** WAV files named `connected.wav`, `poke.wav`, … (hover the
  button for the full list) in `T7/sounds` in your config directory. Missing
  files fall back to the chimes.
- **`settings.ini` packs** already installed on the machine by other voice apps
  are listed automatically and read in place; T7 copies none of their files.
  Events a pack leaves empty stay silent. Text-to-speech packs aren't supported.

Nickname, recent servers, status and voice settings are remembered between sessions.

## Roles & permissions

Each server has ordered roles. A role grants yes-or-no permissions, and each
channel can change them for each role. The server works out what each person
may do, enforces it, and sends each client its own permissions. The client
shows only the menu entries those allow. The rules live in
`crates/proto/src/perms.rs`.

A new server has these roles, strongest first. Each role there also has the
permissions of the roles below it.

| Role | Who | Adds |
| --- | --- | --- |
| Owner | Whoever claimed the server | Everything |
| Admin | People the owner or an admin gave it | Manage channels, Manage channel permissions, Ban, Manage roles |
| Moderator | People given it, and a channel's listed moderators in that channel | Join full or locked channels, Move members, Mute members, Kick |
| Everyone | Every connection | View, Join, Speak, Send messages, Create temporary channels |

- **Claim the server:** on first start the server logs a claim token. In T7,
  open ⋯ next to the server's name, pick **Claim ownership…** and enter the token. The
  first person to use it becomes the owner, and the token stops working. If
  the owner loses their identity, stop the server and run
  `t7-server owner reset`. It prints a new claim token. The old owner stays
  the owner until someone redeems it.
- **Roles:** ⋯ next to the server's name → **Server settings…** → **Roles** lists the roles. There you
  create, rename and reorder roles, switch their permissions, and remove
  members, online or not. To give someone a role, right-click them and open
  **Roles**.
  You act only on roles, and on people, below your own highest role.
- **Channel permissions:** right-click a channel → **Edit channel…** →
  **Permissions**. Pick a role, then allow (check), deny (cross) or inherit
  (dash) each permission. Inherit takes the value from the channel above, and
  at the top from the role. The closest channel that sets a permission wins.
  On one channel, an allow for one of your roles beats a deny for another.
- **Channel moderators:** right-click someone in a channel → **Make moderator
  of** that channel. Listed moderators hold the Moderator role in that channel
  and the ones below it, and can change its name and topic. They have a star
  next to their name. The channel's edit dialog lists them, online or not.
- **Categories:** a category is a heading at the top level that folds when
  you click it. Nobody joins it. Its permissions apply to the channels in it,
  so a private category hides everything inside.
- **Hidden channels:** without View on a channel you don't see it, the
  channels below it, or the people in them. People who manage channels still
  see it, with an eye. Everyone always sees the default channel.
- **Lobbies:** joining a lobby opens a temporary room under it, named after
  you, and moves you in. Nobody stays in the lobby itself, and its rooms have
  its permissions. A new server has one, **Create a room**. To make one, pick
  **Lobby** in the new channel dialog. A lobby can't be at the third level,
  because its rooms go below it.
- **Room owners:** whoever makes a temporary channel owns it, and has a crown
  next to their name. In their room they kick, move and mute like a
  moderator, and edit or delete it. They rank just above Everyone, so
  moderators can still act on them. When the owner leaves, the person who has
  been in the room longest becomes its owner.
- **Speak:** the server drops the voice of anyone without Speak in their
  channel. They see "Can't speak here" and a greyed-out microphone button.
  Others see a grey crossed-out microphone next to them.
- **Server mute:** right-click someone → **Mute on server**. It needs Mute
  members in their channel and a role above theirs. The server drops their
  voice on every connection with their identity, across reconnects, until
  someone unmutes them. A server restart clears it. They see a locked
  microphone and "Muted on the server". Their own mute button keeps its
  setting.
- **Move:** drag someone onto a channel, or right-click them → **Move to**. It
  needs Move members where they are and in the new channel, and a role above
  theirs. They must be able to see the new channel. A full or locked channel
  also needs Join full or locked channels.
- **Kick:** **Kick from channel…** moves someone to the default channel, or,
  if that is full or has a password, to the first open channel with room.
  **Kick from server…** disconnects them, and they can connect again.
  Everyone sees who kicked whom and the reason in chat. Nobody can kick or ban
  themselves.
- **Ban:** **Ban…** bans someone for 1 hour, 1 day, 1 week or for good, and
  kicks them off the server. The ban covers their identity from any address.
  **Also ban IP address** (on by default) also kicks off and keeps out every
  other identity at their address, which includes everyone else behind it,
  like a household or a LAN. The server refuses an IP ban on your own
  address. A banned person who tries to connect sees the reason and how long
  the ban has left.
- **Identity:** on its first connect T7 makes an ed25519 key and keeps it as
  `identity_key` in its `settings.toml`. Every connection proves the key with
  a signature over that connection's TLS exporter value, so the proof can't
  be replayed on another connection or server. Roles, bans, mutes and
  moderator rights follow the key, not the nickname. Copy `identity_key` to be
  the same person on another computer. Without it, you are someone new.

## Server configuration

`t7-server.toml` (pass `-c path` to use another file, `-b addr:port` to override the bind address):

```toml
name = "T7 Server"
welcome = "Welcome!"
bind = "0.0.0.0:9988"
password = ""          # empty = public
max_clients = 128

[[channels]]
name = "Lobby"
default = true         # where new clients land

[[channels]]
name = "Gaming"
  [[channels.children]]
  name = "Squad 1"
  max_clients = 5

[[channels]]
name = "Create a room"
kind = "lobby"         # or "category" for a heading over the channels in it
```

The server keeps more files next to its config:

- `channels.toml`: the live channel tree. On first start it is copied from the
  config's `[[channels]]`; after that people who manage channels edit them in
  T7 and the server saves every change here. Temporary channels are never
  saved. Delete the file to start over from the config. A channel's
  `moderators` holds the identities (64 hex digits each) that moderate it,
  `overrides` what it allows and denies each role (role 0 is Everyone), and
  `kind` whether it is a `"category"` or a `"lobby"`. The config's
  `[[channels]]` take the same keys.
- `files/` holds completed uploads and their metadata. Keep it together with
  `channels.toml` when backing up or moving a server. Each channel has a
  generated `file_key` that keeps its uploads attached across renames, moves
  and restarts. Deleting a channel removes its uploads. Linked images stay
  on their original host.
- `max_file_size_bytes` in the server config sets the initial per-file limit.
  Saving a limit in Server settings writes `file-settings.toml`, which takes
  precedence on restart. Users can upload where they can post in their current
  channel, and browse channels they can see and join. A protected channel
  requires joining with its current password or the Bypass permission.
- `bans.toml`: the ban list, saved at every ban and unban. Expired bans are
  dropped when the server starts and when someone opens the ban list. To
  lift a ban by hand, stop the server and delete its `[[bans]]` entry.
  `next_id` keeps ban ids from being reused, so leave it in place.
- `roles.toml`: the roles in order with their permissions, who holds them
  with the nickname they last used, the owner, and the claim token until
  someone redeems it. Saved at every change. Edit it only while the server is
  stopped. Servers from before roles delete their `admin_token.txt` on first
  start and log a claim token instead. Server mutes are never saved.
- `tls_cert.der` and `tls_key.der`: the server's TLS certificate and private
  key, made on first start. See [Security](#security).

Limits: channel names up to 40 characters, topics up to 200, nesting up to 3
levels, 200 channels per server. The default channel can't be deleted; deleting
a channel moves the people inside to the default channel.

Set log verbosity with `RUST_LOG=debug`.

## Security

- **Control connection:** TLS 1.3 (rustls on ring), so nicknames, passwords,
  chat, claim tokens and voice keys never cross the network in the clear.
- **Voice and screens:** each client gets a random 32-byte key over TLS when
  it joins. Every voice and screen datagram, in both directions and pings
  included, is sealed with ChaCha20-Poly1305. The server opens each one with
  the sender's key and seals it again for each listener or viewer with
  theirs, and receivers drop replayed datagrams. This protects voice and
  shared screens on the network, not from the server: whoever runs the
  server can hear and see them. The network still sees who talks to the
  server, when and how much.
- **Server certificate:** on first start the server makes a self-signed
  certificate, keeps it in `tls_cert.der` and `tls_key.der` next to its
  config, and logs its SHA-256 fingerprint at every start:

  ```
  TLS certificate fingerprint (SHA-256): 3F:A1:…:9C
  ```

  `openssl x509 -inform der -in tls_cert.der -noout -fingerprint -sha256`
  prints the same. Keep `tls_key.der` private, and back both files up with
  the config when moving a server. Deleting them makes a new certificate.
- **Trust on first use:** the first time T7 connects to an address it pins the
  certificate's fingerprint (`server_certs` in its `settings.toml`). If the
  server later presents a different certificate, T7 stops before logging in
  and shows the saved and the new fingerprint. Check the new one against the
  server's log, or ask its admin, before choosing "Trust new certificate and
  connect". The very first connection is only as trustworthy as the network
  it runs over; compare the fingerprint with the admin if that matters to you,
  and before you enter a claim token.
- Clients from before screen sharing (protocol 6 and older) can't connect.
  The server tells them to update.

## Architecture

```
crates/
  proto/    wire protocol + shared client-side state model (ServerView)
  server/   t7-server: tokio, a single core task owning all state
  client/   t7-client: egui UI, CPU-rendered, + background network thread
```

- **Protocol:** length-prefixed [postcard](https://docs.rs/postcard) frames over TLS 1.3 on TCP.
  Compact varint binary, so most messages are a handful of bytes. Versioned handshake.
- **Voice:** 48 kHz mono Opus (pure-Rust `opus1`) in 20 ms frames at 32 kbps, sent
  as UDP datagrams on the same port, each encrypted with the sender's or the
  listener's key (see [Security](#security)). The server forwards frames to
  everyone else in the speaker's channel without decoding the audio, skips
  listeners with sound off, and rate-limits senders. Clients keep a ~60 ms jitter
  buffer per speaker. Each packet carries a low-bitrate copy of the previous frame
  (Opus in-band FEC), so a single lost packet is rebuilt; longer gaps are concealed.
- **Screen sharing:** H.264 frames travel over the voice link, at most 1162
  bytes of a frame per datagram and 512 datagrams per frame. The sharer
  spreads a frame's datagrams over 16 ms. The server forwards them without
  decoding to those in the sharer's channel who chose to watch, and drops a
  sharer's video past 12 Mbit/s or while they may not speak; once one part of
  a frame is over the limit, the rest of that frame goes too. A frame still
  missing parts 100 ms after its latest one is lost. A viewer that starts
  watching, or loses a frame, asks the sharer for a keyframe through the
  server every 250 ms until one arrives, since a still screen sends nothing
  until asked. A frame that reaches the sharer's network thread over 100 ms
  after it was encoded is dropped, along with the frames after it, and the
  sharer encodes a keyframe at once. The server passes on at most one request per sharer
  every 250 ms. A viewer watches up to 4 screens at once. Changing channels
  ends your share and everything you watch. Each watched screen decodes on a
  thread of its own. All of them draw in one Screens window, which redraws only
  the screen that has a new picture, so the main window doesn't repaint for
  video. For the shelf's stills, a client asks the server for a preview: the
  server asks the sharer for a keyframe and forwards that one keyframe to
  whoever asked, up to 8 asks at once and one more per second after that; in
  testing that keyframe was 10 to 41 KB at 1080p. The client decodes it once
  on a thread of its own, scaled to the pixels the shelf draws it at, then
  drops the decoder. Stills come when you join a channel, when someone starts
  sharing, and when you point at one, never on a timer.
- **Sound cleanup** (`crates/client/src/dsp.rs`), on the microphone, 10 ms at a time:
  high-pass filter and WebRTC AEC3 echo cancellation (pure-Rust `sonora`), then
  RNNoise noise suppression (`nnnoiseless`), then WebRTC AGC2 adaptive gain. Voice
  activation opens only for frames RNNoise rates as speech *and* above the level
  threshold, so keyboards and fans don't open the mic; the 40 ms before an onset
  are sent too, so first syllables survive. In an offline comparison on synthetic
  scenes, RNNoise removed ~20 dB more fan noise than WebRTC's own suppressor while
  distorting speech less, and AEC3 + RNNoise removed ~69 dB of speaker echo. The
  chain costs ~0.12 ms per 10 ms of audio (about 1% of one core).
- **Server:** each connection gets a reader loop and a writer task. All state lives
  in one core task (no locks, a total order of events). Broadcasts are serialised
  once and shared (`Arc<[u8]>`) across recipients. Writers batch queued frames into
  one flush. Slow clients whose 1024-frame queue overflows are dropped instead of
  stalling everyone. Chat has a per-client rate limit, and handshakes and idle
  connections have timeouts.
- **Client:** immediate-mode egui UI drawn on the CPU (winit + softbuffer + a
  software rasterizer). No GPU driver is loaded: an empty OpenGL window alone
  commits ~66 MB, more than eight times this whole client. The UI redraws only on
  input or network events, and a full redraw takes ~1.0 ms (median, 1100×720, the
  `frame_time` test), so it
  sits at ~0% CPU when idle. Networking runs on its own thread with a
  current-thread tokio runtime. Icons are painted vectors (no image assets).
  Settings live in `T7/settings.toml` in the user's config directory.

Measured on Windows 11 (release build, median of 5 runs, other apps running).
On a call, one other person talks the whole time:

| | Binary | Private memory (Task Manager) | Committed | CPU |
| --- | --- | --- | --- | --- |
| t7-server, on a call | 3.1 MB | 1.2 MB | 1.9 MB (8.1 MB working set) | ~0.2% of one core |
| t7-client, not connected | 10.7 MB | 6.2 MB | 7.1 MB | 0% |
| t7-client, connected and alone | | 9.7 MB | 11.1 MB | ~2% of one core |
| t7-client, on a call | | 9.9 MB | 11.1 MB | ~4% of one core |

## Development

```sh
cargo nextest run --workspace                # unit + end-to-end server tests
cargo nextest run -p t7-server               # just the crate you changed
cargo run --example bot -- 127.0.0.1:9988    # fill a server with demo users
cargo run --example bot -- 127.0.0.1:9988 500   # crude load test
cargo test -p t7-client snapshots -- --ignored  # every screen to PNG, in target/snapshots
cargo test --release -p t7-client frame_time -- --ignored --nocapture   # time a full redraw
```

Tests run with [cargo-nextest](https://nexte.st) (`cargo install cargo-nextest --locked`),
which runs the tests of every crate at once instead of one crate after another.
`cargo test --workspace` works too, just slower. nextest skips doctests; run
`cargo test --workspace --doc` for those.

## Updates and releases

Release builds update themselves. Shortly after start and every 6 hours T7
reads `latest.json` from the latest GitHub release; if it lists a newer version
for the platform, T7 downloads it next to itself and accepts it only if the
SHA-256 matches and the ed25519 signature verifies against the key built into
the client. A banner then says so above your name, bottom-left. The new build
replaces the old one when T7 closes, or right away when you click Restart.
When a server says the client is too old, T7 checks immediately. Local builds
(without `T7_RELEASE` set) never update themselves.

To publish a release:

```sh
# 1. bump `version` in Cargo.toml, commit
git tag vX.Y.Z && git push origin main vX.Y.Z
```

GitHub Actions (`.github/workflows/release.yml`) then builds Windows, Linux and
macOS, signs each build with the `T7_SIGNING_KEY` secret and publishes the
release with `latest.json`. The optional `T7_DEFAULT_SERVER` repository
variable sets the address prefilled for first-time users.

After the Release workflow succeeds, **Deploy released server** advances
`deploy/t7-server` to the commit tagged by the latest stable GitHub release.
Coolify watches pushes to that branch through a signed GitHub webhook and
redeploys the existing server with its `/data` volume. To catch up after a
missed run, run **Deploy released server** manually. If the branch already
points to the release and the Coolify build failed, retry that deployment in Coolify.

`cargo run -p t7-release -- keygen FILE` makes a new signing key. Its public
half goes in `crates/client/src/update.rs`; builds signed with a different key
are refused, so changing keys means one manual download for everyone.

## License

[MIT](LICENSE).

T7 is an independent project, not affiliated with or endorsed by any other
voice-chat vendor. Product names mentioned in the code, such as the install
folders it searches for sound packs, are trademarks of their owners.
