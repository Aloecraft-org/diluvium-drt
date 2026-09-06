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
        commands
            .send(Command::Endpoint {
                public_key: *pub_b.as_bytes(),
                endpoint: Some(SocketAddr::from(([127, 0, 0, 1], port_b))),
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
    let msg = |fields: Vec<(&str, rmpv::Value)>| {
        rmpv::Value::Map(
            fields
                .into_iter()
                .map(|(k, v)| (rmpv::Value::from(k), v))
                .collect(),
        )
    };

    let set = command_from(&msg(vec![
        ("command", "endpoint".into()),
        ("public_key", key.as_str().into()),
        ("endpoint", "203.0.113.7:51820".into()),
    ]))
    .unwrap();
    assert_eq!(
        set,
        Command::Endpoint {
            public_key: [5u8; KEY_LEN],
            endpoint: Some("203.0.113.7:51820".parse().unwrap()),
        }
    );

    // A Lua table cannot hold a nil value, so leaving the key out is the
    // only way a program can say "clear it" — and it means that.
    let cleared = command_from(&msg(vec![
        ("command", "endpoint".into()),
        ("public_key", key.as_str().into()),
    ]))
    .unwrap();
    assert_eq!(
        cleared,
        Command::Endpoint {
            public_key: [5u8; KEY_LEN],
            endpoint: None
        }
    );

    let keepalive = command_from(&msg(vec![
        ("command", "keepalive".into()),
        ("public_key", key.as_str().into()),
        ("seconds", rmpv::Value::from(25u64)),
    ]))
    .unwrap();
    assert_eq!(
        keepalive,
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
