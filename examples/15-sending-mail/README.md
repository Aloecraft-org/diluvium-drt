# 15 — Sending mail

`host:ssmtp/send`, and the reason it is a connector rather than something a
program does with `rest`.

```
python3 relay.py 2 &          # a fake relay, so this runs anywhere
drt run --config deploy.json
```

`./demo.sh` does both and prints the transcript.

## What the program cannot do

Read `app.dlua`: it names recipients, subjects and bodies. It never names
the relay, the password, or who the mail is from — those are in
`deploy.json`, and the connector supplies them. That is the same trick
`05-calling-a-rest-api` uses for an API credential, and it is worth more
here, because the sender is an identity.

The second granted send asks for it anyway, with a subject of
`From: me@evil.example`. Look at where it lands:

```
From: Notifications <no-reply@example.com>      <- the deployment's
To: someone@example.com
Subject: From: me@evil.example                  <- the program's
```

A subject is only ever a subject. There is no argument that sets `From`,
which is not a validation rule to get right but a surface that does not
exist.

## The three refusals, and only one is about recipients

**`@example.com` is a domain, matched exactly.** `evil.example.net` merely
ends with `example`, and `example.com.evil.net` merely starts with the
granted name. Both are refused, both by name — a refusal that does not say
which address it refused sends you to guess among four.

**A header may not carry a line ending.** `Hi\r\nBcc: everyone@…` would end
the subject and start a `Bcc`. It is refused rather than escaped: a subject
that wanted a newline wanted something else.

**A body may not end the message.** A line of exactly `.` is what closes an
SMTP message, so a program could otherwise stop its own message early and
have the rest read as commands. The transcript shows the body's `.` arriving
as `..`, which is the wire saying "this is text".

## Wiring it to a real relay

Change `host`, `port` and `from` in `deploy.json`. For a submission port,
keep `starttls` on and add `user` and `pass`:

```json
"port": 587, "starttls": true, "user": "…", "pass": "…"
```

A scope naming a user with `starttls` off is refused **at startup**. `AUTH
PLAIN` is base64, not encryption, and a deployment that would put a password
on the wire should find out at boot rather than at 3am.

`from` must be an address the relay will carry — for SES, one it has
verified. This is discofetch's `deploy/mail/` arrangement with the daemon
removed: the same relay, the same options, reached from inside the program
instead of from a puller beside it.
