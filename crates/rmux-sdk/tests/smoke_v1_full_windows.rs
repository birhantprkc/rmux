#![cfg(windows)]

mod common;

use common::windows_smoke::{
    cmd_echo_text, cmd_interactive_command, cmd_long_running_command, session_name,
    wait_for_output_marker, wait_for_pane_absent, wait_for_snapshot_text_after_revision,
    wait_for_stable_snapshot, Harness, TestResult, LIVE_DAEMON_LOCK, OUTPUT_BUDGET,
};
use rmux_sdk::{
    EnsureSession, EnsureSessionPolicy, PaneExitState, PaneOutputStart, ProcessSpec, RmuxError,
    SplitDirection,
};

#[tokio::test]
async fn rust_app_autostarts_and_drives_a_session_windows() -> TestResult {
    let _lock = LIVE_DAEMON_LOCK.lock().await;
    let harness = Harness::start("rustapp").await?;
    let rmux = harness.rmux();
    let session = rmux
        .ensure_session(
            EnsureSession::named(session_name("sdkwinrustapp"))
                .create_only()
                .detached(true)
                .command(cmd_interactive_command()),
        )
        .await?;

    // The captured `SplitDirectionSpec::Vertical` semantic was a stacked
    // split — keep that meaning under the new `SplitDirection { Right,
    // Left, Up, Down }` API.
    let pane = session.pane(0, 0).split(SplitDirection::Down).await?;
    let marker = "RMUX_FULL_WINDOWS_RUST_APP_OK";
    let mut output = pane.output_stream_starting_at(PaneOutputStart::Now).await?;
    pane.send_text(cmd_echo_text(marker)).await?;
    wait_for_output_marker(&mut output, marker.as_bytes()).await?;
    drop(output);
    pane.wait_for_text(marker).await?;
    assert!(pane.snapshot().await?.visible_text().contains(marker));

    harness.finish().await
}

