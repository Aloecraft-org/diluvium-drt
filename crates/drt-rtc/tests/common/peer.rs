//! A native client that does what a browser does with a host's record:
//! builds its session with no answer round trip, controls ICE, is the DTLS
//! and SCTP client, and speaks Wisp v1 on channel 1. Shared by
//! `tests/host.rs` here and `crates/drt/tests/signal.rs`, which include it
//! by path; it is test code, and no crate exports it.
//!
//! ## surface block
//!
//! - Entry points: [`Client::new`], then [`Client::connect`], [`Client::send`],
//!   [`Client::next_packet`], [`Client::read_stream`]; [`Client::until`]
//!   and [`Client::within`] to run it.
//! - Configurable: [`LIMIT`], how long any one wait may take.
//! - Fan-out: [`Client::drain`]'s match over str0m's output.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use drt_rtc::wisp::{self, Packet};
use drt_rtc::Record;
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::config::Fingerprint;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event as RtcEvent, IceCreds, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::time::Instant;

pub const LIMIT: Duration = Duration::from_secs(10);

// depth: the browser

pub struct Client {
    pub rtc: Rtc,
    pub socket: UdpSocket,
    pub addr: SocketAddr,
    pub control: ChannelId,
    pub wisp: ChannelId,
    pub open: [bool; 2],
    pub connected: bool,
    pub hello: Option<String>,
    pub inbox: Vec<Vec<u8>>,
}

impl Client {
    /// What a browser does with the host's record: everything but the SDP.
    pub async fn new(host: &Record) -> (Client, String) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let creds = IceCreds::new();
        let mut rtc = Rtc::builder()
            .set_local_ice_credentials(creds.clone())
            .build(Instant::now().into_std());
        let local = Candidate::host(addr, "udp").unwrap();
        rtc.add_local_candidate(local.clone());
        let fingerprint: [u8; 32] = rtc
            .direct_api()
            .local_dtls_fingerprint()
            .bytes
            .clone()
            .try_into()
            .unwrap();
        let mut api = rtc.direct_api();
        api.set_ice_controlling(true);
        api.set_remote_ice_credentials(IceCreds {
            ufrag: host.ufrag.clone(),
            pass: host.pwd.clone(),
        });
        api.set_remote_fingerprint(Fingerprint {
            hash_func: "sha-256".into(),
            bytes: host.fingerprint.to_vec(),
        });
        api.start_dtls(true).unwrap();
        api.start_sctp(true);
        let channel = |label: &str, id: u16| ChannelConfig {
            label: label.into(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: Some(id),
            protocol: String::new(),
        };
        let control = api.create_data_channel(channel("control", 0));
        let wisp = api.create_data_channel(channel("wisp", 1));
        for c in &host.candidates {
            rtc.add_remote_candidate(Candidate::from_sdp_string(c).unwrap());
        }
        let record = Record {
            ufrag: creds.ufrag,
            pwd: creds.pass,
            fingerprint,
            candidates: vec![local.to_sdp_string()],
        };
        let client = Client {
            rtc,
            socket,
            addr,
            control,
            wisp,
            open: [false; 2],
            connected: false,
            hello: None,
            inbox: Vec::new(),
        };
        (client, record.encode().unwrap())
    }

    pub fn drain(&mut self) -> Instant {
        loop {
            match self.rtc.poll_output().unwrap() {
                Output::Timeout(t) => return Instant::from_std(t),
                Output::Transmit(t) => {
                    let _ = self.socket.try_send_to(&t.contents, t.destination);
                }
                Output::Event(e) => match e {
                    RtcEvent::Connected => self.connected = true,
                    RtcEvent::ChannelOpen(id, _) if id == self.control => self.open[0] = true,
                    RtcEvent::ChannelOpen(id, _) if id == self.wisp => self.open[1] = true,
                    RtcEvent::ChannelData(d) if d.id == self.control => {
                        self.hello = Some(String::from_utf8(d.data).unwrap())
                    }
                    RtcEvent::ChannelData(d) if d.id == self.wisp => self.inbox.push(d.data),
                    _ => {}
                },
            }
        }
    }

    /// Run the client until `done` says so, or fail the test at [`LIMIT`].
    pub async fn until(&mut self, what: &str, mut done: impl FnMut(&mut Client) -> bool) {
        let deadline = Instant::now() + LIMIT;
        let mut buf = vec![0u8; 2000];
        loop {
            let wake = self.drain();
            if done(self) {
                return;
            }
            let now = Instant::now();
            assert!(now < deadline, "the client gave up waiting for {what}");
            let wait = wake
                .min(deadline)
                .saturating_duration_since(now)
                .max(Duration::from_millis(1));
            let input = match tokio::time::timeout(wait, self.socket.recv_from(&mut buf)).await {
                Ok(Ok((n, source))) => Input::Receive(
                    Instant::now().into_std(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: self.addr,
                        contents: buf[..n].try_into().unwrap(),
                    },
                ),
                _ => Input::Timeout(Instant::now().into_std()),
            };
            self.rtc.handle_input(input).unwrap();
        }
    }

    /// Run the client for `within`, and say whether `done` came true.
    /// For the tests that prove something does not happen.
    pub async fn within(
        &mut self,
        within: Duration,
        mut done: impl FnMut(&mut Client) -> bool,
    ) -> bool {
        let deadline = Instant::now() + within;
        let mut buf = vec![0u8; 2000];
        loop {
            let wake = self.drain();
            if done(self) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let wait = wake
                .min(deadline)
                .saturating_duration_since(now)
                .max(Duration::from_millis(1));
            let input = match tokio::time::timeout(wait, self.socket.recv_from(&mut buf)).await {
                Ok(Ok((n, source))) => Input::Receive(
                    Instant::now().into_std(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: self.addr,
                        contents: buf[..n].try_into().unwrap(),
                    },
                ),
                _ => Input::Timeout(Instant::now().into_std()),
            };
            self.rtc.handle_input(input).unwrap();
        }
    }

    pub async fn connect(&mut self) {
        self.until("both channels to open", |c| c.open == [true, true])
            .await;
        self.until("hello on control", |c| c.hello.is_some()).await;
        let credit = self.next_packet().await;
        assert_eq!(
            credit,
            wisp::cont(0, 128),
            "CONTINUE on stream 0 comes first"
        );
    }

    pub fn send(&mut self, pkt: Vec<u8>) {
        assert!(self
            .rtc
            .channel(self.wisp)
            .unwrap()
            .write(true, &pkt)
            .unwrap());
    }

    pub async fn next_packet(&mut self) -> Vec<u8> {
        self.until("a wisp packet", |c| !c.inbox.is_empty()).await;
        self.inbox.remove(0)
    }

    /// Everything that arrives for `stream`, until its CLOSE or `want`
    /// bytes of DATA, returned as (data, close reason).
    pub async fn read_stream(&mut self, stream: u32, want: usize) -> (Vec<u8>, Option<u8>) {
        let mut data = Vec::new();
        loop {
            let pkt = self.next_packet().await;
            match wisp::parse(&pkt).unwrap() {
                Packet::Data { stream: s, payload } if s == stream => {
                    data.extend_from_slice(payload);
                    if data.len() >= want {
                        return (data, None);
                    }
                }
                Packet::Close { stream: s, reason } if s == stream => return (data, Some(reason)),
                _ => {}
            }
        }
    }
}
