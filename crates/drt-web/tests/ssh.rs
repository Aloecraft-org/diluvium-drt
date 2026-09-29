//! A real SSH connection into `drt_web::ssh`, over memory.
//!
//! Native on purpose (doc/SshInBrowser.md §5): what is browser-specific is
//! the socket and the executor, and both are below this. What is *here* is
//! the posture and the shell, and those are the same on any target -- so
//! they are gated by the workspace's own `cargo test` rather than only by
//! a browser the main test job does not run.
//!
//! The client is russh's, which is not the client that matters -- a
//! standard `ssh(1)` through `drt tunnel` is -- but it speaks the protocol
//! rather than an agreement between two things we wrote.

use std::sync::Arc;

use drt_web::ssh::{serve, Authorized, HostKey, Shell};
use russh::keys::{Algorithm, PrivateKey, PrivateKeyWithHashAlg};
use tokio::sync::mpsc;

/// The client half: accepts the host key, because a test that pinned it
/// would be testing `known_hosts` rather than this.
struct AnyHost;

impl russh::client::Handler for AnyHost {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// A key, and the `authorized_keys` line that names it.
fn a_key() -> (PrivateKey, String) {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let line = key.public_key().to_openssh().unwrap();
    (key, line)
}

/// Serve one connection over memory, and hand back the client's end and
/// the shells the server opens.
fn connected(authorized: Authorized) -> (tokio::io::DuplexStream, mpsc::Receiver<Shell>) {
    let (server_side, client_side) = tokio::io::duplex(64 * 1024);
    let host_key = HostKey::parse(&HostKey::generate().unwrap()).unwrap();
    let (shells, opened) = mpsc::channel(4);
    // Spawned rather than awaited: `run_stream` writes the server's SSH id
    // and then waits for the client's, so awaiting it first deadlocks.
    tokio::spawn(async move {
        let _ = serve(server_side, host_key, authorized, shells).await;
    });
    (client_side, opened)
}

/// The whole path: a named key logs in, asks for a terminal, and the two
/// ends carry bytes -- what the page writes reaches the client, and what
/// the client types reaches the page.
#[tokio::test]
async fn a_named_key_gets_a_shell_that_carries_bytes_both_ways() {
    let (key, line) = a_key();
    let (client_side, mut opened) = connected(Authorized::parse(&line).unwrap());

    let mut client = russh::client::connect_stream(
        Arc::new(russh::client::Config::default()),
        client_side,
        AnyHost,
    )
    .await
    .unwrap();
    let auth = client
        .authenticate_publickey("whoever", PrivateKeyWithHashAlg::new(Arc::new(key), None))
        .await
        .unwrap();
    assert!(auth.success(), "a key in authorized_keys was refused");

    let mut channel = client.channel_open_session().await.unwrap();
    channel
        .request_pty(true, "xterm-256color", 100, 30, 0, 0, &[])
        .await
        .unwrap();
    channel.request_shell(true).await.unwrap();

    // The window the client asked for reaches the page, because a shell
    // that does not know its width wraps in the wrong place.
    let mut shell = opened.recv().await.expect("no shell was opened");
    assert_eq!(shell.window.get(), (100, 30));

    shell.write(b"drt in a page\r\n$ ".to_vec()).await.unwrap();
    let mut from_server = Vec::new();
    while !from_server.ends_with(b"$ ") {
        match channel.wait().await.expect("the session ended early") {
            russh::ChannelMsg::Data { data } => from_server.extend_from_slice(&data),
            _ => continue,
        }
    }
    assert_eq!(from_server, b"drt in a page\r\n$ ");

    channel.data_bytes(&b"buildinfo\r"[..]).await.unwrap();
    assert_eq!(shell.read().await.unwrap(), b"buildinfo\r");

    // And a shell that exits is a session that ends, with a status.
    shell.close(0).await;
    let mut status = None;
    while let Some(msg) = channel.wait().await {
        if let russh::ChannelMsg::ExitStatus { exit_status } = msg {
            status = Some(exit_status);
        }
    }
    assert_eq!(status, Some(0));
}

/// The rule the posture rests on, over the wire rather than in a unit
/// test: a key nobody named does not get in.
#[tokio::test]
async fn an_unnamed_key_is_refused() {
    let (_theirs, line) = a_key();
    let (mine, _) = a_key();
    let (client_side, _opened) = connected(Authorized::parse(&line).unwrap());

    let mut client = russh::client::connect_stream(
        Arc::new(russh::client::Config::default()),
        client_side,
        AnyHost,
    )
    .await
    .unwrap();
    let auth = client
        .authenticate_publickey("whoever", PrivateKeyWithHashAlg::new(Arc::new(mine), None))
        .await
        .unwrap();
    assert!(!auth.success(), "a key nobody authorized got in");
}

/// A page that has not said who may log in has said nobody, and the
/// empty case is the one worth having a test for: it is what a host that
/// forgot to configure the server ends up with.
#[tokio::test]
async fn no_authorized_keys_means_nobody() {
    let (mine, _) = a_key();
    let (client_side, _opened) = connected(Authorized::default());

    let mut client = russh::client::connect_stream(
        Arc::new(russh::client::Config::default()),
        client_side,
        AnyHost,
    )
    .await
    .unwrap();
    let auth = client
        .authenticate_publickey("whoever", PrivateKeyWithHashAlg::new(Arc::new(mine), None))
        .await
        .unwrap();
    assert!(!auth.success(), "an empty authorized set let someone in");
}
