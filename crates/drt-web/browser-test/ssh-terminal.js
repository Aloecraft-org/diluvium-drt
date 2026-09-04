// ssh-terminal.js: an SSH session as the terminal object `attach` takes
// (doc/SshInBrowser.md, doc/Browser.md).
//
// `attach` wants four things -- `write(text)`, `onData(cb)`, `cols`,
// `rows` -- and a DrtShell has all four in bytes rather than text. So this
// is a decoder, an encoder, and a read loop, and it is the whole of what
// stands between SSH and the shell a page already runs. There is no second
// terminal implementation: `ego_cli`'s is the one, over xterm.js in a tab
// and over this from a client.
//
// surface block:
//   terminalFor(shell) -> a terminal-shaped object, plus `dispose()`
//     write     text out to the client. Fire-and-forget, as xterm.js's
//               is; the runtime queues and orders it.
//     onData    the client's keystrokes, decoded. Returns a disposable,
//               which is what xterm.js returns and what drt-term.js
//               disposes.
//     cols/rows the window the client asked for.
//   No `input()`: that is xterm.js's "as if the user typed this", and the
//   user here is at the other end of a socket. drt-term.js's `run` and
//   `reset` check for it and say so.

export function terminalFor(shell) {
  const encoder = new TextEncoder();
  // Streaming, because a multi-byte character can be split across two SSH
  // packets and half of one is not a character.
  const decoder = new TextDecoder('utf-8', { fatal: false });
  // A list, because xterm.js's `onData` is one and callers rely on it:
  // drt-term.js registers a second listener for Ctrl+C *after* the editor
  // has registered its own. A single slot silently replaces the editor,
  // and the terminal renders a prompt that never echoes.
  const listeners = new Set();
  let live = true;

  const pump = (async () => {
    for (;;) {
      const bytes = await shell.read();
      if (bytes === undefined) break;
      const text = decoder.decode(bytes, { stream: true });
      for (const listener of [...listeners]) listener(text);
    }
    live = false;
  })();

  return {
    // Getters, not values: a client that resizes its window changes what
    // these answer, and `ego_cli` asks on every keystroke -- so a resize
    // arrives with no event plumbing on this side at all.
    get cols() {
      return shell.cols;
    },
    get rows() {
      return shell.rows;
    },
    write(text) {
      if (live) shell.write(encoder.encode(text));
    },
    onData(callback) {
      listeners.add(callback);
      return {
        dispose() {
          listeners.delete(callback);
        },
      };
    },
    /// End the session. The status is a shell's exit status, so 0 is a
    /// clean logout and anything else is what the client reports.
    dispose(status = 0) {
      live = false;
      listeners.clear();
      shell.close(status);
      return pump;
    },
  };
}
