//! The WireGuard device as DRT runs it.
//!
//! The protocol is gotatun's and tested there. What these check is DRT's
//! half — that the config translates, that the refusals name what was
//! wrong, that the command vocabulary a supervisor writes is the one this
//! reads, and, the one that matters, **that two devices actually carry a
//! packet between them**.
//!
//! That last test is why the device is generic over its IP transport.
//! `drt start` gives it a kernel interface, which needs CAP_NET_ADMIN and
//! so cannot run in CI. These give it a pair of channels instead, which
//! makes the whole thing an ordinary unprivileged test: two devices, two
//! loopback UDP sockets, a real handshake, a real IPv4 packet in one end
//! and the same bytes out the other. Nothing below the tunnel is mocked —
//! if the handshake or the cryptokey routing were wrong, no packet would
//! arrive.

#![cfg(feature = "wireguard")]

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use base64::Engine as _;
use drt::wireguard::{command_from, private_key, public_key, Command, KEY_LEN};
use drt_config::{WireguardConfig, WireguardPeer};
use gotatun::device::{Device, DeviceBuilder, DeviceTransports, Peer};
use gotatun::packet::{Ip, Packet, PacketBufPool};
use gotatun::tun::{IpRecv, IpSend, MtuWatcher};
use gotatun::x25519::StaticSecret;
use tokio::sync::{mpsc, watch};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// One runtime for the whole binary, deliberately never dropped — the same
/// tokio 1.53.1 teardown use-after-free `tests/stun.rs` documents at
/// length. A `static` is never dropped, so the path never runs.
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("a tokio runtime"))
}

// ---------------------------------------------------------------------------
// A tunnel interface that is two channels, which is the whole trick
// ---------------------------------------------------------------------------

/// The device's outbound IP side: decrypted packets arrive here.
struct ChannelTx(mpsc::UnboundedSender<Vec<u8>>);

impl IpSend for ChannelTx {
    async fn send(&mut self, packet: Packet<Ip>) -> io::Result<()> {
        let _ = self.0.send(packet.into_bytes().to_vec());
        Ok(())
    }
}

/// The device's inbound IP side: packets written here are encrypted and
/// sent to whichever peer owns their destination.
struct ChannelRx {
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mtu: MtuWatcher,
}

impl IpRecv for ChannelRx {
    async fn recv<'a>(
        &'a mut self,
        pool: &mut PacketBufPool,
    ) -> io::Result<impl Iterator<Item = Packet<Ip>> + 'a> {
        let bytes = self
            .rx
            .recv()
            .await
            .ok_or_else(|| io::Error::other("the test closed the tun channel"))?;
        let mut packet = pool.get();
        packet[..bytes.len()].copy_from_slice(&bytes);
        packet.truncate(bytes.len());
        match packet.try_into_ip() {
            Ok(packet) => Ok(std::iter::once(packet)),
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    }

    fn mtu(&self) -> MtuWatcher {
        self.mtu.clone()
    }
}

/// A tunnel interface made of channels: what the test writes, and what the
/// test reads.
struct Tun {
    /// Write an IP packet here and the device will encrypt and send it.
    inject: mpsc::UnboundedSender<Vec<u8>>,
    /// Decrypted packets the device received come out here.
    observe: mpsc::UnboundedReceiver<Vec<u8>>,
}

fn channel_tun() -> (Tun, ChannelTx, ChannelRx) {
    let (inject, in_rx) = mpsc::unbounded_channel();
    let (out_tx, observe) = mpsc::unbounded_channel();
    let (_mtu_tx, mtu_rx) = watch::channel(1420u16);
    // The sender is leaked with the watcher: nothing in a test changes an
    // MTU, and a dropped sender would make every read fail.
    std::mem::forget(_mtu_tx);
    (
        Tun { inject, observe },
        ChannelTx(out_tx),
        ChannelRx {
            rx: in_rx,
            mtu: mtu_rx.into(),
        },
    )
}

/// A port nothing holds. Bound and released, the way `tests/stun.rs` finds
/// one: the device needs a port it can name in the *other* device's
/// config, so `0` is no use here.
fn free_port() -> u16 {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = sock.local_addr().unwrap().port();
    drop(sock);
    port
}

/// One end of the tunnel, built.
async fn device(
    secret: StaticSecret,
    port: u16,
    peer: Peer,
    tx: ChannelTx,
    rx: ChannelRx,
) -> Device<impl DeviceTransports> {
    DeviceBuilder::new()
        .with_default_udp()
        .with_ip_pair(tx, rx)
        .with_listen_port(port)
        .with_private_key(secret)
        .with_peer(peer)
        .build()
        .await
        .expect("the device came up")
}

/// A well-formed IPv4/UDP packet, with the header checksum computed: the
/// device routes on the destination and the far side's cryptokey routing
/// checks the source, so both have to be real.
fn ipv4_udp(src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
    let udp_len = 8 + payload.len();
    let total = 20 + udp_len;
    let mut p = Vec::with_capacity(total);
    p.extend_from_slice(&[0x45, 0x00]); // v4, IHL 5, DSCP 0
    p.extend_from_slice(&(total as u16).to_be_bytes());
    p.extend_from_slice(&[0x00, 0x01, 0x00, 0x00]); // id, no flags/fragment
    p.extend_from_slice(&[64, 17]); // ttl, UDP
    p.extend_from_slice(&[0x00, 0x00]); // checksum, filled below
    p.extend_from_slice(&src.octets());
    p.extend_from_slice(&dst.octets());
    let mut sum = 0u32;
    for chunk in p[..20].chunks(2) {
        sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    let checksum = !(sum as u16);
    p[10..12].copy_from_slice(&checksum.to_be_bytes());
    p.extend_from_slice(&7777u16.to_be_bytes()); // source port
    p.extend_from_slice(&7777u16.to_be_bytes()); // destination port
    p.extend_from_slice(&(udp_len as u16).to_be_bytes());
    p.extend_from_slice(&[0x00, 0x00]); // UDP checksum: optional over IPv4
    p.extend_from_slice(payload);
    p
}

/// Wait for a packet whose payload is `wanted`, or say what did arrive.
async fn expect_payload(tun: &mut Tun, wanted: &[u8]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "nothing carrying {wanted:?} ever arrived");
        match tokio::time::timeout(left, tun.observe.recv()).await {
            Ok(Some(packet)) => {
                if packet.ends_with(wanted) {
                    return;
                }
                // Anything else is a keepalive or a retransmit; keep waiting.
            }
            Ok(None) => panic!("the tunnel closed before the packet arrived"),
            Err(_) => panic!("nothing carrying {wanted:?} arrived within ten seconds"),
        }
    }
}

