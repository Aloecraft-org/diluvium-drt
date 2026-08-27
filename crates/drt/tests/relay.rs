//! The relay end to end: a device parks, a caller claims, bytes splice both
//! ways — plus the refusals that make the keys keys.

#![cfg(feature = "relay")]

use drt::relay::{serve, Relay};
use drt_config::{RelayConfig, RelayLabel};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

async fn relay_on_port() -> (std::net::SocketAddr, ()) {
    // Bind first so the test knows the port; serve() takes the config's
    // bind string, so pick a free port the same way the OS does.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let config = RelayConfig {
        bind: addr.to_string(),
        labels: [(
            "xps".to_string(),
            RelayLabel {
                park_key: "park-secret-0123456789".into(),
                caller_key: "caller-secret-987654321".into(),
            },
        )]
        .into(),
    };
    let relay = Relay::new(config);
    tokio::spawn(async move {
        let _ = serve(relay).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    (addr, ())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_and_a_device_splice_through_the_relay() {
    let (addr, ()) = relay_on_port().await;

    // The device parks a leg, as websocat would: a dumb pipe, key in the URL.
    let (mut device, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/park/xps?k=park-secret-0123456789"))
            .await
            .expect("the device could not park");

    // A caller claims it. The claim manifests as the first caller byte —
    // exactly what an SSH client's banner is.
    let (mut caller, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/s/xps?k=caller-secret-987654321"))
            .await
            .expect("the caller could not connect");
    caller
        .send(Message::Binary(b"SSH-2.0-caller\r\n".to_vec()))
        .await
        .unwrap();

    // The device sees the caller's bytes (its cue to dial 127.0.0.1:22
    // lazily)…
    let got = loop {
        match device.next().await.expect("device leg died").unwrap() {
            Message::Binary(b) => break b,
            Message::Ping(_) => continue,
            other => panic!("unexpected on device leg: {other:?}"),
        }
    };
    assert_eq!(&got[..], b"SSH-2.0-caller\r\n");

    // …and answers; the caller reads it back through the splice.
    device
        .send(Message::Binary(b"SSH-2.0-device\r\n".to_vec()))
        .await
        .unwrap();
    let back = loop {
        match caller.next().await.expect("caller leg died").unwrap() {
            Message::Binary(b) => break b,
            Message::Ping(_) => continue,
            other => panic!("unexpected on caller leg: {other:?}"),
        }
    };
    assert_eq!(&back[..], b"SSH-2.0-device\r\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_key_never_upgrades_and_an_empty_pool_says_not_home() {
    let (addr, ()) = relay_on_port().await;

    // Wrong caller key: refused at the handshake — the WebSocket never
    // exists, which is what keeps an open relay from being an open proxy.
    let err = tokio_tungstenite::connect_async(format!("ws://{addr}/s/xps?k=wrong"))
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("403"), "{err}");

    // Unknown label: same refusal, indistinguishable from a bad key.
    let err = tokio_tungstenite::connect_async(format!("ws://{addr}/s/nope?k=x"))
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("403"), "{err}");

    // Right key, no parked leg: the device is not home, and the caller is
    // told so with a close, not a hang.
    let (mut caller, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/s/xps?k=caller-secret-987654321"))
            .await
            .unwrap();
    match caller.next().await.unwrap().unwrap() {
        Message::Close(Some(frame)) => {
            assert!(frame.reason.contains("not home"), "{frame:?}");
        }
        other => panic!("expected a close, got {other:?}"),
    }
}

/// The whole triangle, DRT on all three corners: `drt relay` in the
/// middle, `park()` as the device (lazy local dial, replay of the first
/// bytes), and a caller claiming through `/s/`. Twice, back to back —
/// the second session proves replenish-on-claim parked a fresh leg while
/// the first was still alive.
#[cfg(feature = "tunnel")]
#[tokio::test(flavor = "multi_thread")]
async fn the_full_triangle_and_replenish_on_claim() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (addr, ()) = relay_on_port().await;

    // The "sshd": a local echo server the device dials lazily.
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut c, _)) = echo.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut b = [0u8; 4096];
                while let Ok(n) = c.read(&mut b).await {
                    if n == 0 || c.write_all(&b[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    // The device: parks, re-parks on claim, forever.
    let park_url = format!("ws://{addr}/park/xps?k=park-secret-0123456789");
    tokio::spawn(async move {
        let _ = drt::tunnel::park(&park_url, &echo_addr).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    for round in 0..2u8 {
        let (mut caller, _) = tokio_tungstenite::connect_async(format!(
            "ws://{addr}/s/xps?k=caller-secret-987654321"
        ))
        .await
        .unwrap();
        let hello = format!("hello-{round}");
        caller
            .send(Message::Binary(hello.clone().into_bytes()))
            .await
            .unwrap();
        let back = loop {
            match caller.next().await.expect("caller leg died").unwrap() {
                Message::Binary(b) => break b,
                Message::Ping(_) => continue,
                Message::Close(f) => panic!("closed in round {round}: {f:?}"),
                _ => continue,
            }
        };
        assert_eq!(&back[..], hello.as_bytes(), "round {round}");
        // Leave the first session OPEN while the second claims: the fresh
        // leg must come from replenish, not from this session ending.
        if round == 1 {
            drop(caller);
        } else {
            std::mem::forget(caller);
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }
}
