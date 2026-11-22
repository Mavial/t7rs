//! Connects scripted clients to a server: handy for demos and load tests.
//!
//!     cargo run --example bot -- 127.0.0.1:9988 [COUNT]
//!
//! With COUNT > 6 the bots get numbered names and just idle (load test).

use std::time::Duration;

use t7_proto::*;
use tokio::io::BufReader;
use tokio::net::TcpStream;

const SCRIPT: &[(&str, ChannelId, ClientStatus, Option<&str>)] = &[
    ("Mara", 1, ClientStatus { muted: false, deafened: false, away: false }, Some("hey all, anyone up for a round later?")),
    ("Jonas", 1, ClientStatus { muted: true, deafened: false, away: false }, Some("sure, after dinner")),
    ("Kenji", 4, ClientStatus { muted: false, deafened: false, away: false }, None),
    ("Priya", 4, ClientStatus { muted: false, deafened: true, away: false }, None),
    ("Ola", 2, ClientStatus { muted: false, deafened: false, away: false }, None),
    ("Dmitri", 6, ClientStatus { muted: true, deafened: true, away: true }, None),
];

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| format!("127.0.0.1:{DEFAULT_PORT}"));
    let count: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(SCRIPT.len());

    let mut tasks = Vec::new();
    for i in 0..count {
        let addr = addr.clone();
        tasks.push(tokio::spawn(async move {
            let (nick, channel, status, line) = match SCRIPT.get(i) {
                Some(&(n, c, s, l)) if count <= SCRIPT.len() => (n.to_owned(), c, s, l),
                _ => (format!("bot{i}"), 1, ClientStatus::default(), None),
            };
            if let Err(e) = run(&addr, nick, channel, status, line, i as u64).await {
                eprintln!("bot {i}: {e}");
            }
        }));
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    println!("{count} bots connected; Ctrl+C to stop");
    for t in tasks {
        let _ = t.await;
    }
}

async fn run(
    addr: &str,
    nickname: String,
    channel: ChannelId,
    status: ClientStatus,
    line: Option<&str>,
    seq: u64,
) -> Result<(), FrameError> {
    let (tls, _) = tls::connect(TcpStream::connect(addr).await?, None).await.map_err(std::io::Error::other)?;
    let (r, mut w) = tokio::io::split(tls);
    let mut r = BufReader::new(r);
    write_frame(&mut w, &C2S::Hello { protocol: PROTOCOL_VERSION, nickname, password: None }).await?;
    write_frame(&mut w, &C2S::JoinChannel { channel, password: None }).await?;
    write_frame(&mut w, &C2S::SetStatus(status)).await?;
    if let Some(text) = line {
        tokio::time::sleep(Duration::from_millis(400 + seq * 500)).await;
        write_frame(&mut w, &C2S::SendText { target: TextTarget::Channel, text: text.into() }).await?;
    }

    // Drain incoming traffic and keep the connection alive.
    let reader = tokio::spawn(async move {
        let mut buf = Vec::new();
        while read_frame::<_, S2C>(&mut r, &mut buf).await.is_ok() {}
    });
    let mut nonce = 0;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(PING_INTERVAL_SECS)) => {
                nonce += 1;
                write_frame(&mut w, &C2S::Ping { nonce }).await?;
            }
            _ = tokio::signal::ctrl_c() => {
                let _ = write_frame(&mut w, &C2S::Goodbye).await;
                reader.abort();
                return Ok(());
            }
        }
    }
}
