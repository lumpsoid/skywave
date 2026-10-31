//! A standalone front for the `skywave-testkit` link harness, so a phone or
//! emulator can reach `skywaved` through a bad link.
//!
//! ```text
//! skywave-linkproxy <listen> <upstream> <control-fifo>
//! ```
//!
//! Listens on `<listen>` (a fixed address, so a paired device keeps working
//! across proxy restarts), forwards each connection into a `LinkProxy` that
//! pipes it to `<upstream>`. Commands are read line by line from
//! `<control-fifo>` (create it with `mkfifo`; write with `echo ... > fifo`):
//!
//! - `clean`: no latency, cap or drops
//! - `profile latency=800 jitter=1200 bw=8000 drop-bytes=20000-80000 drop-time=2000-8000`
//!   (every key optional; milliseconds, bytes/s, bytes, milliseconds)
//! - `cut`, `down`, `blackhole`, `up`: see `LinkProxy`
//! - `stats`: print the counters
//!
//! Every applied command and its result goes to stdout.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use skywave_testkit::{LinkProfile, LinkProxy};
use tokio::io::{AsyncBufReadExt, BufReader, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: skywave-linkproxy <listen> <upstream> <control-fifo>");
        std::process::exit(2);
    }
    let listen: SocketAddr = args[1].parse().expect("listen address");
    let upstream: SocketAddr = args[2].parse().expect("upstream address");
    let control = args[3].clone();

    let seed = skywave_testkit::seed();
    let proxy = Arc::new(LinkProxy::start(upstream, LinkProfile::clean(), seed).await);
    let inner = proxy.addr();
    let listener = TcpListener::bind(listen)
        .await
        .expect("bind listen address");
    say(&format!(
        "listening on {listen} -> harness {inner} -> {upstream} (seed {seed})"
    ));

    tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                if let Ok(mut out) = TcpStream::connect(inner).await {
                    let _ = copy_bidirectional(&mut inbound, &mut out).await;
                }
            });
        }
    });

    loop {
        let file = match tokio::fs::File::open(&control).await {
            Ok(f) => f,
            Err(e) => {
                eprintln!("cannot open {control}: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let mut lines = BufReader::new(file).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            apply(&proxy, line.trim());
        }
    }
}

fn apply(proxy: &LinkProxy, line: &str) {
    let mut words = line.split_whitespace();
    match words.next() {
        None => {}
        Some("clean") => {
            proxy.set_profile(LinkProfile::clean());
            say("profile clean");
        }
        Some("profile") => match parse_profile(words) {
            Ok(p) => {
                say(&format!("profile {p:?}"));
                proxy.set_profile(p);
            }
            Err(e) => say(&format!("bad profile: {e}")),
        },
        Some("cut") => {
            proxy.cut();
            say("cut");
        }
        Some("down") => {
            proxy.down();
            say("down");
        }
        Some("blackhole") => {
            proxy.blackhole();
            say("blackhole");
        }
        Some("up") => {
            proxy.up();
            say("up");
        }
        Some("stats") => say(&format!("{:?}", proxy.stats())),
        Some(other) => say(&format!("unknown command {other:?}")),
    }
}

fn parse_profile<'a>(words: impl Iterator<Item = &'a str>) -> Result<LinkProfile, String> {
    let ms = Duration::from_millis;
    let mut p = LinkProfile::clean();
    for word in words {
        let (key, value) = word
            .split_once('=')
            .ok_or(format!("{word}: want key=value"))?;
        let num = |s: &str| s.parse::<u64>().map_err(|e| format!("{word}: {e}"));
        let range = |s: &str| -> Result<(u64, u64), String> {
            let (lo, hi) = s.split_once('-').unwrap_or((s, s));
            Ok((num(lo)?, num(hi)?))
        };
        p = match key {
            "latency" => p.latency(ms(num(value)?)),
            "jitter" => p.jitter(ms(num(value)?)),
            "bw" => p.bandwidth(num(value)?),
            "drop-bytes" => {
                let (lo, hi) = range(value)?;
                p.drop_after_bytes(lo..=hi)
            }
            "drop-time" => {
                let (lo, hi) = range(value)?;
                p.drop_after(ms(lo)..=ms(hi))
            }
            _ => return Err(format!("unknown key {key}")),
        };
    }
    Ok(p)
}

fn say(msg: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    println!("[{}.{:03}] {msg}", now.as_secs(), now.subsec_millis());
}
