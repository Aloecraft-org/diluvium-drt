//! `ssmtp/send` against a fake relay, plus every refusal that makes the
//! scope a scope.
//!
//! The fake is the same idea as discofetch's `deploy/mail/test-mail.sh`,
//! which runs its puller against a fake SMTP server on 2525: the interesting
//! assertions are about the bytes we put on the wire, and a real relay would
//! only make them harder to read.

use std::sync::{Arc, Mutex};

use drt_caps::{Scope, ScopeType};
use drt_connector::Connector;
use drt_connector_ssmtp::{
    fold_ids, msg_ids_of, SsmtpConnector, FOLD_AT_BYTES, MAX_MSGID_BYTES, MAX_REFERENCES,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// A relay that speaks just enough SMTP to accept one message, and records
/// every line it was sent so the tests can assert on the conversation.
async fn fake_relay() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&log);
    tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let (read, mut write) = sock.into_split();
        let mut lines = BufReader::new(read).lines();
        write.write_all(b"220 fake ESMTP\r\n").await.unwrap();
        let mut in_data = false;
        while let Ok(Some(line)) = lines.next_line().await {
            recorded.lock().unwrap().push(line.clone());
            if in_data {
                if line == "." {
                    in_data = false;
                    write.write_all(b"250 queued\r\n").await.unwrap();
                }
                continue;
            }
            let reply: &[u8] = if line.starts_with("EHLO") {
                b"250-fake\r\n250 AUTH PLAIN\r\n"
            } else if line.starts_with("AUTH") {
                b"235 ok\r\n"
            } else if line.starts_with("MAIL FROM") || line.starts_with("RCPT TO") {
                b"250 ok\r\n"
            } else if line.starts_with("DATA") {
                in_data = true;
                b"354 go\r\n"
            } else if line.starts_with("QUIT") {
                break;
            } else {
                b"500 what\r\n"
            };
            write.write_all(reply).await.unwrap();
        }
    });
    (port, log)
}

fn scope_for(port: u16, allow: &[&str]) -> Scope {
    Scope(rmpv::Value::Map(vec![
        ("host".into(), "127.0.0.1".into()),
        ("port".into(), rmpv::Value::from(port)),
        ("starttls".into(), rmpv::Value::Boolean(false)),
        (
            "from".into(),
            "Michael at Discofetch <no-reply@discofetch.net>".into(),
        ),
        (
            "allow".into(),
            rmpv::Value::Array(allow.iter().map(|a| rmpv::Value::from(*a)).collect()),
        ),
    ]))
}

fn args(to: rmpv::Value, subject: &str, body: &str) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("to".into(), to),
        ("subject".into(), subject.into()),
        ("body".into(), body.into()),
    ])
}

/// The whole path, and what actually goes on the wire.
#[tokio::test]
async fn a_message_reaches_the_relay_with_the_scopes_sender() {
    let (port, log) = fake_relay().await;
    let c = SsmtpConnector::new();
    let sc = scope_for(port, &["@example.com"]);

    let out = c
        .call(
            "ssmtp/send",
            Some(args(
                "someone@example.com".into(),
                "Hello",
                "a body\nline two\n",
            )),
            Some(&sc),
        )
        .await
        .unwrap();

    let map = out.as_map().unwrap();
    let field = |n: &str| {
        map.iter()
            .find(|(k, _)| k.as_str() == Some(n))
            .map(|(_, v)| v.clone())
            .unwrap()
    };
    assert_eq!(field("accepted"), rmpv::Value::Boolean(true));

    let sent = log.lock().unwrap().join("\n");
    // The envelope sender is the SCOPE's, stripped of its display name, and
    // the guest never supplied it.
    assert!(
        sent.contains("MAIL FROM:<no-reply@discofetch.net>"),
        "{sent}"
    );
    assert!(sent.contains("RCPT TO:<someone@example.com>"), "{sent}");
    assert!(
        sent.contains("From: Michael at Discofetch <no-reply@discofetch.net>"),
        "{sent}"
    );
    assert!(sent.contains("Subject: Hello"), "{sent}");
    assert!(sent.contains("a body"), "{sent}");
}

/// The allowlist, in both its shapes and at its edges.
#[tokio::test]
async fn recipients_outside_the_grant_are_refused_by_name() {
    let (port, _) = fake_relay().await;
    let c = SsmtpConnector::new();
    let sc = scope_for(port, &["@example.com", "exact@other.net"]);

    for bad in [
        "someone@evil.net",
        // The two near misses an allowlist has to get right: a domain that
        // merely ENDS with the granted one, and one it is a prefix of.
        "someone@evil-example.com",
        "someone@example.com.evil.net",
        "other@other.net",
    ] {
        let e = c
            .call("ssmtp/send", Some(args(bad.into(), "s", "b")), Some(&sc))
            .await
            .unwrap_err();
        assert!(
            e.0.contains(bad) && e.0.contains("granted recipients"),
            "the refusal must name the address: {}",
            e.0
        );
    }

    // And the two that are granted still work.
    assert!(SsmtpConnector::new()
        .call(
            "ssmtp/send",
            Some(args("exact@other.net".into(), "s", "b")),
            Some(&scope_for(port, &["@example.com", "exact@other.net"])),
        )
        .await
        .is_ok());
}