/// The next report that is not an empty snapshot.
///
/// `report_ms` ticks whether or not anything changed (issue #15), and a
/// device with no peers yet has an empty snapshot to send on every tick.
/// That is the clock, not an event, so a test watching for a particular
/// event looks past it. Anything else -- including a snapshot that has a
/// peer in it -- is returned for the caller to judge.
async fn expect_report(
    reports: &mut mpsc::UnboundedReceiver<drt::wireguard::Report>,
    within: Duration,
) -> drt::wireguard::Report {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "no report arrived within {within:?}");
        match tokio::time::timeout(left, reports.recv()).await {
            Ok(Some(drt::wireguard::Report::Peers(peers))) if peers.is_empty() => continue,
            Ok(Some(report)) => return report,
            Ok(None) => panic!("the report channel closed"),
            Err(_) => panic!("no report arrived within {within:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The one that matters
// ---------------------------------------------------------------------------

/// Two devices, a real handshake, and a real packet across it — with no
/// kernel interface and no privilege, because the IP side is a trait.
///
/// This is the whole claim of the feature in one test: if DRT can bring up
/// two of these and pass a packet, then a deployment that has punched a
/// hole has an encrypted link over it.
#[test]
fn a_packet_crosses_between_two_devices_with_no_kernel_and_no_privilege() {
    rt().block_on(async {
        // Fixed rather than random: a test that fails should fail the same
        // way twice. `StaticSecret::from` clamps, so any 32 bytes are a
        // key.
        let (secret_a, secret_b) = (
            StaticSecret::from([0x11u8; KEY_LEN]),
            StaticSecret::from([0x22u8; KEY_LEN]),
        );
        let (pub_a, pub_b) = (
            gotatun::x25519::PublicKey::from(&secret_a),
            gotatun::x25519::PublicKey::from(&secret_b),
        );
        let (port_a, port_b) = (free_port(), free_port());
        let (ip_a, ip_b) = (Ipv4Addr::new(10, 9, 0, 1), Ipv4Addr::new(10, 9, 0, 2));

        // A knows B is at port_b and owns 10.9.0.2; B knows A owns
        // 10.9.0.1. Both directions are needed: the first is routing, the
        // second is the cryptokey routing that decides whether an arriving
        // packet is accepted at all.
        let peer_b = Peer::new(pub_b)
            .with_endpoint(SocketAddr::from(([127, 0, 0, 1], port_b)))
            .with_allowed_ip(ipnetwork::IpNetwork::from(std::net::IpAddr::V4(ip_b)));
        let peer_a = Peer::new(pub_a)
            .with_endpoint(SocketAddr::from(([127, 0, 0, 1], port_a)))
            .with_allowed_ip(ipnetwork::IpNetwork::from(std::net::IpAddr::V4(ip_a)));

        let (mut tun_a, tx_a, rx_a) = channel_tun();
        let (mut tun_b, tx_b, rx_b) = channel_tun();
        let _a = device(secret_a, port_a, peer_b, tx_a, rx_a).await;
        let _b = device(secret_b, port_b, peer_a, tx_b, rx_b).await;

        // A sends. The first packet triggers the handshake and may be
        // dropped while it completes, which is WireGuard's own behaviour,
        // so send until one lands.
        let packet = ipv4_udp(ip_a, ip_b, b"through the tunnel");
        let inject = tun_a.inject.clone();
        let sender = tokio::spawn(async move {
            for _ in 0..100 {
                if inject.send(packet.clone()).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        expect_payload(&mut tun_b, b"through the tunnel").await;
        sender.abort();

        // And back the other way, on the session the first exchange
        // established: a tunnel that only works in the direction it was
        // opened is not a tunnel.
        let reply = ipv4_udp(ip_b, ip_a, b"and back again");
        tun_b.inject.send(reply).unwrap();
        expect_payload(&mut tun_a, b"and back again").await;
    });
}

/// The punch, in the small: a peer with **no endpoint at all** is
/// unreachable until something tells the device where it is, and the
/// `endpoint` command is that something.
///
/// This is the shape a rendezvous produces — neither side knows the
/// other's address until STUN measures it and the relay carries it — so
/// this test is the deployment-side half of a hole punch with the
/// rendezvous replaced by a channel send.
#[test]
fn a_peer_with_no_endpoint_is_unreachable_until_the_endpoint_command_arrives() {
    rt().block_on(async {
        // Fixed rather than random: a test that fails should fail the same
        // way twice. `StaticSecret::from` clamps, so any 32 bytes are a
        // key.
        let (secret_a, secret_b) = (
            StaticSecret::from([0x11u8; KEY_LEN]),
            StaticSecret::from([0x22u8; KEY_LEN]),
        );
        let (pub_a, pub_b) = (
            gotatun::x25519::PublicKey::from(&secret_a),
            gotatun::x25519::PublicKey::from(&secret_b),
        );
        let (port_a, port_b) = (free_port(), free_port());
        let (ip_a, ip_b) = (Ipv4Addr::new(10, 9, 0, 1), Ipv4Addr::new(10, 9, 0, 2));

        // A has B as a peer with NO endpoint: it can route to it and
        // cannot reach it.
        let peer_b = Peer::new(pub_b)
            .with_allowed_ip(ipnetwork::IpNetwork::from(std::net::IpAddr::V4(ip_b)));
        let peer_a = Peer::new(pub_a)
            .with_allowed_ip(ipnetwork::IpNetwork::from(std::net::IpAddr::V4(ip_a)));

        let (tun_a, tx_a, rx_a) = channel_tun();
        let (mut tun_b, tx_b, rx_b) = channel_tun();
        let a = device(secret_a, port_a, peer_b, tx_a, rx_a).await;
        let _b = device(secret_b, port_b, peer_a, tx_b, rx_b).await;

        // Drive A the way `drt start` drives it, and keep sending.
        let (reports, report_rx) = mpsc::unbounded_channel();
        let (commands, command_rx) = mpsc::unbounded_channel();
        let driver = tokio::spawn(drt::wireguard::drive(
            a,
            Duration::from_millis(50),
            reports,
            command_rx,
            None,
        ));
        let packet = ipv4_udp(ip_a, ip_b, b"after the rendezvous");
        let inject = tun_a.inject.clone();
        let sender = tokio::spawn(async move {
            for _ in 0..100 {
                if inject.send(packet.clone()).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });

        // Nothing arrives: A has nowhere to send.
        let quiet = tokio::time::timeout(Duration::from_millis(600), tun_b.observe.recv()).await;
        assert!(
            quiet.is_err(),
            "a peer with no endpoint was somehow reached: {quiet:?}"
        );

        // The rendezvous learned where B is. This is the whole command.
        // One command, carrying the keepalive too: a punched mapping with
        // no traffic through it closes again in tens of seconds, and two
        // commands to open one hole is one more chance to send only the
        // first.
        commands
            .send(Command::Endpoint {
                public_key: *pub_b.as_bytes(),
                endpoint: SocketAddr::from(([127, 0, 0, 1], port_b)),
                keepalive: Some(25),
            })
            .unwrap();

        expect_payload(&mut tun_b, b"after the rendezvous").await;
        sender.abort();

        // And the deployment is told, on the same channel `drt start`
        // pushes onto its queue, that the peer now has an endpoint and a
        // completed handshake — which is how a program knows the punch
        // worked rather than assuming it.
        let mut report_rx = report_rx;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no report ever showed a handshake"
            );
            let Ok(Some(report)) =
                tokio::time::timeout(Duration::from_secs(10), report_rx.recv()).await
            else {
                panic!("the report channel closed")
            };
            if let drt::wireguard::Report::Peers(peers) = report {
                let peer = &peers[0];
                if peer.endpoint.is_some() && peer.last_handshake_ms.is_some() {
                    assert_eq!(peer.public_key, B64.encode(pub_b.as_bytes()));
                    assert!(peer.tx_bytes > 0, "{peer:?}");
                    break;
                }
            }
        }
        driver.abort();
    });
}

// ---------------------------------------------------------------------------
// The config, and what it refuses
// ---------------------------------------------------------------------------

/// One message, as a supervisor would push it.
fn msg(fields: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        fields
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

/// One field of an rmpv map, for reading a report back.
fn field<'a>(value: &'a rmpv::Value, name: &str) -> &'a rmpv::Value {
    value
        .as_map()
        .expect("a map")
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("no field {name}"))
}

fn scope(key: Option<&str>) -> WireguardConfig {
    WireguardConfig {
        listen_port: 51820,
        interface: "drt0".into(),
        address: Some("10.9.0.1/24".into()),
        mtu: 1420,
        turn_fallback: false,
        stun: Vec::new(),
        private_key: key.map(str::to_string),
        private_key_file: None,
        private_key_env: None,
        peers: Vec::new(),
        queue: "wg_in".into(),
        reply_queue: String::new(),
        report_ms: 10_000,
    }
}

/// A key is 32 bytes, base64, and anything else is a paste that went
/// wrong rather than a setting. The two failures worth refusing loudly are
/// a truncated key and a key that is not base64 at all: both would
/// otherwise appear as handshakes that never complete.
#[test]
fn a_key_that_is_not_a_key_is_refused_by_name() {
    let good = B64.encode([7u8; KEY_LEN]);
    assert_eq!(
        drt::wireguard::parse_key("k", &good).unwrap(),
        [7u8; KEY_LEN]
    );
    // Whitespace from a file or a heredoc is trimmed, not refused.
    assert!(drt::wireguard::parse_key("k", &format!("  {good}\n")).is_ok());

    let err = drt::wireguard::parse_key("k", "not base64!").unwrap_err();
    assert!(err.contains("not base64"), "{err}");
    let err = drt::wireguard::parse_key("k", &B64.encode([1u8; 16])).unwrap_err();
    assert!(err.contains("32 bytes"), "{err}");
    assert!(err.contains("16"), "{err}");

    // No key at all, and an env var that is not set: both name themselves.
    // (`StaticSecret` is not `Debug`, deliberately, so `unwrap_err` cannot
    // say what bound instead -- which is the right trade for a key.)
    let refusal = |config: &WireguardConfig| match private_key(config) {
        Ok(_) => panic!("a config with no key produced one"),
        Err(e) => e,
    };
    let err = refusal(&scope(None));
    assert!(
        err.contains("private_key_file, private_key_env, private_key"),
        "{err}"
    );
    let mut unset = scope(None);
    unset.private_key_env = Some("DRT_TEST_WG_KEY_THAT_IS_NOT_SET".into());
    let err = refusal(&unset);
    assert!(err.contains("DRT_TEST_WG_KEY_THAT_IS_NOT_SET"), "{err}");
}

/// The public key `drt wg` prints is the one a peer must be given, and it
/// is `wg pubkey`'s answer for the same secret.
#[test]
fn the_public_key_printed_is_the_one_a_peer_configures() {
    let secret = StaticSecret::from([9u8; KEY_LEN]);
    let printed = public_key(&secret);
    let expected = B64.encode(gotatun::x25519::PublicKey::from(&secret).as_bytes());
    assert_eq!(printed, expected);
    // Round trips: what is printed parses back as a peer's key.
    assert_eq!(
        drt::wireguard::parse_key("peer", &printed).unwrap(),
        *gotatun::x25519::PublicKey::from(&secret).as_bytes()
    );
}

/// A peer's fields are refused one at a time, each naming the peer it came
/// from — a config with six peers and one bad CIDR should not report
/// "invalid network" and leave the reader to find which.
#[test]
fn a_bad_peer_field_names_the_peer_it_came_from() {
    let key = B64.encode([3u8; KEY_LEN]);
    let base = WireguardPeer {
        public_key: key.clone(),
        allowed_ips: vec!["10.9.0.2/32".into()],
        endpoint: None,
        keepalive: Some(25),
        preshared_key_env: None,
    };
    assert!(drt::wireguard::peer(&base).is_ok());

    let mut bad_cidr = base.clone();
    bad_cidr.allowed_ips = vec!["10.9.0.2/33".into()];
    let err = drt::wireguard::peer(&bad_cidr).unwrap_err();
    assert!(err.contains(&key), "{err}");
    assert!(err.contains("allowed_ips"), "{err}");

    let mut bad_endpoint = base.clone();
    bad_endpoint.endpoint = Some("example.com".into());
    let err = drt::wireguard::peer(&bad_endpoint).unwrap_err();
    assert!(err.contains("host:port"), "{err}");

    let mut unset = base.clone();
    unset.preshared_key_env = Some("DRT_TEST_WG_PSK_THAT_IS_NOT_SET".into());
    let err = drt::wireguard::peer(&unset).unwrap_err();
    assert!(err.contains("DRT_TEST_WG_PSK_THAT_IS_NOT_SET"), "{err}");
}

/// The command vocabulary a supervisor writes is the one the device reads.
#[test]
fn the_reply_queues_commands_are_read_as_written() {
    let key = B64.encode([5u8; KEY_LEN]);

    // The punch, in one message: where the peer is, and the keepalive
    // that holds the hole open once it is.
    assert_eq!(
        command_from(&msg(vec![
            ("command", "endpoint".into()),
            ("public_key", key.as_str().into()),
            ("endpoint", "203.0.113.7:51820".into()),
            ("keepalive", rmpv::Value::from(25u64)),
        ]))
        .unwrap(),
        Command::Endpoint {
            public_key: [5u8; KEY_LEN],
            endpoint: "203.0.113.7:51820".parse().unwrap(),
            keepalive: Some(25),
        }
    );

    // A peer the config never named. A rendezvous DISCOVERS peers -- that
    // is what makes it a rendezvous -- so this is the command without
    // which the arrangement only works for peers already known.
    assert_eq!(
        command_from(&msg(vec![
            ("command", "add".into()),
            ("public_key", key.as_str().into()),
            (
                "allowed_ips",
                rmpv::Value::Array(vec!["10.9.0.2/32".into()])
            ),
            ("endpoint", "203.0.113.7:51820".into()),
        ]))
        .unwrap(),
        Command::Add {
            public_key: [5u8; KEY_LEN],
            allowed_ips: vec!["10.9.0.2/32".parse().unwrap()],
            endpoint: Some("203.0.113.7:51820".parse().unwrap()),
            keepalive: None,
        }
    );
    assert_eq!(
        command_from(&msg(vec![
            ("command", "remove".into()),
            ("public_key", key.as_str().into()),
        ]))
        .unwrap(),
        Command::Remove {
            public_key: [5u8; KEY_LEN]
        }
    );
    assert_eq!(
        command_from(&msg(vec![
            ("command", "keepalive".into()),
            ("public_key", key.as_str().into()),
            ("keepalive", rmpv::Value::from(25u64)),
        ]))
        .unwrap(),
        Command::Keepalive {
            public_key: [5u8; KEY_LEN],
            seconds: Some(25)
        }
    );

    // And what is refused, each naming itself rather than being dropped.
    let err = command_from(&msg(vec![("command", "reboot".into())])).unwrap_err();
    assert!(err.contains("unknown command 'reboot'"), "{err}");
    assert!(err.contains("endpoint"), "{err}");
    let err = command_from(&msg(vec![("command", "endpoint".into())])).unwrap_err();
    assert!(err.contains("no `public_key`"), "{err}");
    let err = command_from(&msg(vec![
        ("command", "endpoint".into()),
        ("public_key", key.as_str().into()),
        ("endpoint", "nowhere".into()),
    ]))
    .unwrap_err();
    assert!(err.contains("host:port"), "{err}");
    let err = command_from(&msg(vec![
        ("command", "add".into()),
        ("public_key", key.as_str().into()),
    ]))
    .unwrap_err();
    assert!(err.contains("allowed_ips"), "{err}");
}

/// **The footgun that was there and is not now.** An `endpoint` command
/// with the field absent used to mean "clear it", on the reasoning that a
/// Lua table cannot hold a nil value. That made one mistyped field name --
/// `endpiont`, `Endpoint` -- tear down a working tunnel in silence, which
/// is a far worse failure than the one it avoided. Clearing is said out
/// loud now, and an absent field is an error that explains itself.
#[test]
fn clearing_an_endpoint_must_be_said_out_loud() {
    let key = B64.encode([5u8; KEY_LEN]);

    let typo = command_from(&msg(vec![
        ("command", "endpoint".into()),
        ("public_key", key.as_str().into()),
        ("endpiont", "203.0.113.7:51820".into()),
    ]))
    .unwrap_err();
    assert!(typo.contains("clear = true"), "{typo}");
    assert!(typo.contains("no `endpoint`"), "{typo}");

    assert_eq!(
        command_from(&msg(vec![
            ("command", "endpoint".into()),
            ("public_key", key.as_str().into()),
            ("clear", rmpv::Value::Boolean(true)),
        ]))
        .unwrap(),
        Command::ClearEndpoint {
            public_key: [5u8; KEY_LEN]
        }
    );
}

/// A key pair `drt wg keygen` prints is one the config accepts, and two of
/// them differ. The point of the verb is that setting up a peer never
/// needs `wireguard-tools` installed.
#[test]
fn keygen_prints_a_pair_the_config_accepts() {
    let (private, public) = drt::wireguard::keygen();
    assert_ne!(private, public);
    let (other, _) = drt::wireguard::keygen();
    assert_ne!(private, other, "two keygens produced the same private key");

    let mut config = scope(Some(&private));
    assert_eq!(public_key(&private_key(&config).unwrap()), public);
    config.peers.push(WireguardPeer {
        public_key: public.clone(),
        allowed_ips: vec!["10.9.0.2/32".into()],
        endpoint: None,
        keepalive: None,
        preshared_key_env: None,
    });
    assert!(drt::wireguard::validate(&config).is_ok());
}

/// What a config cannot be, judged before anything binds.
#[test]
fn a_config_that_cannot_work_is_refused_before_anything_binds() {
    let good = B64.encode([1u8; KEY_LEN]);

    // Zero is not an ephemeral port here, it is a port that could never
    // be reported honestly (gotatun answers with the CONFIGURED port) nor
    // punched to (a mapping belongs to a port).
    let mut zero = scope(Some(&good));
    zero.listen_port = 0;
    let err = drt::wireguard::validate(&zero).unwrap_err();
    assert!(err.contains("listen_port"), "{err}");
    assert!(err.contains("51820"), "{err}");

    // The commonest transcription slip: a route's network address where a
    // host address belongs. It yields an interface that answers to
    // nothing, so it is an error and not a warning.
    let mut network = scope(Some(&good));
    network.address = Some("10.9.0.0/24".into());
    let err = drt::wireguard::validate(&network).unwrap_err();
    assert!(err.contains("network address"), "{err}");
    network.address = Some("10.9.0.5/32".into());
    assert!(drt::wireguard::validate(&network).is_ok());

    // One STUN server can report an address; only two can say whether it
    // CHANGED, which is what decides whether a punch can work at all.
    let mut one = scope(Some(&good));
    one.stun = vec!["stun1.example:3478".into()];
    let err = drt::wireguard::validate(&one).unwrap_err();
    assert!(err.contains("two servers"), "{err}");
    one.stun.push("stun2.example:3478".into());
    assert!(drt::wireguard::validate(&one).is_ok());

    // A peer with no allowed IPs can neither be routed to nor accepted
    // from, so it is a peer that does nothing.
    let mut silent = scope(Some(&good));
    silent.peers.push(WireguardPeer {
        public_key: B64.encode([2u8; KEY_LEN]),
        allowed_ips: Vec::new(),
        endpoint: None,
        keepalive: None,
        preshared_key_env: None,
    });
    let err = drt::wireguard::validate(&silent).unwrap_err();
    assert!(err.contains("allowed_ips"), "{err}");
}

/// An absent endpoint is nil on the wire and never the empty string: "not
/// known" and "known to be nothing" are the same fact, and a supervisor
/// testing `if msg.endpoint then` should be right either way.
#[test]
fn a_report_says_nil_for_an_endpoint_it_does_not_have() {
    let value = drt::wireguard::report_value(&drt::wireguard::Report::Peers(vec![
        drt::wireguard::PeerReport {
            public_key: "abc".into(),
            endpoint: None,
            last_handshake_ms: None,
            rx_bytes: 0,
            tx_bytes: 0,
        },
    ]));
    let peers = field(&value, "peers");
    let peer = &peers.as_array().unwrap()[0];
    assert_eq!(field(&value, "event").as_str(), Some("wireguard"));
    assert_eq!(*field(peer, "endpoint"), rmpv::Value::Nil);
    assert_eq!(*field(peer, "last_handshake_ms"), rmpv::Value::Nil);

    let roam = drt::wireguard::report_value(&drt::wireguard::Report::Roamed {
        public_key: "abc".into(),
        endpoint: Some("198.51.100.4:51820".parse().unwrap()),
        previous: None,
    });
    assert_eq!(field(&roam, "event").as_str(), Some("wireguard_endpoint"));
    assert_eq!(
        field(&roam, "endpoint").as_str(),
        Some("198.51.100.4:51820")
    );
    assert_eq!(*field(&roam, "previous"), rmpv::Value::Nil);

    // A symmetric mapping has no address worth publishing, and says so
    // with nil rather than an address a peer could not use.
    let symmetric =
        drt::wireguard::report_value(&drt::wireguard::Report::Mapping(drt::wireguard::Mapping {
            kind: "symmetric",
            punchable: false,
            address: None,
            why: "a fresh mapping per destination".into(),
        }));
    assert_eq!(
        field(&symmetric, "event").as_str(),
        Some("wireguard_mapping")
    );
    assert_eq!(*field(&symmetric, "address"), rmpv::Value::Nil);
    assert_eq!(*field(&symmetric, "punchable"), rmpv::Value::Boolean(false));

    // And a refusal reaches the program that caused it, not just a log.
    let refused = drt::wireguard::report_value(&drt::wireguard::Report::Refused {
        command: "endpoint".into(),
        reason: "no peer abc".into(),
    });
    assert_eq!(field(&refused, "event").as_str(), Some("wireguard_error"));
    assert_eq!(field(&refused, "command").as_str(), Some("endpoint"));
    assert_eq!(field(&refused, "reason").as_str(), Some("no peer abc"));
}

/// The block loads from a `.host.lua` the way every other block does, with
/// `wg-quick`'s field names.
#[test]
fn the_wireguard_block_loads_with_wg_quicks_field_names() {
    let dir = tempfile::tempdir().unwrap();
    let key = B64.encode([2u8; KEY_LEN]);
    std::fs::write(
        dir.path().join("wg.host.lua"),
        format!(
            r#"return {{
  supervisor = "sup.lua",
  wireguard = {{
    listen_port = 51820,
    interface = "drt0",
    private_key_env = "WG_KEY",
    address = "10.9.0.1/24",
    mtu = 1420,
    queue = "wg_in",
    reply_queue = "wg_out",
    report_ms = 5000,
    peers = {{
      {{
        public_key = "{key}",
        allowed_ips = {{ "10.9.0.2/32", "fd00::2/128" }},
        endpoint = "203.0.113.7:51820",
        keepalive = 25,
      }},
    }},
  }},
}}"#
        ),
    )
    .unwrap();
    let config = drt::config::load(Some(&dir.path().join("wg.host.lua"))).unwrap();
    let wg = config.wireguard.expect("the wireguard block loaded");
    assert_eq!(wg.listen_port, 51820);
    assert_eq!(wg.interface, "drt0");
    assert_eq!(wg.private_key_env.as_deref(), Some("WG_KEY"));
    assert_eq!(wg.address.as_deref(), Some("10.9.0.1/24"));
    assert_eq!(wg.mtu, 1420);
    assert_eq!(wg.reply_queue, "wg_out");
    assert_eq!(wg.report_ms, 5000);
    assert_eq!(wg.peers.len(), 1);
    assert_eq!(wg.peers[0].allowed_ips, ["10.9.0.2/32", "fd00::2/128"]);
    assert_eq!(wg.peers[0].endpoint.as_deref(), Some("203.0.113.7:51820"));
    assert_eq!(wg.peers[0].keepalive, Some(25));

    // A typo is a typo, not a silent default -- the C loader's promise,
    // kept for this block like every other.
    std::fs::write(
        dir.path().join("typo.host.lua"),
        r#"return { supervisor = "s.lua", wireguard = { listen_port = 1, privatekey = "x" } }"#,
    )
    .unwrap();
    let err = drt::config::load(Some(&dir.path().join("typo.host.lua"))).unwrap_err();
    assert!(err.contains("privatekey"), "{err}");

    // A peer with no public key has no name at all.
    std::fs::write(
        dir.path().join("anon.host.lua"),
        r#"return { supervisor = "s.lua", wireguard = { peers = { { allowed_ips = { "10.0.0.1/32" } } } } }"#,
    )
    .unwrap();
    let err = drt::config::load(Some(&dir.path().join("anon.host.lua"))).unwrap_err();
    assert!(err.contains("public_key"), "{err}");
}

