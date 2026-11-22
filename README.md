<img src="assets/icon.svg" width="64" height="64" alt="">

# T7

A lightweight, fast voice-chat server and client:
one server you host yourself, a tree of channels, and people in them. No accounts,
no cloud, no bloat.

> **Status:** voice works. Server, channel tree, presence, voice chat per channel
> (voice activation or push-to-talk), text chat (server, channel, private), and
> mute / sound off / away.

## Quick start

```sh
cargo build --release

# Server: writes t7-server.toml with defaults on first run.
./target/release/t7-server

# Client: open the GUI and press Ctrl+S (or "Connect").
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
| Connect | `Ctrl+S`, the Connect button, or click a recent server |
| Switch channel | Double-click a channel, drag yourself onto it, or right-click → Join |
| Private message | Double-click a user, or right-click → Send private message |
| Close a PM tab | Middle-click it, or right-click → Close |
| Mute mic / sound off | `Ctrl+Shift+M` / `Ctrl+Shift+D`, or the buttons bottom-right |
| Voice settings | The sliders button bottom-right: microphone, speakers, voice activation level with a live meter, push-to-talk key, sound cleanup (noise suppression, echo cancellation, automatic volume), notification sounds and sound pack, volumes |
| Poke | Right-click a user → Poke…, with an optional message. They get a popup, a sound and a flashing taskbar entry |

A green ring around an avatar means that person is talking. On Windows the
push-to-talk key works while another app, like a game, has focus.

Notification sounds play for connecting, people connecting to or leaving your
channel, switching channels, pokes, private messages and muting. The built-in
sounds are short chimes synthesized on the fly. Under Voice settings →
Notifications you can pick a sound pack instead:

- **Custom folder:** WAV files named `connected.wav`, `poke.wav`, … (hover the
  button for the full list) in `T7/sounds` in your config directory. Missing
  files fall back to the chimes.
- **`settings.ini` packs** already installed on the machine by other voice apps
  are listed automatically and read in place; T7 copies none of their files.
  Events a pack leaves empty stay silent. Text-to-speech packs aren't supported.

Nickname, recent servers, status and voice settings are remembered between sessions.

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
```

Set log verbosity with `RUST_LOG=debug`.

## Architecture

```
crates/
  proto/    wire protocol + shared client-side state model (ServerView)
  server/   t7-server: tokio, a single core task owning all state
  client/   t7-client: egui UI, CPU-rendered, + background network thread
```

- **Protocol:** length-prefixed [postcard](https://docs.rs/postcard) frames over TCP.
  Compact varint binary, so most messages are a handful of bytes. Versioned handshake.
- **Voice:** 48 kHz mono Opus (pure-Rust `opus1`) in 20 ms frames at 32 kbps, sent
  as UDP datagrams on the same port. Each datagram carries the sender's id and a
  random token from the handshake. The server forwards frames to everyone else in
  the speaker's channel without decoding them, skips listeners with sound off, and
  rate-limits senders. Clients keep a ~60 ms jitter buffer per speaker. Each packet
  carries a low-bitrate copy of the previous frame (Opus in-band FEC), so a single
  lost packet is rebuilt; longer gaps are concealed.
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
  input or network events, and a full redraw takes ~1.2 ms (median, 1100×720), so it
  sits at ~0% CPU when idle. Networking runs on its own thread with a
  current-thread tokio runtime. Icons are painted vectors (no image assets).
  Settings live in `T7/settings.toml` in the user's config directory.

Measured on Windows (release build, idle and connected, median of 5 runs):

| | Binary | Private memory (Task Manager) | Committed | Idle CPU |
| --- | --- | --- | --- | --- |
| t7-server | 1.4 MB | — | 7.5 MB working set | 0% |
| t7-client, not connected | 7.1 MB | 7.2 MB | 8.1 MB | 0% |
| t7-client, on a call | | 9.4 MB | 10.5 MB | ~1–3% of one core |

## Development

```sh
cargo test --workspace                       # unit + end-to-end server tests
cargo run --example bot -- 127.0.0.1:9988    # fill a server with demo users
cargo run --example bot -- 127.0.0.1:9988 500   # crude load test
```

## Roadmap

1. Voice polish: per-user volume.
2. Channel management at runtime: create/edit/delete, temporary channels, passwords.
3. Permissions: server admin token, kick/ban, channel moderators.
4. TLS for the control connection plus encrypted voice.
5. Server-side persistence of channels made at runtime.