/// Header injection: the vector this connector exists to close.
#[tokio::test]
async fn a_newline_in_a_header_is_refused_rather_than_escaped() {
    let (port, _) = fake_relay().await;
    let c = SsmtpConnector::new();
    let sc = scope_for(port, &["@example.com"]);

    // A subject that would add a Bcc and send the rest as body.
    let e = c
        .call(
            "ssmtp/send",
            Some(args(
                "a@example.com".into(),
                "Hi\r\nBcc: everyone@example.com",
                "b",
            )),
            Some(&sc),
        )
        .await
        .unwrap_err();
    assert!(e.0.contains("line ending"), "{}", e.0);

    // And in a recipient, which would forge an envelope.
    let e = c
        .call(
            "ssmtp/send",
            Some(args(
                "a@example.com\r\nRCPT TO:<x@evil.net>".into(),
                "s",
                "b",
            )),
            Some(&sc),
        )
        .await
        .unwrap_err();
    assert!(e.0.contains("line ending"), "{}", e.0);
}

/// A body line of a single `.` ends DATA. Without dot-stuffing a guest
/// closes the message early and the rest is read as SMTP commands.
#[tokio::test]
async fn a_lone_dot_in_the_body_cannot_end_the_message() {
    let (port, log) = fake_relay().await;
    let c = SsmtpConnector::new();
    let sc = scope_for(port, &["@example.com"]);

    c.call(
        "ssmtp/send",
        Some(args(
            "a@example.com".into(),
            "s",
            "before\n.\nMAIL FROM:<forged@evil.net>\nafter",
        )),
        Some(&sc),
    )
    .await
    .unwrap();

    let sent = log.lock().unwrap().clone();
    // The forged command was body, not a command: the relay recorded it
    // between DATA and the terminator, and never answered it.
    let data_at = sent.iter().position(|l| l == "DATA").unwrap();
    let end_at = sent.iter().rposition(|l| l == ".").unwrap();
    assert!(
        sent[data_at..end_at]
            .iter()
            .any(|l| l.contains("forged@evil.net")),
        "the forged line must land inside DATA: {sent:?}"
    );
    // Exactly one terminator, and it is the last line before QUIT.
    assert_eq!(
        sent.iter().filter(|l| *l == ".").count(),
        1,
        "the lone dot was stuffed, so only our terminator ends DATA: {sent:?}"
    );
    assert!(sent[data_at..end_at].iter().any(|l| l == ".."), "{sent:?}");
}

/// Startup refusals. Each is a deployment that would otherwise fail at 3am,
/// or quietly do the wrong thing forever.
#[test]
fn an_ill_formed_scope_is_refused_at_startup_by_name() {
    let ty = SsmtpConnector::new().scope_type();
    let m = |pairs: Vec<(&str, rmpv::Value)>| {
        Scope(rmpv::Value::Map(
            pairs.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        ))
    };
    let ok_allow = || rmpv::Value::Array(vec!["@example.com".into()]);

    assert!(ty.validate(None).is_err(), "no scope at all");

    // The one worth having: a credential with no TLS under it. AUTH PLAIN
    // is base64, not encryption.
    let why = ty
        .validate(Some(&m(vec![
            ("host", "relay".into()),
            ("from", "a@b.net".into()),
            ("allow", ok_allow()),
            ("user", "u".into()),
            ("pass", "p".into()),
            ("starttls", rmpv::Value::Boolean(false)),
        ])))
        .unwrap_err();
    assert!(why.contains("in the clear"), "{why}");

    // An empty allowlist answers nothing; say so at boot.
    let why = ty
        .validate(Some(&m(vec![
            ("host", "relay".into()),
            ("from", "a@b.net".into()),
            ("allow", rmpv::Value::Array(vec![])),
        ])))
        .unwrap_err();
    assert!(why.contains("refuse every recipient"), "{why}");

    // Half a credential.
    assert!(ty
        .validate(Some(&m(vec![
            ("host", "relay".into()),
            ("from", "a@b.net".into()),
            ("allow", ok_allow()),
            ("user", "u".into()),
        ])))
        .is_err());

    // A sender carrying a newline is a forged header at every send.
    assert!(ty
        .validate(Some(&m(vec![
            ("host", "relay".into()),
            ("from", "a@b.net\r\nBcc: x@y.net".into()),
            ("allow", ok_allow()),
        ])))
        .is_err());

    // An allow entry that is not an address or a domain.
    assert!(ty
        .validate(Some(&m(vec![
            ("host", "relay".into()),
            ("from", "a@b.net".into()),
            ("allow", rmpv::Value::Array(vec!["example.com".into()])),
        ])))
        .is_err());

    // And a well-formed one passes, so the above measures refusals rather
    // than a validator that refuses everything.
    ty.validate(Some(&m(vec![
        ("host", "relay".into()),
        ("from", "Name <a@b.net>".into()),
        ("allow", ok_allow()),
        ("user", "u".into()),
        ("pass", "p".into()),
    ])))
    .unwrap();
}