/// A peer the config never named, added at run time and then reachable.
///
/// This is the case a rendezvous actually produces: a deployment that
/// learns of a peer it has never heard of, with an address measured a
/// second ago. Without `add` the whole arrangement only works for peers
/// already written into the config — which is the case that never needed
/// a rendezvous.
///
/// It also covers the refusal path, and it is the test that found the
/// sharpest thing in this whole feature: **gotatun 0.9.2 does not route
/// for a device that was built with no peers**, even after peers are added
/// at run time. Build one with an empty peer list, add a peer, send to its
/// allowed IP, and nothing leaves; add any peer at build time and the same
/// runtime add works. `apply` forces the connection to rebuild when the
/// first peer arrives, and this test is what holds that fix in place --
/// remove it and this goes red while every other test stays green.
#[test]
fn a_peer_the_config_never_named_can_be_added_and_then_reached() {
    rt().block_on(async {
        let (secret_a, secret_b) = (
            StaticSecret::from([0x33u8; KEY_LEN]),
            StaticSecret::from([0x44u8; KEY_LEN]),
        );
        let (pub_a, pub_b) = (
            gotatun::x25519::PublicKey::from(&secret_a),
            gotatun::x25519::PublicKey::from(&secret_b),
        );
        let (port_a, port_b) = (free_port(), free_port());
        let (ip_a, ip_b) = (Ipv4Addr::new(10, 9, 0, 1), Ipv4Addr::new(10, 9, 0, 2));

        // B knows A. A knows NOBODY: it is started with a peer list that
        // does not mention B at all, which is what a config looks like
        // before a rendezvous has run.
        let peer_a = Peer::new(pub_a)
            .with_endpoint(SocketAddr::from(([127, 0, 0, 1], port_a)))
            .with_allowed_ip(ipnetwork::IpNetwork::from(std::net::IpAddr::V4(ip_a)));
        let (tun_a, tx_a, rx_a) = channel_tun();
        let (mut tun_b, tx_b, rx_b) = channel_tun();
        let a = DeviceBuilder::new()
            .with_default_udp()
            .with_ip_pair(tx_a, rx_a)
            .with_listen_port(port_a)
            .with_private_key(secret_a)
            .build()
            .await
            .expect("a device with no peers still comes up");
        let _b = device(secret_b, port_b, peer_a, tx_b, rx_b).await;

        let (reports, mut report_rx) = mpsc::unbounded_channel();
        let (commands, command_rx) = mpsc::unbounded_channel();
        // Five seconds, not fifty milliseconds, and that is the point: the
        // deadlines below are all far shorter, so nothing here can be
        // delivered BY the clock. What arrives, arrives because the
        // command pass sent it.
        let driver = tokio::spawn(drt::wireguard::drive(
            a,
            Duration::from_secs(5),
            reports,
            command_rx,
            None,
        ));

        // First, the refusal: a command for a peer that is not there comes
        // back to the program that sent it, so a supervisor waiting on a
        // handshake learns its key was wrong instead of concluding the
        // network is bad.
        commands
            .send(Command::Endpoint {
                public_key: *pub_b.as_bytes(),
                endpoint: SocketAddr::from(([127, 0, 0, 1], port_b)),
                keepalive: None,
            })
            .unwrap();
        let refusal = expect_report(&mut report_rx, Duration::from_secs(2)).await;
        match refusal {
            drt::wireguard::Report::Refused { command, reason } => {
                assert_eq!(command, "endpoint");
                assert!(reason.contains(&B64.encode(pub_b.as_bytes())), "{reason}");
                assert!(reason.contains("add"), "{reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        // Now the rendezvous result: who the peer is, what it owns, and
        // where it turned out to be, in one command.
        commands
            .send(Command::Add {
                public_key: *pub_b.as_bytes(),
                allowed_ips: vec![ipnetwork::IpNetwork::from(std::net::IpAddr::V4(ip_b))],
                endpoint: Some(SocketAddr::from(([127, 0, 0, 1], port_b))),
                keepalive: Some(25),
            })
            .unwrap();

        // The supervisor is told the add worked, on the same pass -- not
        // at the next snapshot, and not never. (It was "never" until this
        // test was written: see the comment in `drive`.)
        let seen = expect_report(&mut report_rx, Duration::from_secs(2)).await;
        match seen {
            drt::wireguard::Report::Peers(peers) => {
                let added = peers
                    .iter()
                    .find(|p| p.public_key == B64.encode(pub_b.as_bytes()))
                    .expect("the added peer is in the report");
                assert_eq!(
                    added.endpoint,
                    Some(SocketAddr::from(([127, 0, 0, 1], port_b)))
                );
            }
            other => panic!("expected the peer list, got {other:?}"),
        }

        let packet = ipv4_udp(ip_a, ip_b, b"a peer we had never heard of");
        let inject = tun_a.inject.clone();
        let sender = tokio::spawn(async move {
            for _ in 0..100 {
                if inject.send(packet.clone()).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        expect_payload(&mut tun_b, b"a peer we had never heard of").await;
        sender.abort();
        driver.abort();
    });
}

/// The failure this catches is a tunnel that comes up, reports a handshake,
/// and carries nothing: a peer whose `allowed_ips` lies outside the
/// interface address's own prefix, which is the only route the kernel
/// derives. Both numbers are in the config, so it is said at startup rather
/// than found with tcpdump.
#[test]
fn an_allowed_ip_no_route_will_reach_is_named_at_startup() {
    let good = B64.encode([1u8; KEY_LEN]);
    let peer = |cidr: &str| WireguardPeer {
        public_key: B64.encode([2u8; KEY_LEN]),
        allowed_ips: vec![cidr.into()],
        endpoint: None,
        keepalive: None,
        preshared_key_env: None,
    };

    // Inside the interface's own prefix: the on-link route reaches it, so
    // nothing is said.
    let mut fine = scope(Some(&good)); // address is 10.9.0.1/24
    fine.peers.push(peer("10.9.0.2/32"));
    assert!(drt::wireguard::unroutable(&fine).is_empty());

    // Outside it: configured perfectly, and unreachable.
    let mut hub = scope(Some(&good));
    hub.peers.push(peer("192.168.1.0/24"));
    let said = drt::wireguard::unroutable(&hub);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains("192.168.1.0/24"), "{}", said[0]);
    assert!(said[0].contains("carry nothing"), "{}", said[0]);
    // The remedy, spelled out with this interface's name in it.
    assert!(
        said[0].contains("ip route add 192.168.1.0/24 dev drt0"),
        "{}",
        said[0]
    );

    // A different family is never covered by an IPv4 prefix, however it
    // looks: the `contains` alone would panic or mislead without the check.
    let mut v6 = scope(Some(&good));
    v6.peers.push(peer("fd00::/64"));
    assert_eq!(drt::wireguard::unroutable(&v6).len(), 1);

    // No address at all: nothing is routable, and the reason differs.
    let mut homeless = scope(Some(&good));
    homeless.address = None;
    homeless.peers.push(peer("10.9.0.2/32"));
    let said = drt::wireguard::unroutable(&homeless);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains("no `address`"), "{}", said[0]);

    // A default route is the send-everything-through-the-tunnel case. It
    // needs a route too, but saying so for every VPN-shaped config would be
    // noise where the intent is unmistakable.
    let mut everything = scope(Some(&good));
    everything.peers.push(peer("0.0.0.0/0"));
    assert!(drt::wireguard::unroutable(&everything).is_empty());
}

/// **The fallback, end to end.** When `stun` says a NAT cannot be punched,
/// the peer that cannot be reached takes a TURN allocation and publishes
/// *that* address instead of the measured one. Its WireGuard traffic then
/// goes through the relay, and the far side is none the wiser: it has an
/// endpoint, and packets arrive from it.
///
/// Everything here is DRT's: the TURN server is `drt turn`'s, the
/// credential is `crypto/turn_credential`'s scheme, and the two devices are
/// the same ones every other test in this file uses. Loopback, unprivileged,
/// no NAT — so what this proves is the plumbing, not that it beats a real
/// symmetric NAT. Same limit as the punch itself (`doc/WireGuard.md` §2).
#[test]
fn wireguard_traffic_can_fall_back_through_a_turn_allocation() {
    rt().block_on(async {
        const SECRET: &str = "the-turn-secret-for-this-test-01";

        // DRT's own TURN relay, on loopback.
        let turn = drt::turn::bind(
            &drt_config::TurnConfig {
                bind: "127.0.0.1:0".into(),
                relay_address: "127.0.0.1".into(),
                relay_bind: "127.0.0.1".into(),
                realm: "drt".into(),
                key: Some(SECRET.into()),
                key_file: None,
                key_env: None,
                max_allocations: 8,
                queue: "turn_in".into(),
                report_ms: 10_000,
            },
            None,
        )
        .await
        .expect("the turn server bound");
        let turn_addr = turn.local_addr();

        // The credential a program would mint with crypto/turn_credential:
        // coturn's use-auth-secret scheme, which is what the server verifies.
        let (username, password) =
            ego_transport::turn::ephemeral_credentials_for(SECRET, Duration::from_secs(300), "fp")
                .expect("a credential");

        let (secret_a, secret_b) = (
            StaticSecret::from([0x55u8; KEY_LEN]),
            StaticSecret::from([0x66u8; KEY_LEN]),
        );
        let (pub_a, pub_b) = (
            gotatun::x25519::PublicKey::from(&secret_a),
            gotatun::x25519::PublicKey::from(&secret_b),
        );
        let (port_a, port_b) = (free_port(), free_port());
        let (ip_a, ip_b) = (Ipv4Addr::new(10, 9, 0, 1), Ipv4Addr::new(10, 9, 0, 2));

        // A is the peer that cannot be punched to: it will relay.
        let (transport_a, allocation) = drt::wireguard::Transport::new(true);
        let (tun_a, tx_a, rx_a) = channel_tun();
        let (mut tun_b, tx_b, rx_b) = channel_tun();
        let a = DeviceBuilder::new()
            .with_udp(transport_a)
            .with_ip_pair(tx_a, rx_a)
            .with_listen_port(port_a)
            .with_private_key(secret_a)
            .with_peer(
                Peer::new(pub_b)
                    .with_endpoint(SocketAddr::from(([127, 0, 0, 1], port_b)))
                    .with_allowed_ip(ipnetwork::IpNetwork::from(std::net::IpAddr::V4(ip_b))),
            )
            .build()
            .await
            .expect("the relaying device came up");

        // B is built HERE, before the allocation is taken, so it owns
        // port_b first: the TURN server's relay socket takes an ephemeral
        // port, and `free_port` released port_b before either bound it. It
        // starts with no endpoint and is told where A is once the
        // allocation exists, which is the rendezvous's shape anyway.
        let b = device(
            secret_b,
            port_b,
            Peer::new(pub_a)
                .with_allowed_ip(ipnetwork::IpNetwork::from(std::net::IpAddr::V4(ip_a))),
            tx_b,
            rx_b,
        )
        .await;

        let (reports, mut report_rx) = mpsc::unbounded_channel();
        let (commands, command_rx) = mpsc::unbounded_channel();
        let driver = tokio::spawn(drt::wireguard::drive(
            a,
            Duration::from_millis(50),
            reports,
            command_rx,
            allocation,
        ));

        // The program read `punchable: false`, minted a credential, and
        // hands it over. What comes back is the address to publish.
        commands
            .send(drt::wireguard::Command::Relay {
                server: turn_addr,
                username: username.clone(),
                password: password.clone(),
                realm: "drt".into(),
            })
            .unwrap();
        let relayed = loop {
            let report = tokio::time::timeout(Duration::from_secs(10), report_rx.recv())
                .await
                .expect("the relay command was answered")
                .expect("the report channel stayed open");
            match report {
                drt::wireguard::Report::Relaying { address, server } => {
                    assert_eq!(server, Some(turn_addr));
                    break address.expect("an allocation has an address to publish");
                }
                drt::wireguard::Report::Refused { command, reason } => {
                    panic!("{command} refused: {reason}")
                }
                _ => continue,
            }
        };
        assert_ne!(
            relayed.port(),
            port_a,
            "the relayed address must not be the device's own port"
        );

        assert_ne!(
            relayed.port(),
            port_a,
            "the relayed address must be the allocation's, not the device's own port"
        );

        // B is told where A turned out to be: its RELAYED address, which is
        // the only address that reaches A. In a deployment this is the
        // rendezvous answering; here it is one call.
        b.write(async |d| {
            d.modify_peer(&pub_a, |p| p.set_endpoint(Some(relayed)))
                .await
        })
        .await
        .expect("B took the endpoint");

        // A sends. Its packets leave through the allocation, so they reach B
        // from the relayed address B was told about, and B accepts them.
        let packet = ipv4_udp(ip_a, ip_b, b"through the turn relay");
        let inject = tun_a.inject.clone();
        let sender = tokio::spawn(async move {
            for _ in 0..100 {
                if inject.send(packet.clone()).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        expect_payload(&mut tun_b, b"through the turn relay").await;

        sender.abort();

        // **Losing the relay must not lose the tunnel.** gotatun's buffered
        // receive loop ends its task for good on any error from
        // `recv_many_from`, so a transport that propagated a failed
        // allocation would take the DIRECT path down with it -- a dead
        // tunnel over a socket that was fine all along. The transport drops
        // the allocation instead and keeps reading; this is that, driven by
        // the command rather than by a failure, because both go through the
        // same `send_replace(None)` and the same wake.
        commands.send(drt::wireguard::Command::ClearRelay).unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), report_rx.recv())
                .await
                .expect("the clear was answered")
                .expect("the report channel stayed open")
            {
                drt::wireguard::Report::Relaying { address: None, .. } => break,
                _ => continue,
            }
        }

        // The device still works, on the socket it had underneath all along.
        b.write(async |d| {
            d.modify_peer(&pub_a, |p| {
                p.set_endpoint(Some(SocketAddr::from(([127, 0, 0, 1], port_a))))
            })
            .await
        })
        .await
        .expect("B took the direct endpoint");
        let direct = ipv4_udp(ip_a, ip_b, b"and directly once the relay is gone");
        let inject = tun_a.inject.clone();
        let sender = tokio::spawn(async move {
            for _ in 0..100 {
                if inject.send(direct.clone()).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        expect_payload(&mut tun_b, b"and directly once the relay is gone").await;

        sender.abort();
        driver.abort();
    });
}

// ---------------------------------------------------------------------------
// Issue #15: the reports a program cannot afford to miss
// ---------------------------------------------------------------------------

/// A report that is said once survives a queue that is not there yet.
///
/// This is the bug that made the whole block unusable for the thing it was
/// built for. `Mapping` is queued before the device binds, so it reaches
/// `report` on the bridge's very first pass -- which is before a root
/// program has run the line that declares its queue. The push is refused,
/// and under the old code the mapping was dropped, so a rendezvous whose
/// first act is to wait for `wireguard_mapping` waited forever.
///
/// The snapshot has the opposite policy on purpose, and this test holds
/// both halves: hold what is said once, drop what will be said again.
#[test]
fn a_report_said_once_outlives_a_queue_that_is_not_declared_yet() {
    let (reports, mut report_rx) = mpsc::unbounded_channel();
    let mut held = Vec::new();
    let mut refusals = Vec::new();

    // The bridge's first pass: the mapping, and a snapshot behind it,
    // into a program that has not declared `wg_in` yet.
    reports
        .send(drt::wireguard::Report::Refused {
            command: "measure".into(),
            reason: "no answer from either server".into(),
        })
        .unwrap();
    reports
        .send(drt::wireguard::Report::Peers(Vec::new()))
        .unwrap();

    let mut refused = |_: &str, _: &[u8]| false;
    drt::wireguard::push_reports(
        "wg_in",
        &mut held,
        &mut refusals,
        &mut report_rx,
        &mut refused,
    );
    assert_eq!(
        held.len(),
        1,
        "the one-shot report should be held and the snapshot dropped"
    );

    // The program declares its queue. The held report goes out on the very
    // next pass, without the device having said anything since.
    let mut taken: Vec<Vec<u8>> = Vec::new();
    let mut accept = |queue: &str, msg: &[u8]| {
        assert_eq!(queue, "wg_in");
        taken.push(msg.to_vec());
        true
    };
    drt::wireguard::push_reports(
        "wg_in",
        &mut held,
        &mut refusals,
        &mut report_rx,
        &mut accept,
    );
    assert!(held.is_empty(), "nothing should still be held");
    assert_eq!(taken.len(), 1, "exactly the held report, not the snapshot");

    let got = rmpv::decode::read_value(&mut taken[0].as_slice()).expect("a report decoded");
    assert_eq!(field(&got, "event").as_str(), Some("wireguard_error"));
    assert_eq!(field(&got, "command").as_str(), Some("measure"));
}

/// A queue nobody drains does not become this process's problem.
///
/// Holding is for the program that has not declared its queue yet, which
/// takes a pass or two. A queue that stays full is the deployment's own
/// sizing to see, and holding for it without a bound would move the
/// overflow out of the queue and into the device.
#[test]
fn holding_a_report_forever_is_bounded() {
    let (reports, mut report_rx) = mpsc::unbounded_channel();
    let mut held = Vec::new();
    let mut refusals = Vec::new();
    let mut refused = |_: &str, _: &[u8]| false;

    for i in 0..(drt::wireguard::HELD_MAX * 2) {
        reports
            .send(drt::wireguard::Report::Refused {
                command: "relay".into(),
                reason: format!("refusal {i}"),
            })
            .unwrap();
        drt::wireguard::push_reports(
            "wg_in",
            &mut held,
            &mut refusals,
            &mut report_rx,
            &mut refused,
        );
    }
    assert_eq!(held.len(), drt::wireguard::HELD_MAX);

    // The OLDEST are the ones kept: the report a program is blocked on is
    // the one that arrived first.
    let first = rmpv::decode::read_value(&mut held[0].as_slice()).expect("a report decoded");
    assert_eq!(field(&first, "reason").as_str(), Some("refusal 0"));
}

/// `report_ms` is a clock, so it ticks for a device with no peers at all.
///
/// The no-peer device is not a corner case: it is exactly what a config
/// looks like before a rendezvous has run, and a program that waits on the
/// queue for its interval needs the interval to arrive. The snapshot of an
/// empty peer set never differs from the last one, so while the timer was
/// gated on a change this device reported nothing, ever.
#[test]
fn a_device_with_no_peers_still_reports_on_its_interval() {
    rt().block_on(async {
        let (tun, tx, rx) = channel_tun();
        drop(tun);
        let device = DeviceBuilder::new()
            .with_default_udp()
            .with_ip_pair(tx, rx)
            .with_listen_port(free_port())
            .with_private_key(StaticSecret::from([0x55u8; KEY_LEN]))
            .build()
            .await
            .expect("a device with no peers comes up");

        let (reports, mut report_rx) = mpsc::unbounded_channel();
        let (_commands, command_rx) = mpsc::unbounded_channel();
        let driver = tokio::spawn(drt::wireguard::drive(
            device,
            Duration::from_millis(50),
            reports,
            command_rx,
            None,
        ));

        let report = tokio::time::timeout(Duration::from_secs(5), report_rx.recv())
            .await
            .expect("the interval arrived without a peer to report")
            .expect("the report channel stayed open");
        match report {
            drt::wireguard::Report::Peers(peers) => assert!(peers.is_empty()),
            other => panic!("expected an empty snapshot, got {other:?}"),
        }
        driver.abort();
    });
}

/// `peers = {}` is a config, not a mistake.
///
/// Lua has one table type, so `{}` is both the empty list and the empty
/// map; the loader read lists with `as_array` and refused the empty case.
/// The no-peer block is the one a rendezvous writes.
#[test]
fn a_wireguard_block_may_name_no_peers_at_all() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("rendezvous.host.lua"),
        r#"return {
  supervisor = "sup.lua",
  caps = {},
  wireguard = {
    listen_port = 51820,
    interface = "drt-fp",
    private_key_env = "WG_KEY",
    address = "10.9.0.1/24",
    queue = "wg_in",
    reply_queue = "wg_out",
    stun = {},
    peers = {},
  },
}"#,
    )
    .unwrap();
    let config = drt::config::load(Some(&dir.path().join("rendezvous.host.lua")))
        .expect("a block that learns its peers later still loads");
    let wg = config.wireguard.expect("the wireguard block loaded");
    assert!(wg.peers.is_empty());
    assert!(wg.stun.is_empty());
    assert!(config.root.caps.is_empty());

    // An empty allowed_ips is still refused: a peer that owns no addresses
    // is a peer nothing will ever be routed to, which `validate` catches.
    std::fs::write(
        dir.path().join("empty-ips.host.lua"),
        r#"return { supervisor = "s.lua", wireguard = { peers = { { public_key = "x", allowed_ips = {} } } } }"#,
    )
    .unwrap();
    let config = drt::config::load(Some(&dir.path().join("empty-ips.host.lua")))
        .expect("the loader takes it; validate is what refuses it");
    assert!(config.wireguard.unwrap().peers[0].allowed_ips.is_empty());
}
