//! Voice test bot.
//!
//!     cargo run --example voicebot -- ADDR tone   [SECONDS]   # speak a quiet two-note tone
//!     cargo run --example voicebot -- ADDR listen [SECONDS]   # report voice frames heard
//!     cargo run --example voicebot -- ADDR poke NICK [MESSAGE]  # poke someone
//!     cargo run --example voicebot -- ADDR admin TOKEN [SECONDS]  # become admin and stay
//!     cargo run --example voicebot -- ADDR temp NAME PASSWORD [SECONDS]  # make a temporary channel and stay
//!
//! Joins the default channel. `listen` prints, per speaker, how many frames
//! arrived and how loud they decode, so a real client's capture → encode →
//! send path can be checked without ears.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use t7_proto::voice::{self, FRAME_SAMPLES, ToClient, ToServer};
use t7_proto::{
    C2S, ChannelSettings, ClientId, DEFAULT_PORT, PROTOCOL_VERSION, PasswordChange, S2C, identity, read_frame, tls,
    write_frame,
};
use tokio::io::BufReader;
use tokio::net::{TcpStream, UdpSocket};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| format!("127.0.0.1:{DEFAULT_PORT}"));
    let mode = args.next().unwrap_or_else(|| "tone".into());
    let rest: Vec<String> = args.collect();
    let secs: u64 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(5);

    let tcp = TcpStream::connect(&addr).await.expect("connect");
    let server: SocketAddr = tcp.peer_addr().unwrap();
    let (tls, fingerprint) = tls::connect(tcp, None).await.expect("TLS handshake");
    let version = tls.get_ref().1.protocol_version().expect("handshake is done");
    println!("{version:?} to {server}, server certificate {fingerprint}");
    // A new identity each run.
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).expect("system randomness");
    let identity = identity::IdentityKey::from_seed(seed).prove(&tls::exporter(tls.get_ref().1));
    let (r, mut w) = tokio::io::split(tls);
    let mut r = BufReader::new(r);
    let nickname = match mode.as_str() {
        "tone" => "ToneBot",
        "poke" => "PokeBot",
        "admin" => "AdminBot",
        "temp" => "RaidLead",
        _ => "ListenBot",
    }
    .to_owned();
    write_frame(&mut w, &C2S::Hello { protocol: PROTOCOL_VERSION, nickname, password: None, identity }).await.unwrap();
    let mut buf = Vec::new();
    let (mut crypto, names) = match read_frame::<_, S2C>(&mut r, &mut buf).await.unwrap() {
        S2C::Welcome { your_id, voice_key, clients, .. } => {
            let names: HashMap<ClientId, String> = clients.into_iter().map(|c| (c.id, c.nickname)).collect();
            (voice::ClientSide::new(your_id, &voice_key), names)
        }
        other => panic!("not welcomed: {other:?}"),
    };
    if mode == "admin" || mode == "temp" {
        let first = rest.first().cloned().unwrap_or_default();
        let msg = if mode == "admin" {
            C2S::Authenticate { token: first }
        } else {
            let password = rest.get(1).cloned().unwrap_or_default();
            let settings = ChannelSettings {
                name: first,
                topic: "Tonight at 9".into(),
                max_clients: Some(4),
                password: if password.is_empty() { PasswordChange::Keep } else { PasswordChange::Set(password) },
            };
            C2S::CreateChannel { parent: None, settings, temporary: true }
        };
        write_frame(&mut w, &msg).await.unwrap();
        let stay: u64 = rest.last().and_then(|s| s.parse().ok()).unwrap_or(10);
        let _ = tokio::time::timeout(Duration::from_secs(stay), async {
            while let Ok(m) = read_frame::<_, S2C>(&mut r, &mut buf).await {
                if let S2C::Error { message } = m {
                    println!("server said: {message}");
                }
            }
        })
        .await;
        return;
    }
    if mode == "poke" {
        let target_name = rest.first().cloned().unwrap_or_default();
        let message = rest.get(1).cloned().unwrap_or_default();
        let Some((&target, _)) = names.iter().find(|(_, n)| **n == target_name) else {
            return println!("nobody called {target_name:?} is online");
        };
        write_frame(&mut w, &C2S::Poke { target, message }).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match read_frame::<_, S2C>(&mut r, &mut buf).await {
                    Ok(S2C::Error { message }) => return Some(message),
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        })
        .await;
        match reply {
            Ok(Some(err)) => println!("poke refused: {err}"),
            _ => println!("poked {target_name}"),
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        return;
    }
    // Keep the control connection drained.
    tokio::spawn(async move { while read_frame::<_, S2C>(&mut r, &mut buf).await.is_ok() {} });

    let udp = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    udp.connect(server).await.unwrap();
    let mut out = Vec::new();
    crypto.seal(ToServer::Ping { nonce: 1 }, &mut out);
    udp.send(&out).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(secs);
    if mode == "tone" {
        let mut enc =
            opus1::OpusEncoder::new(opus1::SampleRate::Hz48000, opus1::Channels::Mono, opus1::Application::Voip)
                .unwrap();
        let mut pkt = [0u8; voice::MAX_OPUS_PAYLOAD];
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let mut seq = 0u32;
        while Instant::now() < deadline {
            tick.tick().await;
            // Alternate two notes every half second, quietly.
            let hz = if (seq / 25).is_multiple_of(2) { 440.0 } else { 554.0 };
            let pcm: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|i| {
                    let t = (seq as usize * FRAME_SAMPLES + i) as f32 / 48_000.0;
                    (std::f32::consts::TAU * hz * t).sin() * 0.08
                })
                .collect();
            let n = enc.encode(&pcm, FRAME_SAMPLES, &mut pkt).unwrap();
            crypto.seal(ToServer::Audio { seq, payload: &pkt[..n] }, &mut out);
            udp.send(&out).await.unwrap();
            seq += 1;
            if seq.is_multiple_of(50) {
                crypto.seal(ToServer::Ping { nonce: u64::from(seq) }, &mut out);
                udp.send(&out).await.unwrap();
            }
        }
        println!("sent {seq} frames ({:.1} s of audio)", seq as f32 / 50.0);
    } else {
        let mut decoders: HashMap<ClientId, opus1::OpusDecoder> = HashMap::new();
        let mut stats: HashMap<ClientId, (u32, f64)> = HashMap::new();
        let (mut pongs, mut unreadable) = (0, 0);
        let mut dbuf = [0u8; 1500];
        let mut pcm = [0f32; FRAME_SAMPLES];
        let mut ping = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline.into()) => break,
                _ = ping.tick() => {
                    crypto.seal(ToServer::Ping { nonce: 2 }, &mut out);
                    udp.send(&out).await.unwrap();
                }
                res = udp.recv(&mut dbuf) => {
                    let Ok(n) = res else { continue };
                    match crypto.open(&mut dbuf[..n]) {
                        Some(ToClient::Audio { from, payload, .. }) => {
                            let dec = decoders.entry(from).or_insert_with(|| {
                                opus1::OpusDecoder::new(opus1::SampleRate::Hz48000, opus1::Channels::Mono).unwrap()
                            });
                            let samples = dec.decode(payload, FRAME_SAMPLES, &mut pcm).unwrap_or(0);
                            let energy: f64 = pcm[..samples].iter().map(|&v| f64::from(v).powi(2)).sum();
                            let e = stats.entry(from).or_default();
                            e.0 += 1;
                            e.1 += energy / samples.max(1) as f64;
                        }
                        Some(ToClient::Pong { .. }) => pongs += 1,
                        None => unreadable += 1,
                    }
                }
            }
        }
        println!("voice pongs: {pongs}, datagrams that failed to decrypt: {unreadable}");
        if stats.is_empty() {
            println!("heard nobody");
        }
        for (from, (frames, energy)) in stats {
            let name = names.get(&from).cloned().unwrap_or_else(|| format!("client {from}"));
            let db = 10.0 * (energy / f64::from(frames)).max(1e-12).log10();
            println!("heard {name}: {frames} frames ({:.1} s), mean level {db:.1} dBFS", frames as f32 / 50.0);
        }
    }
}
