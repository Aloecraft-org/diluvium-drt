//! The userspace IP side of the WireGuard device: a TCP/IP stack inside
//! this process, on the far end of the channel pair, reached through the
//! `forward` and `expose` lists of a `mode = "userspace"` block.
//!
//! ## Why this exists
//!
//! Everything in the punch path is already unprivileged -- STUN, the
//! rendezvous, the punch, the WireGuard protocol -- and `CAP_NET_ADMIN`
//! buys exactly one thing: an adapter in the kernel's stack. A tool that
//! demonstrably moves bytes *before* it asks for a privilege is a
//! different trust proposition from one that asks first, and on macOS and
//! Windows "asks first" means most users say no. So the IP side that
//! `drt start` hands a kernel interface, and the tests hand two channels,
//! is handed a stack here: smoltcp, poll-driven, owning one `Interface`
//! with the block's `address` and a default route, and a socket per
//! connection. "ssh through the hole" becomes a port on localhost.
//!
//! The device does not know. `drive`, `remap`, `apply`, the reports and
//! the relay fallback are generic over exactly the seam this uses, so a
//! rendezvous program cannot tell the modes apart from the queue.
//!
//! ## surface block
//!
//! - Entry point: [`Stack::start`] -- the interface, the driver task, a
//!   listener per `forward`, a listening socket per `expose`, and one
//!   `wireguard_forward` report for each. Everything else here is what a
//!   connection does once it exists.
//! - Configurable: [`BUFFER`], each socket's send and receive buffer, and
//!   what one read moves; `wireguard::CONNECT_TIMEOUT`, how long a dial
//!   inside the tunnel may go unanswered; `wireguard::PACKET_QUEUE`, how
//!   many packets may wait between the device and the stack.
//! - Fan-out: the two kinds of leg. A **forward** is a tokio listener
//!   whose accepted connection opens a smoltcp socket to `to`
//!   ([`forward_leg`]); an **expose** is a smoltcp listening socket whose
//!   accepted connection dials `to` locally ([`expose_leg`]). Both end in
//!   the one [`pump`].
//!
//! ## The six rules of the pump, stated so they are not re-learned
//!
//! 1. **Claim first, splice second.** A dial that is refused or times
//!    out closes the accepted socket at once, so a client sees a refused
//!    connection and never a half-open one.
//! 2. **Half-close is real.** A local EOF becomes a FIN on the wire and
//!    reading continues; a FIN from the far side shuts the local write
//!    half and writing continues. `printf … | ssh` gets its answer back.
//! 3. **Bytes move only when the stack can take them.** `can_send` and
//!    `can_recv` gate each direction, and nothing is queued past
//!    [`BUFFER`]. A slow peer is back-pressure, not memory.
//! 4. **A dead tunnel is a timeout, not a hang**, at `CONNECT_TIMEOUT`,
//!    reported as a `wireguard_error` naming the entry -- so a program
//!    can tell "no peer" from "nothing listens there".
//! 5. **An expose dials lazily**, on the SYN and never at startup: an
//!    sshd dialed at startup and held idle dies at `LoginGraceTime`,
//!    confusingly, later.
//! 6. **A smoltcp listening socket is one-shot** -- the socket that
//!    accepts *becomes* the connection -- so an expose re-arms a fresh
//!    listener the moment one leaves LISTEN, or the second caller is
//!    refused.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Poll;
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpListenEndpoint};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use drt_config::WireguardConfig;

use crate::wireguard::{Report, StackEnd, CONNECT_TIMEOUT};

/// Each socket's send buffer and receive buffer, and the most one read
/// moves: the tunnel's `CHUNK`. Nothing is queued past it in either
/// direction, which is what makes a slow peer back-pressure and not
/// memory.
pub const BUFFER: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// depth: the stack's device, which is the channel pair seen from smoltcp
// ---------------------------------------------------------------------------

