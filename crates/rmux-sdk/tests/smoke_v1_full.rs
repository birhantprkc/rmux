#![cfg(unix)]

use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use rmux_sdk::{
    bootstrap::discovery::SDK_DAEMON_BINARY_ENV, EnsureSession, EnsureSessionPolicy, PaneExitState,
    PaneOutputChunk, PaneOutputStart, PaneOutputStream, ProcessSpec, Rmux, RmuxBuilder, RmuxError,
    SessionName, SplitDirectionSpec,
};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout, Instant};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const ROOT_PREFIX: &str = "rmux-sdk-v1-full-";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const OUTPUT_BUDGET: usize = 64 * 1024;

static LIVE_DAEMON_LOCK: Mutex<()> = Mutex::const_new(());
static UNIQUE_ID: AtomicUsize = AtomicUsize::new(0);

#[tokio::test]
async fn rust_app_autostarts_and_drives_a_session() -> TestResult {
    let _lock = LIVE_DAEMON_LOCK.lock().await;
    let harness = Harness::start("rust-app").await?;
    let rmux = harness.rmux();
    let session = rmux
        .ensure_session(
            EnsureSession::named(session_name("sdkfullrustapp"))
                .create_only()
                .detached(true),
        )
        .await?;
    assert!(session.exists().await?);

    let split = session
        .window(0)
        .split(SplitDirectionSpec::Vertical)
        .await?;
    let pane = session.pane(split.window_index, split.pane_index);
    let marker = "RMUX_FULL_RUST_APP_OK";
    let mut output = pane.output_stream_starting_at(PaneOutputStart::Now).await?;
    pane.send_text(format!("printf '{marker}\\n'\n")).await?;
    wait_for_output_marker(&mut output, marker.as_bytes()).await?;
    drop(output);
    pane.wait_for_text(marker).await?;
    assert!(
        pane.snapshot().await?.visible_text().contains(marker),
        "driven pane did not expose marker in the rendered snapshot"
    );

    harness.finish().await
}

#[tokio::test]
async fn ci_runner_collects_command_output_and_exit() -> TestResult {
    let _lock = LIVE_DAEMON_LOCK.lock().await;
    let harness = Harness::start("ci-collect").await?;
    let rmux = harness.rmux();
    let _keeper = keepalive_session(rmux, "sdkfullcollectkeep").await?;
    let session = rmux
        .ensure_session(
            EnsureSession::named(session_name("sdkfullcollect"))
                .create_only()
                .detached(true),
        )
        .await?;
    let pane = session.pane(0, 0);
    let collector_pane = pane.clone();
    let collector = tokio::spawn(async move {
        collector_pane
            .collect_output_until_exit_starting_at(PaneOutputStart::Now, OUTPUT_BUDGET)
            .await
    });
    sleep(Duration::from_millis(100)).await;
    pane.send_text("printf 'hello from rmux\\n'; exit 0\n")
        .await?;
    let collected = collector.await??;

    assert!(
        String::from_utf8_lossy(&collected.bytes).contains("hello from rmux"),
        "collected transcript did not contain command output: {:?}",
        collected.bytes
    );
    match exit_code(collected.exit_state.as_ref()) {
        Some(0) => {}
        Some(code) => return Err(format!("expected exit code 0, got {code}").into()),
        None => wait_for_pane_absent(&pane).await?,
    }
    assert!(!collected.truncated);

    harness.finish().await
}

#[tokio::test]
async fn interactive_repl_waits_for_prompt_and_interrupts() -> TestResult {
    let _lock = LIVE_DAEMON_LOCK.lock().await;
    let Some(python) = python3() else {
        eprintln!("python3 not found; scenario deferred by docs/feature-inventory-v1.yaml");
        return Ok(());
    };

    let harness = Harness::start("python-repl").await?;
    let rmux = harness.rmux();
    let _keeper = keepalive_session(rmux, "sdkfullpykeep").await?;
    let script = "\
import signal
import sys
import time

def stop(*_):
    print('interrupted', flush=True)
    sys.exit(130)

signal.signal(signal.SIGINT, stop)
print('ready', flush=True)
while True:
    time.sleep(1)
";
    let session = rmux
        .ensure_session(
            EnsureSession::named(session_name("sdkfullpy"))
                .create_only()
                .detached(true)
                .process(ProcessSpec {
                    command: Some(vec![python, "-c".to_owned(), script.to_owned()]),
                    environment: None,
                }),
        )
        .await?;
    let pane = session.pane(0, 0);
    pane.wait_for_text("ready").await?;
    let mut output = pane.output_stream_starting_at(PaneOutputStart::Now).await?;
    pane.send_key("C-c").await?;
    wait_for_output_marker(&mut output, b"interrupted").await?;
    drop(output);

    match pane.wait_exit().await? {
        Some(exit) => assert!(
            exit.code == Some(130) || exit.signal == Some(2),
            "expected SIGINT-style Python termination, got {exit:?}"
        ),
        None => wait_for_pane_absent(&pane).await?,
    }

    harness.finish().await
}

