#![cfg(unix)]

use std::error::Error;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

mod common;

use common::{session_name, start_server, ClientConnection, TestHarness, PTY_TEST_LOCK};
use rmux_proto::{
    encode_attach_message, AttachMessage, AttachSessionRequest, KillSessionRequest,
    NewSessionRequest, PaneTarget, Request, Response, SelectPaneRequest, SendKeysRequest,
    SendKeysResponse, SplitWindowRequest, SplitWindowTarget, TerminalSize,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const STEP_TIMEOUT: Duration = Duration::from_secs(5);

async fn send_attach_command(
    stream: &mut tokio::net::UnixStream,
    command: &str,
) -> Result<(), Box<dyn Error>> {
    let mut bytes = command.as_bytes().to_vec();
    bytes.push(b'\r');
    let frame = encode_attach_message(&AttachMessage::Data(bytes))?;
    stream.write_all(&frame).await?;
    Ok(())
}

async fn read_attach_message(
    stream: &mut tokio::net::UnixStream,
) -> Result<Option<AttachMessage>, Box<dyn Error>> {
    let mut tag = [0_u8; 1];
    let bytes_read = stream.read(&mut tag).await?;
    if bytes_read == 0 {
        return Ok(None);
    }

    match tag[0] {
        1 => {
            let mut length = [0_u8; 4];
            stream.read_exact(&mut length).await?;
            let payload_len = u32::from_le_bytes(length) as usize;
            let mut payload = vec![0_u8; payload_len];
            stream.read_exact(&mut payload).await?;
            Ok(Some(AttachMessage::Data(payload)))
        }
        2 => {
            let mut size = [0_u8; 4];
            stream.read_exact(&mut size).await?;
            Ok(Some(AttachMessage::Resize(rmux_proto::TerminalSize {
                cols: u16::from_le_bytes([size[0], size[1]]),
                rows: u16::from_le_bytes([size[2], size[3]]),
            })))
        }
        other => Err(rmux_proto::RmuxError::Decode(format!(
            "unknown attach-stream message tag {other}"
        ))
        .into()),
    }
}

async fn read_attach_until_contains(
    stream: &mut tokio::net::UnixStream,
    needle: &str,
) -> Result<String, Box<dyn Error>> {
    let deadline = Instant::now() + STEP_TIMEOUT;
    let mut output = String::new();

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Some(message) = tokio::time::timeout(remaining, read_attach_message(stream)).await??
        else {
            break;
        };

        if let AttachMessage::Data(bytes) = message {
            output.push_str(&String::from_utf8_lossy(&bytes));
            if output.contains(needle) {
                return Ok(output);
            }
        }
    }

    Err(io::Error::other(format!(
        "timed out waiting for attach output containing {needle:?}: {output:?}"
    ))
    .into())
}

