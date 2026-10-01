//! The interactive session (`interactive`), against a real SSH server in
//! the test process (ego-transport's listener) and a [`Terminal`] made of
//! channels, so no tty is needed.
//!
//! What a tty adds on top -- raw mode, the stdin reader, the window size
//! -- is `crates/drt/tests/ssh_cli.rs`'s, against OpenSSH under a pty.
//!
//! ## surface block
//!
//! - Entry points: the `#[tokio::test]`s, one per promise the module makes.
//! - Fan-out: [`Server`] is the far end; [`Canned`] answers prompts.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use drt_connector_ssh::interactive::{self, Credentials, Login, Prompt, Terminal, Trust};
use ego_transport::ssh::{
    generate_ed25519, ClientAuthorization, PrivateKey, SshChannelEvent, SshChannelKind,
    SshListener, SshServerConfig,
};
use ego_transport::transport::Transport;
use tokio::sync::mpsc;

/// What the server saw, for the assertions.
#[derive(Debug, PartialEq)]
enum Saw {
    Pty { term: String, cols: u32, rows: u32 },
    Resize(u32, u32),
    Input(Vec<u8>),
    Eof,
}

/// An sshd with one user key. A shell answers `got:` and what was typed,
/// and `exit` ends it with status 7.
struct Server {
    addr: String,
    host: PrivateKey,
    saw: mpsc::UnboundedReceiver<Saw>,
}

async fn server(client: &PrivateKey) -> Server {
    let host = generate_ed25519();
    let mut config = SshServerConfig::new(host.clone());
    config.authorization = ClientAuthorization::Keys(vec![client.public_key().clone()]);
    let listener = SshListener::bind("127.0.0.1:0", config).await.unwrap();
    let addr = listener.local_addr().to_string();
    let (tx, saw) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(mut conn) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                while let Ok(mut ch) = conn.next_channel().await {
                    let SshChannelKind::Pty(p) = ch.kind().clone() else {
                        continue;
                    };
                    let _ = tx.send(Saw::Pty {
                        term: p.term,
                        cols: p.cols,
                        rows: p.rows,
                    });
                    ch.send(b"ready$ ").await.unwrap();
                    loop {
                        match ch.next_event().await {
                            SshChannelEvent::Data(d) => {
                                let _ = tx.send(Saw::Input(d.clone()));
                                if d.windows(4).any(|w| w == b"exit") {
                                    ch.exit_status(7).await.unwrap();
                                    ch.close().await.ok();
                                    break;
                                }
                                let mut out = b"got:".to_vec();
                                out.extend_from_slice(&d);
                                ch.send(&out).await.unwrap();
                            }
                            SshChannelEvent::WindowChange { cols, rows, .. } => {
                                let _ = tx.send(Saw::Resize(cols, rows));
                            }
                            SshChannelEvent::Eof => {
                                let _ = tx.send(Saw::Eof);
                                ch.exit_status(0).await.ok();
                                ch.close().await.ok();
                                break;
                            }
                            SshChannelEvent::Closed => break,
                            _ => {}
                        }
                    }
                }
            });
        }
    });
    Server { addr, host, saw }
}

/// Answers every confirm with `yes`, and remembers what it was asked.
struct Canned {
    yes: bool,
    asked: Mutex<Vec<String>>,
}

impl Prompt for Canned {
    fn confirm(&self, question: &str) -> bool {
        self.asked.lock().unwrap().push(question.to_string());
        self.yes
    }
    fn secret(&self, _question: &str) -> Option<String> {
        None
    }
    fn say(&self, _text: &str) {}
}

fn canned(yes: bool) -> Arc<Canned> {
    Arc::new(Canned {
        yes,
        asked: Mutex::new(Vec::new()),
    })
}

fn login(server: &Server, client: PrivateKey, trust: Trust) -> Login {
    let port = server.addr.rsplit_once(':').unwrap().1.parse().unwrap();
    Login {
        user: "someone".into(),
        host: "127.0.0.1".into(),
        port,
        trust,
        credentials: Credentials::Key(Box::new(client)),
    }
}

fn pinned(server: &Server) -> Trust {
    Trust::Pinned {
        fingerprints: vec![server
            .host
            .public_key()
            .fingerprint(ssh_key::HashAlg::Sha256)
            .to_string()],
        keys: Vec::new(),
    }
}

async fn tcp(server: &Server) -> tokio::net::TcpStream {
    tokio::net::TcpStream::connect(&server.addr).await.unwrap()
}

/// A terminal whose output is collected and whose size the test sets.
struct Fake {
    keys: mpsc::UnboundedSender<Vec<u8>>,
    screen: Arc<Mutex<Vec<u8>>>,
    size: Arc<Mutex<(u32, u32)>>,
}

