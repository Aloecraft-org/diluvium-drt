// relay-leg.js: a page as a parked device on the rendezvous relay
// (crates/drt/src/relay.rs, doc/SshInBrowser.md).
//
// A page has no inbound address, which is the same problem a laptop behind
// CGNAT has, and it gets the same answer: park an outbound leg on a relay
// by label, and let the relay splice it to whoever claims the label. Then
// `ssh -o ProxyCommand="drt tunnel wss://.../s/<label>?k=..."` reaches the
// page with no new protocol on either side.
//
// The relay's wire is URLs, HTTP status and raw bytes -- there is no
// control message, because the original clients were websocat. So this
// file is a `WebSocket`, a first-byte test, and a re-park; all of the
// protocol is in the two URL shapes.
//
// surface block:
//   park(url, open, { onEvent }) -> { close() }
//     url      wss://<label>--tunnel.<zone>/park/<label>?k=<park_key>
//     open     () -> a DrtSocket for one claimed session. Called once per
//              claim -- typically `sshServer.serve(onShell)`.
//     onEvent  (name, detail) for 'parked' | 'claimed' | 'closed' |
//              'error'. A host that wants to show "reachable" watches
//              this; nothing here decides how to say it.
//   RETRY_MS, RETRY_MAX_MS: the re-park backoff after a leg the relay
//     dropped. Its idle timeout is 300s and it expects the device to come
//     back; a page that does not is a device that went home.

const RETRY_MS = 1000;
const RETRY_MAX_MS = 30000;

export function park(url, open, { onEvent = () => {} } = {}) {
  const legs = new Set();
  let stopped = false;
  let backoff = RETRY_MS;
  let retry = null;

  function parkOne() {
    if (stopped) return;
    let ws;
    try {
      ws = new WebSocket(url);
    } catch (e) {
      onEvent('error', e);
      return repark();
    }
    ws.binaryType = 'arraybuffer';
    legs.add(ws);
    let socket = null;

    ws.onopen = () => {
      backoff = RETRY_MS;
      onEvent('parked', url);
    };

    ws.onmessage = (event) => {
      if (typeof event.data === 'string') return; // the relay sends binary
      if (socket === null) {
        // The first byte *is* the claim -- there is no control message to
        // wait for. So this leg is a session now, and a fresh one is
        // parked for whoever calls next: replenish-on-claim is the
        // relay's whole concurrency story.
        socket = open();
        drain(ws, socket);
        parkOne();
        onEvent('claimed', url);
      }
      socket.deliver(new Uint8Array(event.data));
    };

    ws.onerror = (e) => onEvent('error', e);

    ws.onclose = () => {
      legs.delete(ws);
      if (socket) {
        socket.close();
        onEvent('closed', url);
      } else {
        // Never claimed: the relay's idle timeout, which expects the
        // device to come back rather than to have gone away.
        repark();
      }
    };
  }

  function repark() {
    if (stopped || retry !== null) return;
    retry = setTimeout(() => {
      retry = null;
      parkOne();
    }, backoff);
    backoff = Math.min(backoff * 2, RETRY_MAX_MS);
  }

  parkOne();
  return {
    /// Stop parking and end every leg. The relay forgets a label as soon
    /// as its legs go, so this is how a page stops being reachable.
    close() {
      stopped = true;
      if (retry !== null) clearTimeout(retry);
      for (const ws of [...legs]) ws.close();
      legs.clear();
    },
  };
}

// depth: one claimed leg's outbound half.
async function drain(ws, socket) {
  for (;;) {
    const out = await socket.nextOutgoing();
    if (out === undefined) break;
    if (ws.readyState !== WebSocket.OPEN) break;
    ws.send(out);
  }
  if (ws.readyState === WebSocket.OPEN) ws.close();
}