/// The channel pair as a smoltcp `phy::Device`. Raw IP in both directions,
/// which is what gotatun's IP side speaks and what `medium-ip` expects;
/// nothing is framed.
struct Channels {
    /// Packets the device decrypted, waiting for the stack. The driver
    /// moves them here from the observe channel under the lock, because
    /// `receive` is synchronous and the channel is not.
    incoming: VecDeque<Vec<u8>>,
    /// Packets the stack sends, for the device to encrypt.
    inject: mpsc::Sender<Vec<u8>>,
    mtu: usize,
}

struct RxToken(Vec<u8>);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

struct TxToken<'a>(&'a mpsc::Sender<Vec<u8>>);

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut packet = vec![0u8; len];
        let result = f(&mut packet);
        // Full means the device is not draining; a dropped packet is what
        // a tun queue does with the same condition, and TCP retransmits.
        let _ = self.0.try_send(packet);
        result
    }
}

impl Device for Channels {
    type RxToken<'a>
        = RxToken
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, _now: Instant) -> Option<(RxToken, TxToken<'_>)> {
        let packet = self.incoming.pop_front()?;
        Some((RxToken(packet), TxToken(&self.inject)))
    }

    fn transmit(&mut self, _now: Instant) -> Option<TxToken<'_>> {
        // No token while the queue is full: the packet then stays in its
        // socket and goes on the next poll, which beats dropping it.
        if self.inject.capacity() == 0 {
            return None;
        }
        Some(TxToken(&self.inject))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps
    }
}

// ---------------------------------------------------------------------------
// The stack: one interface, one socket set, one lock, one driver
// ---------------------------------------------------------------------------

/// What the driver and the legs share. smoltcp is single-threaded by
/// design, so everything it owns sits behind one lock that is never held
/// across an `await`: the driver takes it to poll, a leg takes it to move
/// one buffer's worth of bytes, and both let go.
struct Shared {
    iface: Interface,
    device: Channels,
    sockets: SocketSet<'static>,
    /// The ephemeral port the next forward connects from.
    next_port: u16,
    /// Sockets no leg will touch again, for the driver to remove once
    /// they have nothing left to say. A socket is never removed by a leg:
    /// `abort` only schedules the RST, and the poll that emits it has to
    /// come first, or the far side is left with a half-open connection
    /// exactly where it was promised a closed one.
    released: Vec<SocketHandle>,
}

/// Which waker a wait registers. A socket holds one waker per direction
/// and a second task registering the same one replaces the first, so the
/// two halves of a pump must each register only their own -- or the far
/// side's data would wake the writer and the reader would wait forever.
#[derive(Clone, Copy)]
enum On {
    Recv,
    Send,
    /// Both, for a wait on state alone, when no pump is running yet.
    Either,
}

/// The stack, as the legs see it.
pub struct Stack {
    shared: Mutex<Shared>,
    /// Wakes the driver: a leg changed a socket and wants it polled.
    poke: mpsc::UnboundedSender<()>,
    /// The stack's own address, which every forward connects from and
    /// every expose listens on.
    own: IpAddr,
    epoch: std::time::Instant,
    reports: mpsc::UnboundedSender<Report>,
    /// Held so the MTU watcher the device holds never reads a dropped
    /// sender.
    _mtu: tokio::sync::watch::Sender<u16>,
}

