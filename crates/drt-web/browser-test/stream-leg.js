// stream-leg.js: the page's SSH server over a browser access stream
// (doc/BrowserAccess.md §10), as relay-leg.js puts it over a relay leg.
//
// A page that answers a session serves `ssh` by handing each stream a
// peer opens to it here, with a fresh DrtSocket from `server.serve(...)`.
// The stream is Web Streams of bytes; the socket is `deliver` in and
// `nextOutgoing` out. This joins the two and closes each when the other
// ends.
//
// surface block:
//   splice(stream, socket)
//     stream  {readable, writable}, as drt_browser_access.js serves it
//     socket  a DrtSocket, typically `sshServer.serve(onShell)`

export function splice(stream, socket) {
  (async () => {
    const reader = stream.readable.getReader();
    for (;;) {
      const { value, done } = await reader.read().catch(() => ({ done: true }));
      if (done) break;
      socket.deliver(value);
    }
    socket.close();
  })();
  (async () => {
    const writer = stream.writable.getWriter();
    for (;;) {
      const out = await socket.nextOutgoing();
      if (out === undefined) break;
      try {
        await writer.write(out);
      } catch {
        break;
      }
    }
    await writer.close().catch(() => {});
  })();
}