#[tokio::test]
async fn dashboard_snapshot_updates_are_revision_gated() -> TestResult {
    let _lock = LIVE_DAEMON_LOCK.lock().await;
    let harness = Harness::start("dashboard").await?;
    let rmux = harness.rmux();
    let session = rmux
        .ensure_session(
            EnsureSession::named(session_name("sdkfulldash"))
                .create_only()
                .detached(true),
        )
        .await?;
    let pane = session.pane(0, 0);
    let baseline = pane.snapshot().await?;
    let marker = "RMUX_FULL_DASHBOARD_REDRAW";

    pane.send_text(format!("printf '{marker}\\n'\n")).await?;
    let changed = wait_for_snapshot_text_after_revision(&pane, baseline.revision, marker).await?;
    assert!(changed.revision > baseline.revision);
    assert_ne!(changed.visible_text(), baseline.visible_text());

    sleep(Duration::from_millis(120)).await;
    let idle = pane.snapshot().await?;
    assert_eq!(
        idle.revision, changed.revision,
        "snapshot revision advanced without a visible pane transition"
    );
    assert_eq!(idle.visible_text(), changed.visible_text());

    harness.finish().await
}

#[tokio::test]
async fn failure_cleanup_uses_existing_typed_diagnostics() -> TestResult {
    let _lock = LIVE_DAEMON_LOCK.lock().await;
    let mut harness = Harness::start("failure-cleanup").await?;
    let rmux = harness.rmux();
    let _keeper = keepalive_session(rmux, "sdkfullfailurekeep").await?;
    let session_name = session_name("sdkfullfailure");
    let session = rmux
        .ensure_session(
            EnsureSession::named(session_name.clone())
                .create_only()
                .detached(true),
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

    let socket_path = harness.socket_path().to_path_buf();
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
    wait_for_path_absent(&socket_path).await?;
    harness.disarm_after_shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn warm_reconnect_keeps_existing_runtime() -> TestResult {
    let _lock = LIVE_DAEMON_LOCK.lock().await;
    let harness = Harness::start("warm-reconnect").await?;
    let rmux = harness.rmux();
    let session_name = session_name("sdkfullwarm");
    rmux.ensure_session(
        EnsureSession::named(session_name.clone())
            .policy(EnsureSessionPolicy::CreateOrReuse)
            .detached(true),
    )
    .await?;
    let original_pid = wait_for_daemon_pid(harness.socket_path()).await?;

    let warm = RmuxBuilder::new()
        .unix_socket(harness.socket_path())
        .default_timeout(DEFAULT_TIMEOUT)
        .connect_or_start()
        .await?;
    assert_eq!(
        wait_for_daemon_pid(harness.socket_path()).await?,
        original_pid
    );
    assert!(warm.list_sessions().await?.contains(&session_name));
    assert!(warm.session(session_name).await?.exists().await?);
    drop(warm);

    harness.finish().await
}

fn session_name(value: &str) -> SessionName {
    SessionName::new(value).expect("valid smoke session name")
}

fn exit_code(exit: Option<&PaneExitState>) -> Option<i32> {
    exit.and_then(|state| state.code)
}

fn python3() -> Option<String> {
    Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .filter(std::process::ExitStatus::success)
        .map(|_| "python3".to_owned())
}

async fn keepalive_session(rmux: &Rmux, name: &str) -> rmux_sdk::Result<rmux_sdk::Session> {
    rmux.ensure_session(
        EnsureSession::named(session_name(name))
            .create_only()
            .detached(true),
    )
    .await
}

async fn wait_for_output_marker(stream: &mut PaneOutputStream, marker: &[u8]) -> TestResult {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("pane output stream did not emit expected marker".into());
        }
        match timeout(remaining, stream.next()).await?? {
            Some(PaneOutputChunk::Bytes { bytes, .. })
                if bytes.windows(marker.len()).any(|window| window == marker) =>
            {
                return Ok(());
            }
            Some(_) => {}
            None => return Err("pane output stream closed before expected marker".into()),
        }
    }
}

async fn wait_for_snapshot_text_after_revision(
    pane: &rmux_sdk::Pane,
    previous_revision: u64,
    marker: &str,
) -> TestResult<rmux_sdk::PaneSnapshot> {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
        let snapshot = pane.snapshot().await?;
        if snapshot.revision > previous_revision && snapshot.visible_text().contains(marker) {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "snapshot did not advance past revision {previous_revision} with marker {marker:?}"
            )
            .into());
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_path_absent(path: &Path) -> TestResult {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
        if !path.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("path remained after shutdown: {}", path.display()).into());
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_pane_absent(pane: &rmux_sdk::Pane) -> TestResult {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
        if pane.id().await?.is_none() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("pane remained listed after expected process exit".into());
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_daemon_pid(socket_path: &Path) -> TestResult<u32> {
    let needle = socket_path.to_string_lossy().into_owned();
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
        if let Some(pid) = daemon_pid_for_socket(&needle)? {
            return Ok(pid);
        }
        if Instant::now() >= deadline {
            return Err(format!("daemon pid for {} was not visible", socket_path.display()).into());
        }
        sleep(Duration::from_millis(25)).await;
    }
}