#[tokio::test]
async fn an_unknown_call_is_refused() {
    let (port, _) = fake_relay().await;
    SsmtpConnector::new()
        .call("ssmtp/receive", None, Some(&scope_for(port, &["@a.com"])))
        .await
        .expect_err("only ssmtp/send exists");
}

/// FM-3, both halves. `drt run` has no reactor; a connector that dies there
/// is a connector that shipped uncallable, twice already.
#[test]
fn a_call_with_no_reactor_refuses_rather_than_panicking() {
    // A port nothing is listening on, reached through the allowlist so the
    // refusal comes from the socket rather than from the scope.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let out = pollster::block_on(SsmtpConnector::new().call(
        "ssmtp/send",
        Some(args("a@example.com".into(), "s", "b")),
        Some(&scope_for(port, &["@example.com"])),
    ));
    out.expect_err("nothing is listening; this cannot succeed");
}

/// `args` for a reply: the threading fields, each present only if given.
fn reply_args(in_reply_to: Option<rmpv::Value>, references: Option<rmpv::Value>) -> rmpv::Value {
    let mut map = vec![
        ("to".into(), "a@example.com".into()),
        ("subject".into(), "Re: Hello".into()),
        ("body".into(), "b".into()),
    ];
    if let Some(v) = in_reply_to {
        map.push(("in_reply_to".into(), v));
    }
    if let Some(v) = references {
        map.push(("references".into(), v));
    }
    rmpv::Value::Map(map)
}

/// The header block as a receiver reads it: the lines between DATA and the
/// blank line, each folded continuation joined back onto its field.
fn unfolded_headers(sent: &[String]) -> Vec<String> {
    let data_at = sent.iter().position(|l| l == "DATA").unwrap();
    let mut out: Vec<String> = Vec::new();
    for line in &sent[data_at + 1..] {
        if line.is_empty() {
            break;
        }
        match out.last_mut() {
            Some(field) if line.starts_with(' ') || line.starts_with('\t') => field.push_str(line),
            _ => out.push(line.clone()),
        }
    }
    out
}

/// Send one reply through the fake relay and hand back the wire.
async fn wire_of(args: rmpv::Value) -> Result<Vec<String>, String> {
    let (port, log) = fake_relay().await;
    SsmtpConnector::new()
        .call(
            "ssmtp/send",
            Some(args),
            Some(&scope_for(port, &["@example.com"])),
        )
        .await
        .map_err(|e| e.0)?;
    let sent = log.lock().unwrap().clone();
    Ok(sent)
}

/// Threading: a reply names what it answers, and the wire carries both
/// lines where a client looks for them.
#[tokio::test]
async fn a_reply_carries_the_thread_it_answers() {
    let sent = wire_of(reply_args(
        Some("<parent@example.com>".into()),
        Some(rmpv::Value::Array(vec![
            "<root@example.com>".into(),
            "<parent@example.com>".into(),
        ])),
    ))
    .await
    .unwrap();

    let at = |line: &str| {
        sent.iter()
            .position(|l| l == line)
            .unwrap_or_else(|| panic!("{line:?} is not on the wire: {sent:?}"))
    };
    let in_reply_to = at("In-Reply-To: <parent@example.com>");
    let references = at("References: <root@example.com> <parent@example.com>");
    // Under the scope's From and the guest's Subject and above the MIME
    // fields: never above the one line the guest cannot set.
    assert!(at("From: Michael at Discofetch <no-reply@discofetch.net>") < in_reply_to);
    assert!(at("Subject: Re: Hello") < in_reply_to);
    assert!(in_reply_to < references && references < at("MIME-Version: 1.0"));
}

