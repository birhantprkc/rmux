use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[cfg(unix)]
use rmux_proto::SendKeysExtRequest;
use rmux_proto::{PaneTarget, Request, Response, SendKeysRequest, SendKeysResponse};
use tokio::time::sleep;

use super::RequestHandler;

static UNIQUE_ID: AtomicUsize = AtomicUsize::new(0);

pub(in crate::handler) struct PaneInputCapture {
    output_path: PathBuf,
    #[cfg(windows)]
    ready_path: PathBuf,
    #[cfg(windows)]
    script_path: PathBuf,
}

impl PaneInputCapture {
    pub(in crate::handler) async fn start(
        handler: &RequestHandler,
        session_name: &rmux_proto::SessionName,
        label: &str,
        expected_len: usize,
    ) -> Self {
        assert!(expected_len > 0, "input capture requires expected bytes");
        let output_path = unique_output_path(label);

        #[cfg(unix)]
        {
            start_unix_capture(handler, session_name, &output_path).await;
            Self { output_path }
        }

        #[cfg(windows)]
        {
            let ready_path = output_path.with_extension("ready");
            let script_path = output_path.with_extension("capture");
            let _ = fs::remove_file(&ready_path);
            let _ = fs::remove_file(&script_path);
            write_windows_capture_script(&script_path);
            start_windows_capture(
                handler,
                session_name,
                &script_path,
                &output_path,
                &ready_path,
                expected_len,
            )
            .await;
            wait_for_file_to_exist(&ready_path)
                .await
                .expect("Windows capture helper should signal readiness");
            Self {
                output_path,
                ready_path,
                script_path,
            }
        }
    }

    pub(in crate::handler) async fn finish(
        &self,
        handler: &RequestHandler,
        session_name: &rmux_proto::SessionName,
    ) {
        #[cfg(unix)]
        finish_unix_capture(handler, session_name).await;

        #[cfg(windows)]
        let _ = (handler, session_name);
    }

    pub(in crate::handler) async fn assert_contents(self, expected: &[u8]) {
        wait_for_file_bytes(&self.output_path, expected)
            .await
            .expect("pane input capture contents");
        self.cleanup();
    }

    fn cleanup(&self) {
        let _ = fs::remove_file(&self.output_path);
        #[cfg(windows)]
        {
            let _ = fs::remove_file(&self.ready_path);
            let _ = fs::remove_file(&self.script_path);
        }
    }
}

/// Probes the bytes rmux writes toward a pane.
///
/// Unix can observe these through `cat` on a PTY. Native Windows console apps
/// may consume VT input wrappers before user code can read them, so raw-byte
/// assertions use a test-only writer spy there.
pub(in crate::handler) enum RawPaneInputProbe {
    #[cfg(unix)]
    Console(PaneInputCapture),
    #[cfg(windows)]
    Spy(PaneInputSpy),
}

impl RawPaneInputProbe {
    pub(in crate::handler) async fn start(
        handler: &RequestHandler,
        session_name: &rmux_proto::SessionName,
        label: &str,
        expected_len: usize,
    ) -> Self {
        #[cfg(unix)]
        {
            Self::Console(PaneInputCapture::start(handler, session_name, label, expected_len).await)
        }

        #[cfg(windows)]
        {
            let _ = (label, expected_len);
            Self::Spy(PaneInputSpy::start(handler, session_name).await)
        }
    }

    pub(in crate::handler) async fn finish(
        &self,
        handler: &RequestHandler,
        session_name: &rmux_proto::SessionName,
    ) {
        match self {
            #[cfg(unix)]
            Self::Console(capture) => capture.finish(handler, session_name).await,
            #[cfg(windows)]
            Self::Spy(_) => {
                let _ = (handler, session_name);
            }
        }
    }

    pub(in crate::handler) async fn assert_contents(
        self,
        handler: &RequestHandler,
        expected: &[u8],
    ) {
        #[cfg(unix)]
        let _ = handler;

        match self {
            #[cfg(unix)]
            Self::Console(capture) => capture.assert_contents(expected).await,
            #[cfg(windows)]
            Self::Spy(spy) => spy.assert_contents(handler, expected).await,
        }
    }
}

#[cfg(windows)]
pub(in crate::handler) struct PaneInputSpy {
    target: PaneTarget,
}

#[cfg(windows)]
impl PaneInputSpy {
    pub(in crate::handler) async fn start(
        handler: &RequestHandler,
        session_name: &rmux_proto::SessionName,
    ) -> Self {
        let target = PaneTarget::new(session_name.clone(), 0);
        let state = handler.state.lock().await;
        state.start_pane_input_capture_for_test(&target);
        Self { target }
    }

    pub(in crate::handler) async fn assert_contents(
        self,
        handler: &RequestHandler,
        expected: &[u8],
    ) {
        let state = handler.state.lock().await;
        assert_eq!(
            state.pane_input_capture_for_test(&self.target),
            Some(expected.to_vec())
        );
    }
}

fn unique_output_path(label: &str) -> PathBuf {
    let unique_id = UNIQUE_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "rmux-{label}-{}-{unique_id}.bin",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    path
}

