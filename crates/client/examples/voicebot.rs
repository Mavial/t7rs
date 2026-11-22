//! Voice test bot.
//!
//!     cargo run --example voicebot -- ADDR tone   [SECONDS]   # speak a quiet two-note tone
//!     cargo run --example voicebot -- ADDR listen [SECONDS]   # report voice frames heard
//!
//! Joins the default channel. `listen` prints, per speaker, how many frames
//! arrived and how loud they decode, so a real client's capture → encode →
//! send path can be checked without ears.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use t7_proto::voice::{self, FRAME_SAMPLES, ToClient, ToServer};
use t7_proto::*;
use tokio::io::BufReader;
use tokio::net::{TcpStream, UdpSocket};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| format!("127.0.0.1:{DEFAULT_PORT}"));
    let mode = args.next().unwrap_or_else(|| "tone".into());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);

    let tcp = TcpStream::connect(&addr).await.expect("connect");
    let server: SocketAddr = tcp.peer_addr().unwrap();
    let (r, mut w) = tcp.into_split();
    let mut r = BufReader::new(r);
    let nickname = if mode == "tone" { "ToneBot" } else { "ListenBot" }.to_owned();
    write_frame(&mut w, &C2S::Hello { protocol: PROTOCOL_VERSION, nickname, password: None }).await.unwrap();
    let mut buf = Vec::new();
    let (id, token, names) = match read_frame::<_, S2C>(&mut r, &mut buf).await.unwrap() {
        S2C::Welcome { your_id, voice_token, clients, .. } => {
            let names: HashMap<ClientId, String> = clients.into_iter().map(|c| (c.id, c.nickname)).collect();
            (your_id, voice_token, names)
        }
        other => panic!("not welcomed: {other:?}"),
    };
    // Keep the control connection drained.
    tokio::spawn(async move { while read_frame::<_, S2C>(&mut r, &mut buf).await.is_ok() {} });

    let udp = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    udp.connect(server).await.unwrap();
    let mut out = Vec::new();
    voice::encode_to_server(id, token, ToServer::Ping { nonce: 1 }, &mut out);
    udp.send(&out).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(secs);
    if mode == "tone" {
        let mut enc =
            opus1::OpusEncoder::new(opus1::SampleRate::Hz48000, opus1::Channels::Mono, opus1::Application::Voip).unwrap();
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
            voice::encode_to_server(id, token, ToServer::Audio { seq, payload: &pkt[..n] }, &mut out);
            udp.send(&out).await.unwrap();
            seq += 1;
            if seq.is_multiple_of(50) {
                voice::encode_to_server(id, token, ToServer::Ping { nonce: seq as u64 }, &mut out);
                udp.send(&out).await.unwrap();
            }
        }
        println!("sent {seq} frames ({:.1} s of audio)", seq as f32 / 50.0);
    } else {
        let mut decoders: HashMap<ClientId, opus1::OpusDecoder> = HashMap::new();
        let mut stats: HashMap<ClientId, (u32, f64)> = HashMap::new();
        let mut pongs = 0;
        let mut dbuf = [0u8; 1500];
        let mut pcm = [0f32; FRAME_SAMPLES];
        let mut ping = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline.into()) => break,
                _ = ping.tick() => {
                    voice::encode_to_server(id, token, ToServer::Ping { nonce: 2 }, &mut out);
                    udp.send(&out).await.unwrap();
                }
                res = udp.recv(&mut dbuf) => {
                    let Ok(n) = res else { continue };
                    match voice::parse_to_client(&dbuf[..n]) {
                        Some(ToClient::Audio { from, payload, .. }) => {
                            let dec = decoders.entry(from).or_insert_with(|| {
                                opus1::OpusDecoder::new(opus1::SampleRate::Hz48000, opus1::Channels::Mono).unwrap()
                            });
                            let samples = dec.decode(payload, FRAME_SAMPLES, &mut pcm).unwrap_or(0);
                            let energy: f64 = pcm[..samples].iter().map(|&v| (v as f64).powi(2)).sum();
                            let e = stats.entry(from).or_default();
                            e.0 += 1;
                            e.1 += energy / samples.max(1) as f64;
                        }
                        Some(ToClient::Pong { .. }) => pongs += 1,
                        None => {}
                    }
                }
            }
        }
        println!("voice pongs: {pongs}");
        if stats.is_empty() {
            println!("heard nobody");
        }
        for (from, (frames, energy)) in stats {
            let name = names.get(&from).cloned().unwrap_or_else(|| format!("client {from}"));
            let db = 10.0 * (energy / frames as f64).max(1e-12).log10();
            println!("heard {name}: {frames} frames ({:.1} s), mean level {db:.1} dBFS", frames as f32 / 50.0);
        }
    }
}
