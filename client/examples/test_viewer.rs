//! Headless viewer: joins a room, watches the first video stream and decodes it
//! with the production [`StreamDecoder`] (codec sniffing, hardware-first
//! pipeline with software fallback). No window, no GPU renderer: it checks the
//! network, crypto and decode half of a cross-platform run and prints what the
//! tile tooltip would show.
//!
//! ```text
//! cargo run --release -p simple-screensharing-client --example test_viewer -- \
//!     <join-link> [--name viewer] [--seconds 15] [--from NAME] [--dump frame.ppm]
//! ```
//!
//! Prints one line per second (decoder element, codec, decoded fps, received
//! kbit/s) and, with `--dump`, writes the last decoded frame as a binary PPM.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use simple_screensharing_client::decode::StreamDecoder;
use simple_screensharing_client::net::{self, ClientEvent};
use simple_screensharing_client::protocol::{Member, PresenceEvent, StreamKind};

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let mut args = std::env::args().skip(1);
    let join = args.next().context("usage: test_viewer <join-link> [--name N] [--seconds N] [--from NAME] [--dump FILE]")?;
    let (mut name, mut seconds, mut dump, mut from) = ("test-viewer".to_string(), 15u64, None::<String>, None::<String>);
    while let Some(flag) = args.next() {
        let mut value = || args.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--name" => name = value()?,
            "--seconds" => seconds = value()?.parse()?,
            "--dump" => dump = Some(value()?),
            "--from" => from = Some(value()?),
            other => bail!("unknown argument {other}"),
        }
    }

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let endpoint = net::build_client_endpoint()?;
        let (client, snapshot, mut events) = net::ScreenshareClient::connect(&endpoint, &join, &name).await.context("connect to the relay")?;
        tracing::info!("connected as conn {:?}; {} member(s), {} stream(s) in the room", client.conn_id(), snapshot.members.len(), snapshot.streams.len());

        // The stream to watch: one already in the snapshot, else the first to start.
        // `--from NAME` only takes streams whose owner joined under that display
        // name (a killed sender can leave a stale stream in the room for a while).
        let owner_ok = |owner: u64, members: &[Member]| match &from {
            None => true,
            Some(want) => members.iter().any(|m| m.conn_id == owner && &m.display_name == want),
        };
        let mut members = snapshot.members.clone();
        let mut target = snapshot.streams.iter().find(|s| s.kind == StreamKind::Video && owner_ok(s.owner, &members)).cloned();
        while target.is_none() {
            match tokio::time::timeout(Duration::from_secs(60), events.recv()).await {
                Ok(Some(ClientEvent::PresenceDelta(PresenceEvent::MemberJoined(m)))) => members.push(m),
                Ok(Some(ClientEvent::PresenceDelta(PresenceEvent::StreamStarted(info)))) if info.kind == StreamKind::Video && owner_ok(info.owner, &members) => target = Some(info),
                Ok(Some(_)) => {}
                _ => bail!("no video stream appeared within 60 s"),
            }
        }
        let target = target.unwrap();
        tracing::info!("watching owner {} stream {} ({}x{}, label {:?})", target.owner, target.stream_id, target.width, target.height, target.label);
        client.subscribe(target.owner, target.stream_id).await.context("subscribe")?;

        let mut decoder = StreamDecoder::new(target.stream_id)?;
        let started = Instant::now();
        let mut last_report = Instant::now();
        let mut media = 0u64;
        let mut last_frame = None;
        while started.elapsed() < Duration::from_secs(seconds) {
            match tokio::time::timeout(Duration::from_millis(20), events.recv()).await {
                Ok(Some(ClientEvent::MediaFrame(f))) if f.conn_id == target.owner && f.stream_id == target.stream_id => {
                    media += 1;
                    decoder.feed(&f);
                }
                Ok(Some(ClientEvent::ControlClosed(why))) => bail!("control closed: {why}"),
                Ok(None) => bail!("event channel closed"),
                _ => {}
            }
            if let Some(img) = decoder.latest_frame() {
                last_frame = Some(img);
            }
            if last_report.elapsed() >= Duration::from_secs(1) {
                last_report = Instant::now();
                let st = decoder.stats();
                println!(
                    "t={:>4.1}s decoder={:?} codec={:?} hw={} media_frames={} decoded_fps={:.1} rx_kbps={:.0}",
                    started.elapsed().as_secs_f64(),
                    st.info.decoder_name,
                    st.info.codec,
                    st.info.hardware,
                    media,
                    decoder.decoded_fps(),
                    decoder.received_bitrate_kbps(),
                );
            }
        }
        if let (Some(path), Some(img)) = (dump, last_frame) {
            let [w, h] = img.size;
            let mut out = std::io::BufWriter::new(std::fs::File::create(&path)?);
            write!(out, "P6\n{w} {h}\n255\n")?;
            for p in &img.pixels {
                out.write_all(&[p.r(), p.g(), p.b()])?;
            }
            println!("wrote {path} ({w}x{h})");
        }
        client.leave_and_close().await;
        Ok(())
    })
}
