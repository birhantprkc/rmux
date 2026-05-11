#![cfg(unix)]

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use rmux_proto::{encode_frame, FrameDecoder, HasSessionRequest, Request, Response};
use rmux_sdk::{
    EnsureSession, PaneCloseOutcome, PaneInfo, PaneProcessState, PaneRespawnOptions, ProcessSpec,
    RmuxBuilder, SessionName, SplitDirectionSpec,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::time::Instant;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

static LIVE_DAEMON_LOCK: Mutex<()> = Mutex::const_new(());
static UNIQUE_ID: AtomicUsize = AtomicUsize::new(0);

#[tokio::test]
async fn pane_close_and_respawn_preserve_slot_semantics() -> TestResult {
    let _lock = LIVE_DAEMON_LOCK.lock().await;
    let harness = Harness::start("pane-lifecycle").await?;
    let rmux = harness.rmux();
    let alpha = session_name("sdkpanelife");
    let session = EnsureSession::named(alpha.clone())
        .create_only()
        .ensure(&rmux)
        .await?;
    let pane = session.pane(0, 0);
    let target = pane.target().clone();

    pane.clone().detach();
    assert!(pane.exists().await?, "detach must leave the pane alive");

    let before = only_pane_info(&pane.info().await?);
    let pane_id = before.id;
    let marker = "RMUX_SDK_OLD_MARKER";
    pane.send_text(format!("printf '{marker}\\n'\n")).await?;
    wait_for_visible_text(&pane, marker).await?;

    let active_error = pane
        .respawn(PaneRespawnOptions::default())
        .await
        .expect_err("active respawn without -k must fail");
    assert!(
        active_error.to_string().contains("still active"),
        "unexpected active respawn error: {active_error}"
    );

    let respawned = pane
        .respawn(PaneRespawnOptions {
            kill: true,
            start_directory: None,
            process: ProcessSpec {
                command: Some(vec!["cat >/dev/null".to_owned()]),
                environment: None,
            },
        })
        .await?;
    assert_eq!(respawned, target);

    let after = wait_for_generation(&pane, before.generation).await?;
    assert_eq!(after.id, pane_id);
    assert!(after.generation > before.generation);
    assert!(
        !pane.snapshot().await?.visible_text().contains(marker),
        "respawn must clear old visible parser state"
    );

    let stubborn_target = session
        .window(0)
        .split(SplitDirectionSpec::Vertical)
        .await?;
    let stubborn_pane = session.pane(stubborn_target.window_index, stubborn_target.pane_index);
    let stubborn_before = only_pane_info(&stubborn_pane.info().await?);
    stubborn_pane
        .respawn(PaneRespawnOptions {
            kill: true,
            start_directory: None,
            process: ProcessSpec {
                command: Some(vec!["trap '' HUP; while :; do sleep 1; done".to_owned()]),
                environment: None,
            },
        })
        .await?;
    let stubborn_after = wait_for_generation(&stubborn_pane, stubborn_before.generation).await?;
    let stubborn_pid = running_pid(&stubborn_after)?;
    assert!(
        process_exists(stubborn_pid)?,
        "stubborn pane process should be running before close"
    );
    assert_eq!(
        stubborn_pane.close().await?,
        PaneCloseOutcome::Closed {
            target: stubborn_target,
            window_destroyed: false,
        }
    );
    wait_for_process_absent(stubborn_pid).await?;

    let close_target = session
        .window(0)
        .split(SplitDirectionSpec::Vertical)
        .await?;
    let close_pane = session.pane(close_target.window_index, close_target.pane_index);
    let stale_observer = rmux.pane(close_target.clone()).await?;
    assert_eq!(
        close_pane.close().await?,
        PaneCloseOutcome::Closed {
            target: close_target.clone(),
            window_destroyed: false,
        }
    );
    wait_for_pane_absent(&stale_observer).await?;
    assert_eq!(
        stale_observer.close().await?,
        PaneCloseOutcome::AlreadyClosed {
            target: close_target
        }
    );

    harness.finish().await
}

fn running_pid(info: &PaneInfo) -> TestResult<u32> {
    match info.process {
        PaneProcessState::Running { pid: Some(pid) } => Ok(pid),
        ref state => Err(format!("expected running pane pid, got {state:?}").into()),
    }
}

async fn wait_for_visible_text(pane: &rmux_sdk::Pane, marker: &str) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = pane.snapshot().await?;
        if snapshot.visible_text().contains(marker) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("pane did not render {marker:?} within deadline").into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_generation(pane: &rmux_sdk::Pane, previous: u64) -> TestResult<PaneInfo> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let info = only_pane_info(&pane.info().await?);
        if info.generation > previous {
            return Ok(info);
        }
        if Instant::now() >= deadline {
            return Err(
                format!("pane lifecycle generation did not advance past {previous}").into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_pane_absent(pane: &rmux_sdk::Pane) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if pane.id().await?.is_none() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("pane remained listed after close".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_process_absent(pid: u32) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if !process_exists(pid)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("process {pid} remained alive after pane close").into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn process_exists(pid: u32) -> TestResult<bool> {
    Ok(Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success())
}

fn only_pane_info(info: &rmux_sdk::InfoSnapshot) -> PaneInfo {
    assert_eq!(info.panes.len(), 1, "expected exactly one pane info row");
    info.panes[0].clone()
}

fn session_name(value: &str) -> SessionName {
    SessionName::new(value).expect("valid session name")
}

async fn framed_request(socket_path: &Path, request: Request) -> TestResult<Response> {
    let mut stream = UnixStream::connect(socket_path).await?;
    let frame = encode_frame(&request)?;
    stream.write_all(&frame).await?;
    read_response(&mut stream).await
}

async fn read_response(stream: &mut UnixStream) -> TestResult<Response> {
    let mut decoder = FrameDecoder::new();
    let mut read_buffer = [0_u8; 8192];

    loop {
        if let Some(response) = decoder.next_frame::<Response>()? {
            return Ok(response);
        }

        let bytes_read = stream.read(&mut read_buffer).await?;
        if bytes_read == 0 {
            return Err("connection closed before response frame".into());
        }
        decoder.push_bytes(&read_buffer[..bytes_read]);
    }
}

struct Harness {
    _root: TestRoot,
    socket_path: PathBuf,
    child: Option<Child>,
}

impl Harness {
    async fn start(label: &str) -> TestResult<Self> {
        let root = TestRoot::new(label);
        std::fs::create_dir_all(root.path())?;
        let socket_path = root.path().join("daemon.sock");
        let mut child = Command::new(rmux_binary()?)
            .arg("--__internal-daemon")
            .arg(&socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        wait_for_daemon_ready(&socket_path, &mut child).await?;

        Ok(Self {
            _root: root,
            socket_path,
            child: Some(child),
        })
    }

    fn rmux(&self) -> rmux_sdk::Rmux {
        RmuxBuilder::new().unix_socket(&self.socket_path).build()
    }

    async fn finish(self) -> TestResult {
        let shutdown = self.rmux().shutdown().await;
        wait_for_child_exit(self, "server did not exit during cleanup").await?;
        if let Err(error) = shutdown {
            let rendered = error.to_string();
            assert!(
                rendered.contains("connect to rmux daemon")
                    || rendered.contains("rmux daemon closed the transport")
                    || rendered.contains("rmux transport actor is closed")
                    || rendered.contains("Connection reset by peer"),
                "unexpected cleanup shutdown error: {rendered}"
            );
        }
        Ok(())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }
}

async fn wait_for_child_exit(mut harness: Harness, timeout_message: &'static str) -> TestResult {
    let mut child = harness.child.take().expect("harness owns daemon child");
    let deadline = Instant::now() + Duration::from_secs(5);

    loop {
        if let Some(status) = child.try_wait()? {
            assert!(status.success(), "daemon exited with status {status}");
            return Ok(());
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(timeout_message.into());
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_daemon_ready(socket_path: &Path, child: &mut Child) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    let probe = session_name("sdkprobe");

    loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!("daemon exited before accepting RPC: {status}").into());
        }

        if matches!(
            framed_request(
                socket_path,
                Request::HasSession(HasSessionRequest {
                    target: probe.clone()
                })
            )
            .await,
            Ok(Response::HasSession(_))
        ) {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "daemon at '{}' did not accept RPC before timeout",
                socket_path.display()
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
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
    let status =
        std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .arg("build")
            .arg("--bin")
            .arg("rmux")
            .arg("--locked")
            .arg("--manifest-path")
            .arg(workspace_root().join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", &target_dir)
            .status()?;
    if !status.success() {
        return Err(format!("failed to build rmux binary for daemon tests: {status}").into());
    }
    if !candidate.is_file() {
        return Err(format!(
            "rmux daemon build succeeded but '{}' was not created",
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

struct TestRoot {
    path: PathBuf,
}

impl TestRoot {
    fn new(label: &str) -> Self {
        let unique_id = UNIQUE_ID.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from("/tmp").join(format!(
            "rmux-sdk-lifecycle-{}-{}-{unique_id}",
            compact_label(label),
            std::process::id()
        ));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn compact_label(label: &str) -> String {
    let compact = label
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(16)
        .collect::<String>();
    if compact.is_empty() {
        "x".to_owned()
    } else {
        compact
    }
}