#[tokio::test]
async fn ci_runner_collects_command_output_and_exit_windows() -> TestResult {
    let _lock = LIVE_DAEMON_LOCK.lock().await;
    let harness = Harness::start("collect").await?;
    let rmux = harness.rmux();
    let _keeper = keepalive_session(rmux, "sdkwincollectkeep").await?;
    let session = rmux
        .ensure_session(
            EnsureSession::named(session_name("sdkwincollect"))
                .create_only()
                .detached(true)
                .command(cmd_interactive_command()),
        )
        .await?;
    let pane = session.pane(0, 0);
    let pane_for_collect = pane.clone();
    let collect = tokio::spawn(async move {
        pane_for_collect
            .collect_output_until_exit_starting_at(PaneOutputStart::Now, OUTPUT_BUDGET)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    pane.send_text(cmd_echo_text("hello from rmux")).await?;
    pane.wait_for_text("hello from rmux").await?;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    pane.send_text("exit 0\r").await?;
    let collected = collect.await??;

    assert!(
        String::from_utf8_lossy(&collected.bytes).contains("hello from rmux"),
        "collected transcript did not contain command output: {:?}",
        collected.bytes
    );
    if let Some(code) = exit_code(collected.exit_state.as_ref()) {
        assert_eq!(code, 0, "expected exit code 0");
    } else {
        wait_for_pane_absent(&pane).await?;
    }
    assert!(!collected.truncated);

    harness.finish().await
}

#[tokio::test]
async fn interactive_cmd_waits_for_prompt_and_interrupts_windows() -> TestResult {
    let _lock = LIVE_DAEMON_LOCK.lock().await;
    let harness = Harness::start("interrupt").await?;
    let rmux = harness.rmux();
    let session = rmux
        .ensure_session(
            EnsureSession::named(session_name("sdkwininterrupt"))
                .create_only()
                .detached(true)
                .command(cmd_interactive_command()),
        )
        .await?;
    let pane = session.pane(0, 0);
    pane.send_text(cmd_echo_text("ready")).await?;
    pane.wait_for_text("ready").await?;
    let started = "interrupt-command-started";
    pane.send_text(cmd_long_running_command(started)).await?;
    pane.wait_for_text(started).await?;
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    pane.send_key("C-c").await?;
    pane.send_text(cmd_echo_text("interrupted")).await?;
    pane.wait_for_text("interrupted").await?;

    harness.finish().await
}

#[tokio::test]
async fn dashboard_snapshot_updates_are_revision_gated_windows() -> TestResult {
    let _lock = LIVE_DAEMON_LOCK.lock().await;
    let harness = Harness::start("dashboard").await?;
    let rmux = harness.rmux();
    let session = rmux
        .ensure_session(
            EnsureSession::named(session_name("skdwindash"))
                .create_only()
                .detached(true)
                .command(cmd_interactive_command()),
        )
        .await?;
    let pane = session.pane(0, 0);
    let baseline = pane.snapshot().await?;
    let marker = "RMUX_FULL_WINDOWS_DASHBOARD";

    pane.send_text(cmd_echo_text(marker)).await?;
    let changed = wait_for_snapshot_text_after_revision(&pane, baseline.revision, marker).await?;
    assert!(changed.revision > baseline.revision);
    assert_ne!(changed.visible_text(), baseline.visible_text());

    let changed = wait_for_stable_snapshot(&pane, changed.revision).await?;
    let idle = pane.snapshot().await?;
    assert_eq!(
        idle.revision, changed.revision,
        "snapshot revision advanced without a visible pane transition"
    );
    assert_eq!(idle.visible_text(), changed.visible_text());

    harness.finish().await
}

#[tokio::test]
async fn failure_cleanup_uses_existing_typed_diagnostics_windows() -> TestResult {
    let _lock = LIVE_DAEMON_LOCK.lock().await;
    let mut harness = Harness::start("failure").await?;
    let rmux = harness.rmux();
    let _keeper = keepalive_session(rmux, "sdkwinfailurekeep").await?;
    let session = rmux
        .ensure_session(
            EnsureSession::named(session_name("sdkwinfailure"))
                .create_only()
                .detached(true)
                .command(cmd_interactive_command()),
        )
        .await?;
    let pane = session.pane(0, 0);
    assert!(session.kill().await?);

    let stale_error = pane
        .send_text("after kill")
        .await
        .expect_err("stale pane send_text must fail");
    assert!(
        matches!(
            stale_error,
            RmuxError::Protocol { .. } | RmuxError::Transport { .. }
        ),
        "expected existing protocol/transport diagnostic for stale pane, got {stale_error:?}"
    );

    let rmux = harness.take_rmux()?;
    rmux.shutdown().await?;
    let transport_error = pane
        .info()
        .await
        .expect_err("pane info after daemon shutdown must fail");
    assert!(
        matches!(transport_error, RmuxError::Transport { .. }),
        "expected transport diagnostic after daemon shutdown, got {transport_error:?}"
    );
    harness.disarm_after_shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn warm_reconnect_keeps_existing_runtime_windows() -> TestResult {
    let _lock = LIVE_DAEMON_LOCK.lock().await;
    let harness = Harness::start("warm").await?;
    let rmux = harness.rmux();
    let session_name = session_name("sdkwinwarm");
    rmux.ensure_session(
        EnsureSession::named(session_name.clone())
            .policy(EnsureSessionPolicy::CreateOrReuse)
            .detached(true)
            .process(ProcessSpec {
                command: Some(cmd_interactive_command()),
                environment: None,
            }),
    )
    .await?;

    let warm = common::windows_smoke::builder(harness.pipe_name())
        .connect_or_start()
        .await?;
    assert!(warm.list_sessions().await?.contains(&session_name));
    assert!(warm.session(session_name).await?.exists().await?);
    drop(warm);

    harness.finish().await
}

fn exit_code(exit: Option<&PaneExitState>) -> Option<i32> {
    exit.and_then(|state| state.code)
}

async fn keepalive_session(
    rmux: &rmux_sdk::Rmux,
    name: &str,
) -> rmux_sdk::Result<rmux_sdk::Session> {
    rmux.ensure_session(
        EnsureSession::named(session_name(name))
            .create_only()
            .detached(true)
            .command(cmd_interactive_command()),
    )
    .await
}