/// The common reply knows only the parent's id. `References` is then that
/// id, which is what a reply to a thread's first message carries; and a
/// message that is not a reply carries neither line.
#[tokio::test]
async fn references_defaults_to_in_reply_to_and_neither_appears_unasked() {
    let sent = wire_of(reply_args(Some("<parent@example.com>".into()), None))
        .await
        .unwrap();
    assert!(
        sent.iter()
            .any(|l| l == "In-Reply-To: <parent@example.com>"),
        "{sent:?}"
    );
    assert!(
        sent.iter().any(|l| l == "References: <parent@example.com>"),
        "{sent:?}"
    );

    let sent = wire_of(args("a@example.com".into(), "Hello", "b"))
        .await
        .unwrap();
    assert!(
        !sent
            .iter()
            .any(|l| l.starts_with("In-Reply-To:") || l.starts_with("References:")),
        "{sent:?}"
    );
}

/// A copied header is the shape a program has: brackets present or not
/// (JMAP strips them), folded across lines or not. Both arrive as ids, and
/// a line ending inside the field is a fold — what follows it is one more
/// id inside the same header, never a header of its own.
#[tokio::test]
async fn ids_are_bracketed_and_a_fold_is_a_separator_not_a_header() {
    let sent = wire_of(reply_args(
        Some("parent@example.com".into()),
        Some("<root@example.com>\r\n <parent@example.com>\r\nBcc: everyone@example.com".into()),
    ))
    .await
    .unwrap();
    let fields = unfolded_headers(&sent);
    assert!(
        fields
            .iter()
            .any(|l| l == "In-Reply-To: <parent@example.com>"),
        "{sent:?}"
    );
    assert!(
        fields.iter().any(|l| l
            == "References: <root@example.com> <parent@example.com> <Bcc:> <everyone@example.com>"),
        "{sent:?}"
    );
    assert!(!fields.iter().any(|l| l.starts_with("Bcc:")), "{sent:?}");
    // Four ids pass the fold, so the wire itself carried a continuation
    // line — the shape the guest's copy arrived in.
    assert!(sent.iter().any(|l| l.starts_with(" <")), "{sent:?}");
}

/// What a message id may not be, each refused by name and by field.
#[tokio::test]
async fn a_message_id_that_is_not_one_is_refused_by_name() {
    for (bad, why) in [
        ("<unclosed@example.com", "does not close"),
        ("<a@b>(comment)", "does not close"),
        ("a<b>c@example.com", "not a message id"),
        ("<a@b>,", "does not close"),
        ("", "names no message id"),
        ("   ", "names no message id"),
    ] {
        let e = wire_of(reply_args(Some(bad.into()), None))
            .await
            .unwrap_err();
        assert!(e.contains(why), "{bad:?}: {e}");
        assert!(
            e.starts_with("in_reply_to"),
            "the refusal names the field: {e}"
        );
    }
    let e = wire_of(reply_args(None, Some(rmpv::Value::from(7))))
        .await
        .unwrap_err();
    assert!(e.contains("neither a message id nor a list"), "{e}");

    // The bounds: one id that will not fit its line, and a thread longer
    // than any client keeps.
    let long = format!("<{}@example.com>", "x".repeat(MAX_MSGID_BYTES));
    let e = wire_of(reply_args(Some(long.into()), None))
        .await
        .unwrap_err();
    assert!(e.contains("bound for one message id"), "{e}");
    let many: Vec<rmpv::Value> = (0..=MAX_REFERENCES)
        .map(|i| format!("<{i}@example.com>").into())
        .collect();
    let e = wire_of(reply_args(None, Some(rmpv::Value::Array(many))))
        .await
        .unwrap_err();
    assert!(e.contains("is the bound for one message"), "{e}");
}

/// Folding, on the function itself: no line past the fold, every
/// continuation line begins with a space, and the ids read back whole.
#[test]
fn a_long_thread_folds_and_reads_back_whole() {
    let ids: Vec<String> = (0..12)
        .map(|i| format!("<{i:0>24}@mail.example.com>"))
        .collect();
    let folded = fold_ids("References", &ids);
    let lines: Vec<&str> = folded.strip_suffix("\r\n").unwrap().split("\r\n").collect();
    assert!(lines.len() > 1, "{folded:?}");
    assert!(lines[0].starts_with("References: <"), "{folded:?}");
    for line in &lines {
        assert!(line.len() <= FOLD_AT_BYTES, "{line:?}");
    }
    for line in &lines[1..] {
        assert!(line.starts_with(' '), "{line:?}");
    }
    // Unfolded — the CRLF removed, as a receiver does — it is the list.
    let value = folded.replace("\r\n", "");
    let back = msg_ids_of(
        "references",
        &value.strip_prefix("References:").unwrap().into(),
    )
    .unwrap();
    assert_eq!(back, ids);

    // A single id longer than the fold stays on the field's own line.
    let long = vec![format!("<{}@example.com>", "x".repeat(2 * FOLD_AT_BYTES))];
    assert_eq!(fold_ids("In-Reply-To", &long).matches("\r\n").count(), 1);
}
