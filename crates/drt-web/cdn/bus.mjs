// bus.mjs: an in-page `net` transport. Every address is a name on one
// bus, and a connection is two ends handing each other's messages across.
// Hand the same bus to several kernels on one page and they reach each
// other by name:
//
//   const net = bus();
//   const a = new DrtTerm(sink, { net });
//   const b = new DrtTerm(sink, { net });
//
// ## surface block
//
// - Entry point: `bus()` -> a transport for `{ net }` (crates/drt-web
//   src/js.rs): `open`, `listen`, `send`, `close`.
// - Configurable: none.
// - Fan-out: none. A peer is named `<address>#<n>`, n counting the
//   connections the bus has made.
//
// The JavaScript twin of drt-connector-net's `bus::Bus`, which the native
// tests run on; a transport over WebRTC answers the same four calls.

export function bus() {
  const listeners = new Map();
  let made = 0;
  return {
    listen(address, acceptor) {
      if (listeners.has(address)) throw new Error(`'${address}' already has a listener`);
      listeners.set(address, acceptor);
      return { address };
    },
    open(address, link) {
      const acceptor = listeners.get(address);
      if (!acceptor) throw new Error(`nobody listens on '${address}'`);
      const near = { link, far: null };
      const far = { link: null, far: near };
      near.far = far;
      made += 1;
      far.link = acceptor.connection(far, `${address}#${made}`);
      return near;
    },
    send(conn, bytes) {
      if (!conn.far) throw new Error('the far end is gone');
      conn.far.link.receive(bytes);
    },
    close(conn) {
      if ('address' in conn) {
        listeners.delete(conn.address);
        return;
      }
      const far = conn.far;
      conn.far = null;
      if (far) {
        far.far = null;
        far.link.closed('the far end closed');
      }
    },
  };
}