impl Stack {
    /// Bring the stack up on `end`, bind every `forward`, arm every
    /// `expose`, and say what was bound. Everything that can be refused
    /// is refused before any task exists: a held `bind` address fails
    /// here by name, so `drt start` never comes up with one forward
    /// silently missing.
    ///
    /// Spawns onto the current runtime, which is the bridge's, so the
    /// driver ends when that runtime does -- beside `drive`, on the same
    /// leaked-not-dropped thread.
    pub async fn start(
        config: &WireguardConfig,
        end: StackEnd,
        reports: mpsc::UnboundedSender<Report>,
    ) -> Result<Arc<Stack>, String> {
        // `validate` refused an absent or malformed address already.
        let cidr: ipnetwork::IpNetwork = config
            .address
            .as_deref()
            .ok_or("wireguard: mode = \"userspace\" needs an `address`")?
            .parse()
            .map_err(|e| format!("wireguard.address: {e}"))?;
        let own = cidr.ip();
        let epoch = std::time::Instant::now();

        let mut device = Channels {
            incoming: VecDeque::new(),
            inject: end.inject,
            mtu: usize::from(config.mtu),
        };
        let mut iface_config = Config::new(HardwareAddress::Ip);
        // Sequence numbers and ports start from this; it needs to differ
        // per boot, not to be secret.
        iface_config.random_seed = seed();
        let mut iface = Interface::new(iface_config, &mut device, Instant::from_micros(0));
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::from(own), cidr.prefix()))
                .expect("one address fits in an empty list");
        });
        // A default route, so a peer whose `allowed_ips` sits outside the
        // prefix still routes: there is nobody here to run the `ip route
        // add` that kernel mode tells the operator to run. The gateway is
        // nominal -- an IP medium has no neighbours to resolve -- so it is
        // this stack's own address.
        match own {
            IpAddr::V4(v4) => {
                let _ = iface.routes_mut().add_default_ipv4_route(v4);
            }
            IpAddr::V6(v6) => {
                let _ = iface.routes_mut().add_default_ipv6_route(v6);
            }
        }

        // Bind before anything runs, so a held port is a refusal by name.
        let mut forwards = Vec::new();
        for (i, entry) in config.forward.iter().enumerate() {
            let listener = tokio::net::TcpListener::bind(&entry.bind)
                .await
                .map_err(|e| {
                    format!(
                        "wireguard.forward[{i}].bind: cannot bind {}: {e}",
                        entry.bind
                    )
                })?;
            let to: SocketAddr = entry
                .to
                .parse()
                .map_err(|_| format!("wireguard.forward[{i}].to: not an ip:port"))?;
            forwards.push((listener, to));
        }
        let mut exposes = Vec::new();
        for (i, entry) in config.expose.iter().enumerate() {
            let tunnel: SocketAddr = entry
                .tunnel
                .parse()
                .map_err(|_| format!("wireguard.expose[{i}].tunnel: not an ip:port"))?;
            exposes.push((tunnel, entry.to.clone()));
        }

        let (poke, poked) = mpsc::unbounded_channel();
        let stack = Arc::new(Stack {
            shared: Mutex::new(Shared {
                iface,
                device,
                sockets: SocketSet::new(Vec::new()),
                next_port: 49152 + (seed() % 16384) as u16,
                released: Vec::new(),
            }),
            poke,
            own,
            epoch,
            reports,
            _mtu: end.mtu,
        });
        tokio::spawn(drive(stack.clone(), end.observe, poked));

        for (listener, to) in forwards {
            let bind = listener.local_addr().map_err(|e| e.to_string())?;
            let _ = stack.reports.send(Report::Forward {
                kind: "forward",
                from: bind.to_string(),
                to: to.to_string(),
            });
            tokio::spawn(forward_loop(stack.clone(), listener, bind, to));
        }
        for (tunnel, to) in exposes {
            let _ = stack.reports.send(Report::Forward {
                kind: "expose",
                from: tunnel.to_string(),
                to: to.clone(),
            });
            tokio::spawn(expose_loop(stack.clone(), tunnel, to));
        }
        Ok(stack)
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        // A panic under the lock is a bug in a leg, not a reason for every
        // other connection to stop moving bytes.
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn poke(&self) {
        let _ = self.poke.send(());
    }

    fn now(&self) -> Instant {
        Instant::from_micros(self.epoch.elapsed().as_micros() as i64)
    }

    // depth: the waits, and the one primitive under them

    /// Wait until `ready` says so, registering the socket's waker(s)
    /// meanwhile. `ready` runs under the lock and must not block.
    async fn until<T>(
        &self,
        handle: SocketHandle,
        on: On,
        mut ready: impl FnMut(&mut tcp::Socket) -> Option<T>,
    ) -> T {
        std::future::poll_fn(|cx| {
            let mut shared = self.lock();
            let socket = shared.sockets.get_mut::<tcp::Socket>(handle);
            if let Some(value) = ready(socket) {
                return Poll::Ready(value);
            }
            match on {
                On::Recv => socket.register_recv_waker(cx.waker()),
                On::Send => socket.register_send_waker(cx.waker()),
                On::Either => {
                    socket.register_recv_waker(cx.waker());
                    socket.register_send_waker(cx.waker());
                }
            }
            Poll::Pending
        })
        .await
    }

    /// Open a socket to `to` inside the tunnel, from an ephemeral port on
    /// this stack's own address. Returns once the SYN is queued; whether
    /// it is answered is [`Stack::established`]'s to say.
    fn connect(&self, to: SocketAddr) -> Result<SocketHandle, String> {
        let mut shared = self.lock();
        let port = shared.next_port;
        shared.next_port = if port == u16::MAX { 49152 } else { port + 1 };
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; BUFFER]),
            tcp::SocketBuffer::new(vec![0u8; BUFFER]),
        );
        let Shared { iface, sockets, .. } = &mut *shared;
        socket
            .connect(
                iface.context(),
                to,
                IpListenEndpoint {
                    addr: Some(IpAddress::from(self.own)),
                    port,
                },
            )
            .map_err(|e| format!("cannot open a connection to {to}: {e}"))?;
        let handle = sockets.add(socket);
        drop(shared);
        self.poke();
        Ok(handle)
    }

    /// A listening socket on `tunnel`, which becomes the connection when
    /// a SYN arrives (rule 6).
    fn listen(&self, tunnel: SocketAddr) -> Result<SocketHandle, String> {
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; BUFFER]),
            tcp::SocketBuffer::new(vec![0u8; BUFFER]),
        );
        socket
            .listen(tunnel)
            .map_err(|e| format!("cannot listen on {tunnel} inside the tunnel: {e}"))?;
        Ok(self.lock().sockets.add(socket))
    }

    /// Wait for the handshake. `Err` is the one reason it will not come:
    /// the socket closed (a RST answered the SYN), or, for a listener, it
    /// fell back to LISTEN after a RST in SYN-RECEIVED and is nobody's
    /// connection any more.
    async fn established(&self, handle: SocketHandle) -> Result<(), &'static str> {
        self.until(handle, On::Either, |socket| match socket.state() {
            tcp::State::Established => Some(Ok(())),
            tcp::State::SynSent | tcp::State::SynReceived => None,
            tcp::State::Listen => Some(Err("the handshake was reset before it completed")),
            _ => Some(Err("refused")),
        })
        .await
    }

    /// Queue bytes to the far side, as many as the buffer takes. `Err`
    /// once the socket can no longer send: closed by either side.
    async fn send(&self, handle: SocketHandle, data: &[u8]) -> Result<usize, ()> {
        let sent = self
            .until(handle, On::Send, |socket| {
                if !socket.may_send() {
                    return Some(Err(()));
                }
                if socket.can_send() {
                    return Some(socket.send_slice(data).map_err(|_| ()));
                }
                None
            })
            .await?;
        self.poke();
        Ok(sent)
    }

    /// Take what the far side sent. `Ok(0)` is its FIN (or RST) with
    /// nothing left behind it, which is the reader's EOF.
    async fn recv(&self, handle: SocketHandle, buf: &mut [u8]) -> usize {
        let taken = self
            .until(handle, On::Recv, |socket| {
                if socket.can_recv() {
                    return Some(socket.recv_slice(buf).unwrap_or(0));
                }
                if !socket.may_recv() {
                    return Some(0);
                }
                None
            })
            .await;
        // Taking bytes opened the window; the ACK that says so goes on
        // the next poll.
        self.poke();
        taken
    }

    /// Our FIN: no more bytes from this side, still reading (rule 2).
    fn close(&self, handle: SocketHandle) {
        self.lock().sockets.get_mut::<tcp::Socket>(handle).close();
        self.poke();
    }

    /// A RST, for a connection that cannot be finished cleanly -- sent by
    /// the driver's next poll, which is why this does not also release.
    fn abort(&self, handle: SocketHandle) {
        self.lock().sockets.get_mut::<tcp::Socket>(handle).abort();
        self.poke();
    }

    /// Hand the socket to the driver to remove once it is done: a FIN
    /// still draining behind the last bytes, a RST not yet sent, a
    /// TIME-WAIT. The leg never touches the handle again.
    fn release(&self, handle: SocketHandle) {
        self.lock().released.push(handle);
        self.poke();
    }

    fn error(&self, kind: &str, from: &str, to: &str, reason: &str) {
        eprintln!("drt wg: {kind} {from} -> {to}: {reason}");
        let _ = self.reports.send(Report::Refused {
            command: kind.into(),
            reason: format!("{from} -> {to}: {reason}"),
        });
    }
}

