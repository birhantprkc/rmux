#![cfg(windows)]

use std::io::ErrorKind;
use std::io::{Read, Write};
use std::time::Duration;

use rmux_ipc::{connect_blocking, endpoint_for_label, LocalListener};
use rmux_os::identity::UserIdentity;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

#[tokio::test]
async fn named_pipe_roundtrip_uses_bound_endpoint() -> std::io::Result<()> {
    let endpoint = endpoint_for_label(format!("integration-{}", std::process::id()))?;
    let listener = LocalListener::bind(&endpoint)?;

    let accept = tokio::spawn(async move {
        let (mut stream, peer) = listener.accept().await?;
        assert_ne!(peer.pid, 0);
        assert!(matches!(peer.user, UserIdentity::Sid(ref sid) if sid.starts_with("S-")));

        let mut request = [0_u8; 4];
        stream.read_exact(&mut request).await?;
        assert_eq!(&request, b"ping");
        stream.write_all(b"pong").await?;
        std::io::Result::Ok(())
    });

    tokio::task::yield_now().await;

    let endpoint_for_client = endpoint.clone();
    let client = timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            connect_blocking(&endpoint_for_client, Duration::from_secs(2))
        }),
    )
    .await
    .expect("client connect timed out")
    .expect("client connect task")?;

    timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            let mut client = client;
            client.write_all(b"ping")?;
            let mut response = [0_u8; 4];
            client.read_exact(&mut response)?;
            assert_eq!(&response, b"pong");
            std::io::Result::Ok(())
        }),
    )
    .await
    .expect("client roundtrip timed out")
    .expect("client roundtrip task")?;

    timeout(Duration::from_secs(2), accept)
        .await
        .expect("accept task timed out")
        .expect("accept task")?;
    Ok(())
}

#[tokio::test]
async fn first_pipe_instance_rejects_second_listener() -> std::io::Result<()> {
    let endpoint = endpoint_for_label(format!("squat-{}", std::process::id()))?;
    let _first = LocalListener::bind(&endpoint)?;
    let second = LocalListener::bind(&endpoint).expect_err("second listener should fail");

    assert!(
        matches!(
            second.kind(),
            ErrorKind::PermissionDenied | ErrorKind::AlreadyExists
        ) || matches!(second.raw_os_error(), Some(5) | Some(231)),
        "unexpected bind error: {second:?}"
    );
    Ok(())
}