#[cfg(unix)]
async fn start_unix_capture(
    handler: &RequestHandler,
    session_name: &rmux_proto::SessionName,
    path: &Path,
) {
    let response = handler
        .handle(Request::SendKeys(SendKeysRequest {
            target: PaneTarget::new(session_name.clone(), 0),
            keys: vec![
                format!("cat > {}", sh_single_quote(path)),
                "Enter".to_owned(),
            ],
        }))
        .await;
    assert_eq!(
        response,
        Response::SendKeys(SendKeysResponse { key_count: 2 })
    );
    wait_for_file_to_exist(path)
        .await
        .expect("cat capture file should be created by shell redirection");
}

#[cfg(unix)]
async fn finish_unix_capture(handler: &RequestHandler, session_name: &rmux_proto::SessionName) {
    let response = handler
        .handle(Request::SendKeysExt(SendKeysExtRequest {
            target: Some(PaneTarget::new(session_name.clone(), 0)),
            keys: vec!["04".to_owned()],
            expand_formats: false,
            hex: true,
            literal: false,
            dispatch_key_table: false,
            copy_mode_command: false,
            forward_mouse_event: false,
            reset_terminal: false,
            repeat_count: None,
        }))
        .await;
    assert_eq!(
        response,
        Response::SendKeys(SendKeysResponse { key_count: 1 })
    );
}

#[cfg(unix)]
fn sh_single_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(windows)]
async fn start_windows_capture(
    handler: &RequestHandler,
    session_name: &rmux_proto::SessionName,
    script_path: &Path,
    output_path: &Path,
    ready_path: &Path,
    expected_len: usize,
) {
    let command = format!(
        "powershell.exe -NoProfile -ExecutionPolicy Bypass -File {} {} {} {}",
        cmd_quote(script_path),
        cmd_quote(output_path),
        cmd_quote(ready_path),
        expected_len
    );
    let response = handler
        .handle(Request::SendKeys(SendKeysRequest {
            target: PaneTarget::new(session_name.clone(), 0),
            keys: vec![command, "Enter".to_owned()],
        }))
        .await;
    assert_eq!(
        response,
        Response::SendKeys(SendKeysResponse { key_count: 2 })
    );
}

#[cfg(windows)]
fn write_windows_capture_script(path: &Path) {
    fs::write(path, WINDOWS_CAPTURE_SCRIPT).expect("write Windows pane input capture helper");
}

#[cfg(windows)]
fn cmd_quote(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

#[cfg(windows)]
const WINDOWS_CAPTURE_SCRIPT: &str = r#"
param(
    [Parameter(Mandatory=$true)][string]$OutputPath,
    [Parameter(Mandatory=$true)][string]$ReadyPath,
    [Parameter(Mandatory=$true)][int]$ByteCount
)

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

public static class RmuxConsoleMode {
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr GetStdHandle(int nStdHandle);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool GetConsoleMode(IntPtr hConsoleHandle, out int lpMode);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool SetConsoleMode(IntPtr hConsoleHandle, int dwMode);
}
"@

$inputHandle = [RmuxConsoleMode]::GetStdHandle(-10)
[int]$mode = 0
if ([RmuxConsoleMode]::GetConsoleMode($inputHandle, [ref]$mode)) {
    [RmuxConsoleMode]::SetConsoleMode($inputHandle, 0x0200) | Out-Null
}

[IO.File]::WriteAllText($ReadyPath, "ready")

$stdin = [Console]::OpenStandardInput()
$buffer = [byte[]]::new($ByteCount)
$offset = 0
while ($offset -lt $ByteCount) {
    $read = $stdin.Read($buffer, $offset, $ByteCount - $offset)
    if ($read -le 0) {
        Start-Sleep -Milliseconds 10
        continue
    }
    $offset += $read
    if ($offset -gt 0) {
        [IO.File]::WriteAllBytes($OutputPath, $buffer[0..($offset - 1)])
    }
}

[IO.File]::WriteAllBytes($OutputPath, $buffer)
"#;

async fn wait_for_file_bytes(path: &Path, expected: &[u8]) -> Result<(), io::Error> {
    for _ in 0..100 {
        match fs::read(path) {
            Ok(contents) if contents == expected => return Ok(()),
            Ok(_) | Err(_) => sleep(Duration::from_millis(20)).await,
        }
    }

    let actual = fs::read(path).ok();
    let actual_hex = actual
        .as_deref()
        .map(hex_dump)
        .unwrap_or_else(|| "<missing>".to_owned());
    Err(io::Error::other(format!(
        "file '{}' never reached expected contents; expected={}, actual={}",
        path.display(),
        hex_dump(expected),
        actual_hex
    )))
}

fn hex_dump(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join("-")
}

async fn wait_for_file_to_exist(path: &Path) -> Result<(), io::Error> {
    for _ in 0..100 {
        if path.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(20)).await;
    }

    Err(io::Error::other(format!(
        "file '{}' was not created",
        path.display()
    )))
}