fn daemon_pid_for_socket(socket_needle: &str) -> TestResult<Option<u32>> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if !line.contains("--__internal-daemon") || !line.contains(socket_needle) {
            continue;
        }
        let Some(pid) = line.split_whitespace().next() else {
            continue;
        };
        if let Ok(pid) = pid.parse::<u32>() {
            return Ok(Some(pid));
        }
    }
    Ok(None)
}

struct Harness {
    root: PathBuf,
    socket_path: PathBuf,
    rmux: Option<Rmux>,
    armed: bool,
}

impl Harness {
    async fn start(label: &str) -> TestResult<Self> {
        let root = smoke_root(label)?;
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(&root)?;
        let socket_path = root.join("daemon.sock");
        let daemon_binary = rmux_binary()?.to_path_buf();
        let _daemon_binary_env = EnvGuard::set(SDK_DAEMON_BINARY_ENV, daemon_binary.as_os_str());
        let rmux = RmuxBuilder::new()
            .unix_socket(&socket_path)
            .default_timeout(DEFAULT_TIMEOUT)
            .connect_or_start()
            .await?;
        assert_socket(&socket_path)?;
        Ok(Self {
            root,
            socket_path,
            rmux: Some(rmux),
            armed: true,
        })
    }

    fn rmux(&self) -> &Rmux {
        self.rmux.as_ref().expect("harness rmux is available")
    }

    fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn take_rmux(&mut self) -> TestResult<Rmux> {
        self.rmux
            .take()
            .ok_or_else(|| "harness rmux was already taken".into())
    }

    async fn finish(mut self) -> TestResult {
        let socket_path = self.socket_path.clone();
        if let Some(rmux) = self.rmux.take() {
            rmux.shutdown().await?;
            wait_for_path_absent(&socket_path).await?;
        }
        fs::remove_dir_all(&self.root)?;
        self.armed = false;
        Ok(())
    }

    async fn disarm_after_shutdown(mut self) -> TestResult {
        wait_for_path_absent(&self.socket_path).await?;
        fs::remove_dir_all(&self.root)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.socket_path.exists() {
            let _ = Command::new(rmux_binary().unwrap_or_else(|_| Path::new("rmux")))
                .arg("-S")
                .arg(&self.socket_path)
                .arg("kill-server")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_socket(path: &Path) -> TestResult {
    let metadata = fs::symlink_metadata(path)?;
    assert!(
        metadata.file_type().is_socket(),
        "{} exists but is not a Unix socket",
        path.display()
    );
    Ok(())
}

fn smoke_root(label: &str) -> TestResult<PathBuf> {
    let id = UNIQUE_ID.fetch_add(1, Ordering::Relaxed);
    let root = PathBuf::from(format!(
        "/tmp/{ROOT_PREFIX}{}-{id}-{label}",
        std::process::id()
    ));
    if !is_tmp_smoke_root(&root) {
        return Err(format!("invalid smoke root {}", root.display()).into());
    }
    Ok(root)
}

fn is_tmp_smoke_root(root: &Path) -> bool {
    if !root.is_absolute() || !root.starts_with(Path::new("/tmp")) {
        return false;
    }
    if root
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return false;
    }

    match root.file_name().and_then(|name| name.to_str()) {
        Some(name) => name.starts_with(ROOT_PREFIX) && name.len() > ROOT_PREFIX.len(),
        None => false,
    }
}

fn rmux_binary() -> TestResult<&'static Path> {
    static RMUX_BINARY: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    match RMUX_BINARY.get_or_init(|| resolve_rmux_binary().map_err(|error| error.to_string())) {
        Ok(path) => Ok(path.as_path()),
        Err(error) => Err(std::io::Error::other(error.clone()).into()),
    }
}

fn resolve_rmux_binary() -> TestResult<PathBuf> {
    if let Some(path) = option_env!("CARGO_BIN_EXE_rmux") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }

    let target_dir = target_dir()?;
    let candidate = target_dir.join("debug").join("rmux");
    if candidate.is_file() {
        return Ok(candidate);
    }

    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .arg("build")
        .arg("--bin")
        .arg("rmux")
        .arg("--locked")
        .arg("--manifest-path")
        .arg(workspace_root().join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()?;
    if !status.success() {
        return Err(format!("failed to build rmux binary for full SDK smoke: {status}").into());
    }
    if !candidate.is_file() {
        return Err(format!(
            "rmux binary build succeeded but '{}' was not created",
            candidate.display()
        )
        .into());
    }

    Ok(candidate)
}

fn target_dir() -> TestResult<PathBuf> {
    if let Some(target_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        return Ok(PathBuf::from(target_dir));
    }

    let current = std::env::current_exe()?;
    current
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| "test executable is not under a target directory".into())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("rmux-sdk manifest lives under crates/rmux-sdk")
        .to_path_buf()
}

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}