struct Screen(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Screen {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn terminal() -> (Fake, Terminal) {
    let (keys, input) = mpsc::unbounded_channel();
    let screen = Arc::new(Mutex::new(Vec::new()));
    let size = Arc::new(Mutex::new((100, 30)));
    let s = size.clone();
    let term = Terminal {
        input,
        output: Box::new(Screen(screen.clone())),
        size: Box::new(move || *s.lock().unwrap()),
        term: "xterm-test".into(),
    };
    (Fake { keys, screen, size }, term)
}

async fn until(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never: {what}");
}

async fn next(saw: &mut mpsc::UnboundedReceiver<Saw>) -> Saw {
    tokio::time::timeout(Duration::from_secs(5), saw.recv())
        .await
        .expect("the server saw nothing")
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shell_gets_the_pty_the_keys_and_the_resize_and_ends_with_its_status() {
    let client = generate_ed25519();
    let mut s = server(&client).await;
    let conn = interactive::connect(tcp(&s).await, login(&s, client, pinned(&s)), canned(false))
        .await
        .unwrap();
    let (fake, term) = terminal();
    let session = tokio::spawn(interactive::shell(conn, term));

    assert_eq!(
        next(&mut s.saw).await,
        Saw::Pty {
            term: "xterm-test".into(),
            cols: 100,
            rows: 30
        }
    );
    let screen = fake.screen.clone();
    until("the prompt", || {
        String::from_utf8_lossy(&screen.lock().unwrap()).contains("ready$ ")
    })
    .await;

    fake.keys.send(b"ls\r".to_vec()).unwrap();
    assert_eq!(next(&mut s.saw).await, Saw::Input(b"ls\r".to_vec()));
    until("the echo", || {
        String::from_utf8_lossy(&screen.lock().unwrap()).contains("got:ls")
    })
    .await;

    *fake.size.lock().unwrap() = (132, 43);
    assert_eq!(next(&mut s.saw).await, Saw::Resize(132, 43));

    fake.keys.send(b"exit\r".to_vec()).unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(status, Some(7));
}

#[tokio::test(flavor = "multi_thread")]
async fn tilde_dot_disconnects_and_a_closed_keyboard_is_end_of_file() {
    let client = generate_ed25519();
    let mut s = server(&client).await;

    let conn = interactive::connect(
        tcp(&s).await,
        login(&s, client.clone(), pinned(&s)),
        canned(false),
    )
    .await
    .unwrap();
    let (fake, term) = terminal();
    let session = tokio::spawn(interactive::shell(conn, term));
    let _ = next(&mut s.saw).await;
    fake.keys.send(b"~.".to_vec()).unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.unwrap(), None, "closed from this side: no status");
    assert!(
        String::from_utf8_lossy(&fake.screen.lock().unwrap()).contains("Connection closed (~.)")
    );

    let conn = interactive::connect(tcp(&s).await, login(&s, client, pinned(&s)), canned(false))
        .await
        .unwrap();
    let (fake, term) = terminal();
    let session = tokio::spawn(interactive::shell(conn, term));
    let _ = next(&mut s.saw).await;
    drop(fake.keys);
    assert_eq!(next(&mut s.saw).await, Saw::Eof);
    let status = tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.unwrap(), Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_pin_is_refused_before_anything_signs_in() {
    let client = generate_ed25519();
    let s = server(&client).await;
    let trust = Trust::Pinned {
        fingerprints: vec!["SHA256:not-this-one".into()],
        keys: Vec::new(),
    };
    let err = interactive::connect(tcp(&s).await, login(&s, client, trust), canned(true))
        .await
        .err()
        .unwrap();
    assert!(err.contains("refused the host key"), "{err}");
    assert!(err.contains("not the key this connection pins"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn known_hosts_asks_once_then_remembers_and_refuses_a_changed_key() {
    let client = generate_ed25519();
    let s = server(&client).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("known_hosts");
    let known = |ask| Trust::KnownHosts {
        file: file.clone(),
        ask,
    };

    // Unknown, and strict: refused without a question.
    let p = canned(true);
    let err = interactive::connect(
        tcp(&s).await,
        login(&s, client.clone(), known(false)),
        p.clone(),
    )
    .await
    .err()
    .unwrap();
    assert!(err.contains("is not in"), "{err}");
    assert!(p.asked.lock().unwrap().is_empty());

    // Unknown, asked, declined: refused, nothing written.
    let err = interactive::connect(
        tcp(&s).await,
        login(&s, client.clone(), known(true)),
        canned(false),
    )
    .await
    .err()
    .unwrap();
    assert!(err.contains("not trusted"), "{err}");
    assert!(!file.exists());

    // Unknown, asked, accepted: recorded.
    let p = canned(true);
    interactive::connect(
        tcp(&s).await,
        login(&s, client.clone(), known(true)),
        p.clone(),
    )
    .await
    .unwrap();
    assert_eq!(p.asked.lock().unwrap().len(), 1);
    let asked = p.asked.lock().unwrap()[0].clone();
    assert!(asked.contains("SHA256:"), "{asked}");
    assert!(std::fs::read_to_string(&file)
        .unwrap()
        .contains("ssh-ed25519"));

    // Known now: no question.
    let p = canned(false);
    interactive::connect(
        tcp(&s).await,
        login(&s, client.clone(), known(true)),
        p.clone(),
    )
    .await
    .unwrap();
    assert!(p.asked.lock().unwrap().is_empty());

    // The same name with another key: refused, and never asked.
    let other = server(&client).await;
    let mut l = login(&other, client, known(true));
    l.port = login(&s, generate_ed25519(), known(true)).port;
    let p = canned(true);
    let err = interactive::connect(tcp(&other).await, l, p.clone())
        .await
        .err()
        .unwrap();
    assert!(err.contains("is NOT the key"), "{err}");
    assert!(p.asked.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_the_server_does_not_know_is_refused_by_name() {
    let client = generate_ed25519();
    let s = server(&client).await;
    let err = interactive::connect(
        tcp(&s).await,
        login(&s, generate_ed25519(), pinned(&s)),
        canned(false),
    )
    .await
    .err()
    .unwrap();
    assert!(err.contains("someone@[127.0.0.1]:"), "{err}");
    assert!(err.contains("did not accept the configured key"), "{err}");
}
