//! Shared boilerplate for `sdk_demo_*.rs` examples.
//!
//! Each SDK demo scenario only wants to show 2–4 SDK lines, but the
//! capture pipeline needs a deterministic shell (no rc-files, fixed prompt,
//! fixed locale) so the recorded ANSI is reproducible. Pulling that
//! plumbing out of the per-scenario file keeps the source code that ships
//! in the SDK demo UI honest: it shows the SDK call you'd actually write,
//! not the capture scaffolding.
//!
//! This file is brought into each example via `#[path]`. `#[allow(dead_code)]`
//! because individual examples only consume a subset of the helpers.

#![allow(dead_code)]

use std::time::Duration;

use rmux_sdk::{EnsureSession, ProcessSpec, Rmux, Session, TerminalSizeSpec};

#[cfg(unix)]
fn shell_command() -> [&'static str; 3] {
    ["bash", "-c", "printf 'BANNER\\n'; exec bash --noprofile --norc -i"]
}

#[cfg(windows)]
fn shell_command() -> [&'static str; 3] {
    ["cmd", "/Q", "/K echo BANNER"]
}

/// Colored prompt rmux's daemon paints in freshly-spawned panes. Painting
/// the same prompt from the example keeps every captured frame visually
/// identical regardless of which side spawned the shell.
const PROMPT: &str =
    "\\033[36muser@rmuxio\\033[0m:\\033[32m~/workspace\\033[0m$ ";

/// Connects to (or starts) the daemon and returns a deterministic session
/// whose pane 0 has finished spawning its shell.
///
/// The shell is interactive but the screen is still blank — call
/// [`paint_idle_prompt`] or one of the `paint_*` helpers to draw the
/// scenario's demo frame.
pub(crate) async fn demo_session(name: &str) -> rmux_sdk::Result<(Rmux, Session)> {
    let rmux = Rmux::builder()
        .default_timeout(Duration::from_secs(5))
        .connect_or_start()
        .await?;
    let session = rmux
        .ensure_session(
            EnsureSession::try_named(name)?
                .create_or_reuse()
                .detached(true)
                .size(TerminalSizeSpec::new(80, 24))
                .process(ProcessSpec {
                    command: Some(shell_command().into_iter().map(String::from).collect()),
                    environment: Some(vec![
                        "LC_ALL=C.UTF-8".to_owned(),
                        "TZ=UTC".to_owned(),
                        "TERM=xterm-256color".to_owned(),
                    ]),
                }),
        )
        .await?;
    let pane = session.pane(0, 0);
    pane.wait_for_text("BANNER").await?;
    Ok((rmux, session))
}

/// Paints the colored rmux prompt with no command, then `read -r _` to keep
/// the prompt visible. Use this before scenarios that visually need a
/// prompt on screen but don't run a command themselves (e.g. split).
pub(crate) async fn paint_idle_prompt(session: &Session) -> rmux_sdk::Result<()> {
    let pane = session.pane(0, 0);
    let paint = format!("clear; printf '{PROMPT}'; read -r _");
    pane.send_text(&paint).await?;
    pane.send_key("Enter").await?;
    Ok(())
}

/// Paints a one-shot `echo hello` demonstration frame on the demo pane.
///
/// Draws the entire `prompt > echo hello / hello / prompt >` sequence in
/// one shot, then blocks the shell on `read -r _` so the captured fixture
/// is reproducible.
pub(crate) async fn paint_echo_hello(session: &Session) -> rmux_sdk::Result<()> {
    paint_command_run(session, "echo hello", "hello").await
}

/// Paints a one-shot `uname -s` demonstration frame on the demo pane.
pub(crate) async fn paint_uname(session: &Session) -> rmux_sdk::Result<()> {
    paint_command_run(session, "uname -s", "Linux").await
}

async fn paint_command_run(
    session: &Session,
    command: &str,
    expected_output: &str,
) -> rmux_sdk::Result<()> {
    let pane = session.pane(0, 0);
    let cmd = format!(
        "clear; \
         printf '{PROMPT}{command}\\n'; \
         {command}; \
         printf '{PROMPT}'; \
         read -r _"
    );
    pane.send_text(&cmd).await?;
    pane.send_key("Enter").await?;
    pane.wait_for_text(expected_output).await?;
    Ok(())
}

/// Kills the session unless `RMUX_SDK_DEMO_KEEP_SESSION` is set in the
/// environment — handy when iterating locally with `rmux attach`.
pub(crate) async fn cleanup(session: Session) -> rmux_sdk::Result<()> {
    if std::env::var_os("RMUX_SDK_DEMO_KEEP_SESSION").is_none() {
        let _ = session.kill().await?;
    }
    Ok(())
}

/// Creates a throwaway companion session that the scenario can safely
/// consume (kill, close, etc.) without disturbing the demo session that
/// the fixture pipeline is recording.
pub(crate) async fn throwaway_session(rmux: &Rmux, name: &str) -> rmux_sdk::Result<Session> {
    rmux.ensure_session(
        EnsureSession::try_named(name)?
            .create_or_reuse()
            .detached(true)
            .size(TerminalSizeSpec::new(80, 24))
            .process(ProcessSpec {
                command: Some(shell_command().into_iter().map(String::from).collect()),
                environment: Some(vec!["LC_ALL=C.UTF-8".to_owned()]),
            }),
    )
    .await
}