/// A seed that differs per boot, which is all smoltcp asks of it.
fn seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (u64::from(std::process::id()) << 32)
}

// ---------------------------------------------------------------------------
// depth: the driver
// ---------------------------------------------------------------------------

/// The one task that polls smoltcp. It wakes on three things: a packet
/// from the device, a poke from a leg that changed a socket, and the
/// stack's own `poll_delay` -- retransmits, delayed ACKs, TIME-WAIT. It
/// ends when the device does, which closes the observe channel.
async fn drive(
    stack: Arc<Stack>,
    mut observe: mpsc::Receiver<Vec<u8>>,
    mut poked: mpsc::UnboundedReceiver<()>,
) {
    loop {
        let delay = {
            let mut shared = stack.lock();
            let now = stack.now();
            let Shared {
                iface,
                device,
                sockets,
                released,
                ..
            } = &mut *shared;
            iface.poll(now, device, sockets);
            // Released sockets go once they have nothing left to say: a
            // RST or a FIN still to send keeps one; TIME-WAIT, or closed
            // with the four-tuple forgotten, is done.
            released.retain(|handle| {
                let socket = sockets.get_mut::<tcp::Socket>(*handle);
                let done = match socket.state() {
                    tcp::State::TimeWait => true,
                    tcp::State::Closed => socket.remote_endpoint().is_none(),
                    _ => false,
                };
                if done {
                    sockets.remove(*handle);
                }
                !done
            });
            iface.poll_delay(now, sockets)
        };
        let sleep = tokio::time::sleep(
            delay
                .map(Duration::from)
                .unwrap_or_else(|| Duration::from_secs(60)),
        );
        tokio::select! {
            packet = observe.recv() => {
                let Some(packet) = packet else { return };
                let mut shared = stack.lock();
                shared.device.incoming.push_back(packet);
                while let Ok(more) = observe.try_recv() {
                    shared.device.incoming.push_back(more);
                }
            }
            poke = poked.recv() => {
                if poke.is_none() {
                    return;
                }
                while poked.try_recv().is_ok() {}
            }
            _ = sleep => {}
        }
    }
}