async fn wait_for_file_contents(
    path: &Path,
    expected: &str,
    timeout: Duration,
) -> Result<bool, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        match fs::read_to_string(path) {
            Ok(contents) if contents == expected => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    Ok(false)
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[tokio::test]
async fn send_keys_writes_to_the_correct_pane_through_the_socket() -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("send-keys");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;

    let created = client
        .send_request(&Request::NewSession(NewSessionRequest {
            session_name: session_name("alpha"),
            detached: true,
            size: Some(TerminalSize { cols: 80, rows: 24 }),
            environment: None,
        }))
        .await?;
    assert!(matches!(created, Response::NewSession(_)));

    let (_, mut attach_stream) = ClientConnection::connect(&socket_path)
        .await?
        .begin_attach(AttachSessionRequest {
            target: session_name("alpha"),
        })
        .await?;

    let response = client
        .send_request(&Request::SendKeys(SendKeysRequest {
            target: PaneTarget::new(session_name("alpha"), 0),
            keys: vec!["printf send-keys-ok".to_owned(), "Enter".to_owned()],
        }))
        .await?;
    assert_eq!(
        response,
        Response::SendKeys(SendKeysResponse { key_count: 2 })
    );
    let output = read_attach_until_contains(&mut attach_stream, "send-keys-ok").await?;
    assert!(output.contains("send-keys-ok"));

    let empty_response = client
        .send_request(&Request::SendKeys(SendKeysRequest {
            target: PaneTarget::new(session_name("alpha"), 0),
            keys: vec![],
        }))
        .await?;
    assert_eq!(
        empty_response,
        Response::SendKeys(SendKeysResponse { key_count: 0 })
    );

    let missing = client
        .send_request(&Request::SendKeys(SendKeysRequest {
            target: PaneTarget::new(session_name("missing"), 0),
            keys: vec!["x".to_owned()],
        }))
        .await?;
    assert!(matches!(missing, Response::Error(_)));

    let missing_pane = client
        .send_request(&Request::SendKeys(SendKeysRequest {
            target: PaneTarget::new(session_name("alpha"), 99),
            keys: vec!["x".to_owned()],
        }))
        .await?;
    assert!(matches!(missing_pane, Response::Error(_)));

    let empty_missing = client
        .send_request(&Request::SendKeys(SendKeysRequest {
            target: PaneTarget::new(session_name("nonexistent"), 0),
            keys: vec![],
        }))
        .await?;
    assert!(matches!(empty_missing, Response::Error(_)));

    drop(attach_stream);
    let removed = common::send_request(
        &socket_path,
        &Request::KillSession(KillSessionRequest {
            target: session_name("alpha"),
            kill_all_except_target: false,
            clear_alerts: false,
        }),
    )
    .await?;
    assert_eq!(
        removed,
        Response::KillSession(rmux_proto::KillSessionResponse { existed: true })
    );
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn send_keys_targets_the_correct_pane_in_a_multi_pane_session() -> Result<(), Box<dyn Error>>
{
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("send-keys-multi-pane");
    let socket_path = harness.socket_path().to_path_buf();
    let handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;

    let created = client
        .send_request(&Request::NewSession(NewSessionRequest {
            session_name: session_name("beta"),
            detached: true,
            size: Some(TerminalSize {
                cols: 120,
                rows: 40,
            }),
            environment: None,
        }))
        .await?;
    assert!(matches!(created, Response::NewSession(_)));

    let split = client
        .send_request(&Request::SplitWindow(SplitWindowRequest {
            target: SplitWindowTarget::Session(session_name("beta")),
            direction: rmux_proto::SplitDirection::Vertical,
            before: false,
            environment: None,
        }))
        .await?;
    assert!(matches!(split, Response::SplitWindow(_)));

    let (_, mut attach_stream) = ClientConnection::connect(&socket_path)
        .await?
        .begin_attach(AttachSessionRequest {
            target: session_name("beta"),
        })
        .await?;

    let pane0_response = client
        .send_request(&Request::SendKeys(SendKeysRequest {
            target: PaneTarget::new(session_name("beta"), 0),
            keys: vec!["printf pane-zero".to_owned(), "Enter".to_owned()],
        }))
        .await?;
    assert_eq!(
        pane0_response,
        Response::SendKeys(SendKeysResponse { key_count: 2 })
    );
    let pane0_output = read_attach_until_contains(&mut attach_stream, "pane-zero").await?;
    assert!(pane0_output.contains("pane-zero"));

    let selected = client
        .send_request(&Request::SelectPane(SelectPaneRequest {
            target: PaneTarget::new(session_name("beta"), 1),
            title: None,
        }))
        .await?;
    assert!(matches!(selected, Response::SelectPane(_)));

    let pane1_response = client
        .send_request(&Request::SendKeys(SendKeysRequest {
            target: PaneTarget::new(session_name("beta"), 1),
            keys: vec!["printf pane-one".to_owned(), "Enter".to_owned()],
        }))
        .await?;
    assert_eq!(
        pane1_response,
        Response::SendKeys(SendKeysResponse { key_count: 2 })
    );
    let pane1_output = read_attach_until_contains(&mut attach_stream, "pane-one").await?;
    assert!(pane1_output.contains("pane-one"));

    drop(attach_stream);
    let removed = common::send_request(
        &socket_path,
        &Request::KillSession(KillSessionRequest {
            target: session_name("beta"),
            kill_all_except_target: false,
            clear_alerts: false,
        }),
    )
    .await?;
    assert_eq!(
        removed,
        Response::KillSession(rmux_proto::KillSessionResponse { existed: true })
    );
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn send_keys_ctrl_c_interrupts_a_real_pane_process() -> Result<(), Box<dyn Error>> {
    let _guard = PTY_TEST_LOCK.lock().await;
    let harness = TestHarness::new("send-keys-ctrl-c");
    let socket_path = harness.socket_path().to_path_buf();
    let root = socket_path
        .parent()
        .expect("socket path must have a parent");
    let recovery_path = root.join("ctrl-c-recovered.txt");
    let handle = start_server(&harness).await?;
    let mut client = ClientConnection::connect(&socket_path).await?;

    let created = client
        .send_request(&Request::NewSession(NewSessionRequest {
            session_name: session_name("gamma"),
            detached: true,
            size: Some(TerminalSize { cols: 80, rows: 24 }),
            environment: None,
        }))
        .await?;
    assert!(matches!(created, Response::NewSession(_)));

    let (_, mut attach_stream) = ClientConnection::connect(&socket_path)
        .await?
        .begin_attach(AttachSessionRequest {
            target: session_name("gamma"),
        })
        .await?;

    let start_sleep = client
        .send_request(&Request::SendKeys(SendKeysRequest {
            target: PaneTarget::new(session_name("gamma"), 0),
            keys: vec!["sleep 5".to_owned(), "Enter".to_owned()],
        }))
        .await?;
    assert_eq!(
        start_sleep,
        Response::SendKeys(SendKeysResponse { key_count: 2 })
    );
    let sleep_output = read_attach_until_contains(&mut attach_stream, "sleep 5").await?;
    assert!(sleep_output.contains("sleep 5"));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let interrupt = client
        .send_request(&Request::SendKeys(SendKeysRequest {
            target: PaneTarget::new(session_name("gamma"), 0),
            keys: vec!["C-c".to_owned()],
        }))
        .await?;
    assert_eq!(
        interrupt,
        Response::SendKeys(SendKeysResponse { key_count: 1 })
    );
    let interrupt_output = read_attach_until_contains(&mut attach_stream, "^C").await?;
    assert!(
        interrupt_output.contains("^C"),
        "attach output should include the interrupt echo before recovery, got {interrupt_output:?}"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;

    let recovery_command = format!("printf ctrl-c-recovered > {}", shell_quote(&recovery_path));
    let mut recovered = false;
    for _ in 0..4 {
        send_attach_command(&mut attach_stream, &recovery_command).await?;
        if wait_for_file_contents(
            &recovery_path,
            "ctrl-c-recovered",
            Duration::from_millis(250),
        )
        .await?
        {
            recovered = true;
            break;
        }
    }
    assert!(recovered, "shell should accept input again after ctrl-c");

    drop(attach_stream);
    let removed = common::send_request(
        &socket_path,
        &Request::KillSession(KillSessionRequest {
            target: session_name("gamma"),
            kill_all_except_target: false,
            clear_alerts: false,
        }),
    )
    .await?;
    assert_eq!(
        removed,
        Response::KillSession(rmux_proto::KillSessionResponse { existed: true })
    );
    handle.shutdown().await?;
    Ok(())
}
