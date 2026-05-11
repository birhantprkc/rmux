//! Pane-events stream example: consume the live pane-output byte stream
//! and decoded line stream produced by `Pane::output_stream` /
//! `Pane::line_stream`.
//!
//! Compile-tested by `cargo build --workspace --examples` and
//! `cargo clippy --workspace --all-targets --locked`. Running it requires
//! a reachable RMUX daemon. The example demonstrates the two-stream split:
//! raw bytes preserve every payload byte the daemon delivered, while the
//! line stream layers lossy UTF-8 rendering and partial-line buffering on
//! top of the same subscription.
//!
//! Uses only types re-exported from `rmux_sdk`. Does not depend on
//! `rmux-client`, `rmux-core`, `rmux-server`, or `rmux-pty`.

use std::time::Duration;

use rmux_sdk::{
    EnsureSession, PaneLineItem, PaneOutputChunk, PaneOutputStart, ProcessSpec, Result, Rmux,
    TerminalSizeSpec,
};

const MAX_BYTE_CHUNKS: usize = 8;
const MAX_LINES: usize = 16;
// Hard upper bound on stream events the example will consume before
// returning. Non-byte events (lag notices, future non-exhaustive
// variants) advance this counter so a daemon stuck emitting only lag
// notices cannot keep the example blocked indefinitely.
const MAX_STREAM_EVENTS: usize = 64;

#[tokio::main]
async fn main() -> Result<()> {
    let rmux = Rmux::builder()
        .default_endpoint()
        .default_timeout(Duration::from_secs(30))
        .build();

    let session = rmux
        .ensure_session(
            EnsureSession::try_named("rmux-sdk-pane-events-stream")?
                .create_or_reuse()
                .size(TerminalSizeSpec::new(80, 24))
                .process(ProcessSpec {
                    command: Some(vec![
                        "sh".to_owned(),
                        "-c".to_owned(),
                        "for i in 1 2 3 4; do printf 'tick %d\\n' \"$i\"; done".to_owned(),
                    ]),
                    environment: None,
                }),
        )
        .await?;

    let pane = session.pane(0, 0);

    // The raw byte stream is anchored at `Now` so it only delivers bytes
    // appended after subscription. `Oldest` would replay the daemon's
    // retained backlog before live output.
    let mut bytes = pane.output_stream_starting_at(PaneOutputStart::Now).await?;
    let mut total_bytes = 0_usize;
    let mut byte_chunks = 0_usize;
    let mut byte_events = 0_usize;
    while byte_chunks < MAX_BYTE_CHUNKS && byte_events < MAX_STREAM_EVENTS {
        byte_events += 1;
        match bytes.next().await? {
            Some(PaneOutputChunk::Bytes { sequence, bytes }) => {
                total_bytes += bytes.len();
                byte_chunks += 1;
                println!("byte chunk seq={} len={}", sequence, bytes.len());
            }
            Some(PaneOutputChunk::Lag(notice)) => {
                println!(
                    "raw lag: missed={} resume_seq={}",
                    notice.missed_events, notice.resume_sequence,
                );
            }
            Some(_) => {
                // `PaneOutputChunk` is `#[non_exhaustive]`; future
                // variants are surfaced as opaque events here.
                println!("byte stream: future chunk variant observed");
            }
            None => break,
        }
    }
    drop(bytes);
    println!("raw stream collected {total_bytes} bytes across {byte_chunks} chunks");

    // The line stream wraps a fresh raw subscription and yields decoded
    // UTF-8 lines split on `\n`. Invalid byte sequences are replaced with
    // the Unicode replacement character; partial trailing lines stay
    // buffered until the next LF arrives.
    let mut lines = pane.line_stream().await?;
    let mut delivered = 0_usize;
    let mut line_events = 0_usize;
    while delivered < MAX_LINES && line_events < MAX_STREAM_EVENTS {
        line_events += 1;
        match lines.next().await? {
            Some(PaneLineItem::Line { text }) => {
                delivered += 1;
                println!("line: {text}");
            }
            Some(PaneLineItem::Lag(notice)) => {
                println!(
                    "line lag: missed={} resume_seq={}",
                    notice.missed_events, notice.resume_sequence,
                );
            }
            Some(_) => {
                // `PaneLineItem` is `#[non_exhaustive]`; future
                // variants are surfaced as opaque events here.
                println!("line stream: future item variant observed");
            }
            None => break,
        }
    }

    Ok(())
}
