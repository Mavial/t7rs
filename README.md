<img src="assets/icon.svg" width="64" height="64" alt="">

# T7

A lightweight, fast voice-chat server and client:
one server you host yourself, a tree of channels, and people in them. No accounts,
no cloud, no bloat.

> **Status:** milestone 1. Server, channel tree, presence, text chat (server,
> channel, private), and self-status (mute / sound off / away). **Voice is the
> next milestone.** The protocol and UI are already built around it.

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

The default port is **9987/TCP**. When connecting you can leave the port off.

## Using the client

| Action | How |
| --- | --- |
| Connect | `Ctrl+S`, the Connect button, or click a recent server |
| Switch channel | Double-click a channel, drag yourself onto it, or right-click → Join |
| Private message | Double-click a user, or right-click → Send private message |
| Close a PM tab | Middle-click it, or right-click → Close |
| Mute mic / sound off | `Ctrl+Shift+M` / `Ctrl+Shift+D`, or the buttons bottom-right |

Nickname, recent servers and your status are remembered between sessions.

## Server configuration

`t7-server.toml` (pass `-c path` to use another file, `-b addr:port` to override the bind address):

```toml
name = "T7 Server"
welcome = "Welcome!"
bind = "0.0.0.0:9987"
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
  client/   t7-client: egui/eframe UI + background network thread
```

- **Protocol:** length-prefixed [postcard](https://docs.rs/postcard) frames over TCP.
  Compact varint binary, so most messages are a handful of bytes. Versioned handshake.
- **Server:** each connection gets a reader loop and a writer task. All state lives
  in one core task (no locks, a total order of events). Broadcasts are serialised
  once and shared (`Arc<[u8]>`) across recipients. Writers batch queued frames into
  one flush. Slow clients whose 1024-frame queue overflows are dropped instead of
  stalling everyone. Chat has a per-client rate limit, and handshakes and idle
  connections have timeouts.
- **Client:** immediate-mode UI that redraws only on input or network events, so it
  sits at ~0% CPU when idle. Networking runs on its own thread with a
  current-thread tokio runtime. Icons are painted vectors (no image assets).

Measured on Windows (release build, 6 clients connected):

| | Binary | RAM | Idle CPU |
| --- | --- | --- | --- |
| t7-server | 1.4 MB | 7.5 MB | 0% |
| t7-client | 6.5 MB | 77 MB | 0% |

## Development

```sh
cargo test --workspace                       # unit + end-to-end server tests
cargo run --example bot -- 127.0.0.1:9987    # fill a server with demo users
cargo run --example bot -- 127.0.0.1:9987 500   # crude load test
```

## Roadmap

1. **Voice:** Opus over UDP (session token from the TCP handshake), push-to-talk
   and voice activation, per-user volume, talking indicators in the tree.
2. Channel management at runtime: create/edit/delete, temporary channels, passwords.
3. Permissions: server admin token, kick/ban, channel moderators.
4. TLS for the control connection plus encrypted voice.
5. Server-side persistence of channels made at runtime.