// ---------------------------------------------------------------------------
// depth: the legs, and the pump they share
// ---------------------------------------------------------------------------

/// A `forward`: accept on the local port, one leg per connection.
async fn forward_loop(
    stack: Arc<Stack>,
    listener: tokio::net::TcpListener,
    bind: SocketAddr,
    to: SocketAddr,
) {
    loop {
        let Ok((conn, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(forward_leg(stack.clone(), conn, bind, to));
    }
}

/// One accepted local connection, dialed into the tunnel (rule 1, rule 4).
/// `conn` is dropped -- closed, at once -- on every path that does not
/// reach the pump.
async fn forward_leg(
    stack: Arc<Stack>,
    conn: tokio::net::TcpStream,
    bind: SocketAddr,
    to: SocketAddr,
) {
    let (kind, from, target) = ("forward", bind.to_string(), to.to_string());
    let handle = match stack.connect(to) {
        Ok(handle) => handle,
        Err(e) => return stack.error(kind, &from, &target, &e),
    };
    match tokio::time::timeout(CONNECT_TIMEOUT, stack.established(handle)).await {
        Ok(Ok(())) => pump(&stack, handle, conn).await,
        Ok(Err(why)) => {
            stack.release(handle);
            stack.error(
                kind,
                &from,
                &target,
                &format!("{why} by {to}: a peer answered, and nothing listens there"),
            );
        }
        Err(_) => {
            stack.abort(handle);
            stack.release(handle);
            stack.error(
                kind,
                &from,
                &target,
                &format!(
                    "no answer from {to} in {}s: no peer is allowed it, or its tunnel is down",
                    CONNECT_TIMEOUT.as_secs()
                ),
            );
        }
    }
}

/// An `expose`: a listening socket on `tunnel`, re-armed the moment one
/// leaves LISTEN (rule 6), each becoming a leg.
async fn expose_loop(stack: Arc<Stack>, tunnel: SocketAddr, to: String) {
    loop {
        let handle = match stack.listen(tunnel) {
            Ok(handle) => handle,
            Err(e) => return stack.error("expose", &tunnel.to_string(), &to, &e),
        };
        stack
            .until(handle, On::Either, |socket| {
                (socket.state() != tcp::State::Listen).then_some(())
            })
            .await;
        tokio::spawn(expose_leg(stack.clone(), handle, tunnel, to.clone()));
    }
}

/// One inbound connection from the tunnel, dialed to the local target
/// lazily (rule 5) and only once the handshake is done (rule 1). A target
/// that refuses resets the tunnel side at once, so the caller's client
/// sees a closed connection and never a half-open one.
async fn expose_leg(stack: Arc<Stack>, handle: SocketHandle, tunnel: SocketAddr, to: String) {
    let from = tunnel.to_string();
    match tokio::time::timeout(CONNECT_TIMEOUT, stack.established(handle)).await {
        Ok(Ok(())) => {}
        // Either way the socket is nobody's connection: reset in
        // SYN-RECEIVED (back to LISTEN, and another listener is armed), or
        // a handshake that never finished.
        Ok(Err(_)) | Err(_) => {
            stack.abort(handle);
            return stack.release(handle);
        }
    }
    match tokio::net::TcpStream::connect(&to).await {
        Ok(conn) => pump(&stack, handle, conn).await,
        Err(e) => {
            stack.abort(handle);
            stack.release(handle);
            stack.error("expose", &from, &to, &format!("cannot dial {to}: {e}"));
        }
    }
}

/// Bytes both ways between a smoltcp socket and a tokio stream, each
/// direction to its own end (rule 2), gated by the stack (rule 3).
async fn pump(stack: &Stack, handle: SocketHandle, conn: tokio::net::TcpStream) {
    let (mut reader, mut writer) = conn.into_split();
    let inbound = async {
        // Local bytes into the tunnel; a local EOF is our FIN.
        let mut buf = vec![0u8; BUFFER];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => {
                    stack.close(handle);
                    return true;
                }
                Ok(n) => {
                    let mut sent = 0;
                    while sent < n {
                        match stack.send(handle, &buf[sent..n]).await {
                            Ok(more) => sent += more,
                            Err(()) => return false,
                        }
                    }
                }
                Err(_) => return false,
            }
        }
    };
    let outbound = async {
        // Tunnel bytes out; the far side's FIN is our local shutdown.
        let mut buf = vec![0u8; BUFFER];
        loop {
            let n = stack.recv(handle, &mut buf).await;
            if n == 0 {
                let _ = writer.shutdown().await;
                return true;
            }
            if writer.write_all(&buf[..n]).await.is_err() {
                return false;
            }
        }
    };
    let (in_clean, out_clean) = tokio::join!(inbound, outbound);
    if !(in_clean && out_clean) {
        // The local side went away mid-stream; the far side gets a RST
        // rather than a FIN over bytes it will never see.
        stack.abort(handle);
    }
    stack.release(handle);
}
