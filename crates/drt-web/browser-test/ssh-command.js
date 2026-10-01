// ssh-command.js: `drt ssh` and the REPL's `:ssh`, in a page
// (doc/SshInBrowser.md, doc/ssh-transport-matrix.md).
//
// The client is ssh.html's (crates/drt-ssh-web): russh compiled to wasm,
// the page handing it a transport. A page cannot open a TCP connection, so
// where the native `drt ssh` dials host:port, this reaches the host the
// ways ssh.html does: a relay claim (what `drt p2p --relay` dials), or WebRTC --
// a host's record (direct mode) or a signalling URL (doc/DRT-Signalling.md).
// The arguments are the native command's, so a line works in both places
// or is refused here by name.
//
// Trust and keys are the page's: a host key is remembered per name in
// `store` (the name the person typed, as known_hosts keys it natively),
// asked about the first time and refused when it changes; `--hostkey`
// pins one instead. Sign-in tries the page's key (`store`'s `key`, the
// record ssh.html keeps) and then a password.
//
// surface block:
//   sshCommand(argv, { client, io, announce }) -> Promise<status>
//     argv    the words after `drt ssh` (or `:ssh`)
//     client  { Ssh, access?, store? }: drt-ssh-web's `Ssh`; the
//             drt_browser_access module, for `--via rtc:`; and a store with
//             async get(key) / set(key, value), in memory if none is given
//     io      what drt-term.js lends a command: write(text), ask(prompt,
//             { secret }) -> line | null, keys(handler) -> release(), and
//             size() -> { cols, rows }
//     announce  say "ssh: <host> closed, exit N" after a session, as the
//             native REPL does after `:ssh`
//   words(text) -> argv: a line split as the page's shell splits words
//   USAGE, RETRIES, RETRY_MS, KEEPALIVE_MS, RESIZE_MS, SIGNAL_MS,
//   PASSWORD_TRIES: the limits, as ssh.html and the native command have them
//   fan-out: transport(), one branch per --via scheme; REFUSALS, what a
//   signalling server's statuses mean to a caller

export const USAGE =
  'usage: drt ssh [user@]host[:port] --via <wss://relay/s/label?k=… | rtc:<record | https://…/v1/name/calls?k=…>>\n' +
  '               [--to service|host:port] [--hostkey SHA256:…] [-l user]\n';
export const RETRIES = 5;
export const RETRY_MS = 250;
export const KEEPALIVE_MS = 20000;
export const RESIZE_MS = 250;
export const SIGNAL_MS = 35000;
export const PASSWORD_TRIES = 3;

const REFUSALS = {
  400: 'the request was malformed',
  401: 'no token, or one the server does not know',
  403: 'this token may not call',
  404: 'no such name on this server',
  410: 'the answerer refused the call',
  413: "this page's record is too large",
  429: 'too many calls are waiting; try again',
  503: 'no answerer is present',
  504: 'nobody answered in time',
};

/// A line split into words the way the page's shell splits them: spaces,
/// and '…' or "…" keeping a word whole.
export function words(text) {
  const out = [];
  let cur = '';
  let quote = null;
  let started = false;
  for (const c of text) {
    if (quote) {
      if (c === quote) quote = null;
      else cur += c;
    } else if (c === "'" || c === '"') {
      quote = c;
      started = true;
    } else if (/\s/.test(c)) {
      if (started || cur) out.push(cur);
      cur = '';
      started = false;
    } else {
      cur += c;
    }
  }
  if (quote) throw new Error('an unclosed quote');
  if (started || cur) out.push(cur);
  return out;
}

export async function sshCommand(argv, { client, io, announce = false }) {
  const fail = (text) => {
    io.write(`drt ssh: ${text}\r\n`);
    return 255;
  };
  let args;
  try {
    args = parse(argv);
  } catch (e) {
    return fail(`${e.message}\r\n${USAGE.replace(/\n/g, '\r\n')}`);
  }
  if (args.help) {
    io.write(USAGE.replace(/\n/g, '\r\n'));
    return 0;
  }
  if (!client || !client.Ssh) return fail('this page has no SSH client (attach it with { ssh: { Ssh } })');
  if (!args.via) {
    return fail(
      'a page cannot open a TCP connection; reach the host with --via wss://<relay>/s/<label>?k=… ' +
        '(a relay claim) or --via rtc:<record or signalling URL>',
    );
  }
  const store = client.store ?? memory();
  let opened;
  try {
    opened = await transport(args, client, io);
  } catch (e) {
    return fail(e.message || String(e));
  }
  const { ssh, session } = opened;
  const done = () => {
    try {
      ssh.close();
    } catch (_) {
      /* already closed */
    }
    session?.close();
  };
  try {
    if (!(await trusted(ssh, args, store, io))) {
      done();
      return 255;
    }
    if (!(await signIn(ssh, args, store, io))) {
      done();
      return fail(`${args.user}@${args.name}: sign-in refused`);
    }
  } catch (e) {
    done();
    return fail(e.message || String(e));
  }
  const status = await shell(ssh, io);
  done();
  if (announce) io.write(`ssh: ${args.target} closed${status == null ? '' : `, exit ${status}`}\r\n`);
  return status ?? 0;
}

// depth: arguments

function parse(argv) {
  const a = { user: null, via: null, to: null, hostkey: null, target: null, help: false };
  for (let i = 0; i < argv.length; i++) {
    const w = argv[i];
    const value = () => {
      if (i + 1 >= argv.length) throw new Error(`${w} needs a value`);
      return argv[++i];
    };
    if (w === '-h' || w === '--help') a.help = true;
    else if (w === '--via') a.via = value();
    else if (w.startsWith('--via=')) a.via = w.slice(6);
    else if (w === '--to') a.to = value();
    else if (w === '--hostkey') a.hostkey = value();
    else if (w === '-l' || w === '--login') a.user = value();
    else if (w.startsWith('-')) throw new Error(`${w} is not a flag this page takes`);
    else if (a.target === null) a.target = w;
    else throw new Error(`one host only; ${w} is a second`);
  }
  if (a.help) return a;
  if (!a.target) throw new Error('no host');
  const at = a.target.lastIndexOf('@');
  if (at > 0) a.user = a.target.slice(0, at);
  let rest = at >= 0 ? a.target.slice(at + 1) : a.target;
  let port = 22;
  const m = rest.match(/^\[([^\]]+)\](?::(\d+))?$/) || rest.match(/^([^:]+)(?::(\d+))?$/);
  if (m) {
    rest = m[1];
    if (m[2]) port = Number(m[2]);
  }
  if (!rest) throw new Error('no host');
  if (!a.user) throw new Error('no user: give one as user@host or -l');
  a.name = port === 22 ? rest : `[${rest}]:${port}`;
  return a;
}

// depth: how the bytes travel

async function transport(args, client, io) {
  const { via } = args;
  if (via.startsWith('ws://') || via.startsWith('wss://')) {
    if (args.to) throw new Error('--to goes with --via rtc:…; a relay claim names its device');
    for (let attempt = 0; ; attempt++) {
      try {
        return { ssh: await client.Ssh.connect(via, args.hostkey || undefined), session: null };
      } catch (e) {
        // 1013: the claim was right and nothing is parked yet. A device
        // parks a fresh leg as soon as another caller takes one.
        if (e && e.code === 1013 && attempt < RETRIES) {
          io.write(`The device is not home yet; retrying (${attempt + 1}/${RETRIES})…\r\n`);
          await new Promise((r) => setTimeout(r, RETRY_MS * 2 ** attempt));
          continue;
        }
        throw refusal(e);
      }
    }
  }
  if (via.startsWith('rtc:')) {
    const access = client.access;
    if (!access) throw new Error('--via rtc: needs the browser access client (attach it with { ssh: { access } })');
    const rest = via.slice(4);
    const session = /^https?:\/\//.test(rest) ? await called(access, rest) : await access.direct(access.parseRecord(rest));
    try {
      const named = !args.to && (session.hello.services || []).includes('ssh');
      const target = named ? null : pick(session.hello, args.to);
      const stream = named ? session.connect('ssh') : target ? session.connect(...target) : session.connect();
      return { ssh: await client.Ssh.connect(stream, args.hostkey || undefined), session };
    } catch (e) {
      session.close();
      throw refusal(e);
    }
  }
  throw new Error(`--via ${via}: expected ws://, wss:// (a relay claim) or rtc:`);
}

function refusal(e) {
  if (e && e.code === 'hostkey') return new Error('the host key is not the one --hostkey pins');
  return e instanceof Error ? e : new Error(String(e));
}

// One POST of this page's record, held until the answerer answers
// (doc/DRT-Signalling.md §3), and its record back.
async function called(access, url) {
  const pending = await access.offer();
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), SIGNAL_MS);
  let res;
  let reply;
  try {
    res = await fetch(url, {
      method: 'POST',
      body: pending.recordText,
      signal: abort.signal,
      headers: { 'content-type': 'text/plain;charset=utf-8' },
    });
    reply = await res.text();
  } catch (e) {
    pending.close?.();
    throw new Error(`${url.split('?')[0]} could not be reached: ${e.message || e}`);
  } finally {
    clearTimeout(timer);
  }
  if (!res.ok) {
    pending.close?.();
    const why = REFUSALS[res.status];
    throw new Error(`${url.split('?')[0]} answered ${res.status}${why ? `: ${why}` : ''}`);
  }
  return pending.accept(reply);
}

// `--to host:port`, else the host's first ssh:// entry, else its default.
function pick(hello, to) {
  if (to) {
    const at = to.lastIndexOf(':');
    if (at < 0) return [to];
    return [to.slice(0, at).replace(/^\[|\]$/g, ''), Number(to.slice(at + 1))];
  }
  // No scope named: a stream to whatever the host forwards to (doc/P2P.md §5.1).
  if (!hello.scope) return null;
  const entry = hello.scope.find((e) => e.scheme === 'ssh') ?? hello.default;
  if (!entry) throw new Error('the host serves no ssh:// target; name one with --to host:port');
  return [entry.host, entry.port];
}

// depth: the host key, then sign-in

async function trusted(ssh, args, store, io) {
  const fp = ssh.hostKey;
  if (args.hostkey) return true; // the connect itself refused any other key
  const key = `host:${args.name}`;
  const known = await store.get(key);
  if (known === fp) return true;
  if (known) {
    io.write(
      `drt ssh: refused the host key of ${args.name} (${fp}): it is NOT the key this page ` +
        `remembers (${known}). Someone may be intercepting this connection, or the host's key ` +
        'was replaced.\r\n',
    );
    return false;
  }
  io.write(`The host ${args.name} is not known to this page.\r\nIts key is ${fp}.\r\n`);
  for (;;) {
    const answer = await io.ask('Trust it and continue? (yes/no) ');
    if (answer === null) return false;
    const a = answer.trim().toLowerCase();
    if (a === 'yes' || a === 'y') break;
    if (a === 'no' || a === 'n') {
      io.write('Not trusted; nothing was sent.\r\n');
      return false;
    }
    io.write('Please type yes or no.\r\n');
  }
  await store.set(key, fp);
  io.write(`Remembered ${args.name}.\r\n`);
  return true;
}

async function signIn(ssh, args, store, io) {
  const key = await store.get('key');
  if (key && key.privateOpenssh && (await ssh.authKey(args.user, key.privateOpenssh))) return true;
  for (let i = 0; i < PASSWORD_TRIES; i++) {
    const pw = await io.ask(`${args.user}@${args.name}'s password: `, { secret: true });
    if (pw === null) return false;
    if (await ssh.authPassword(args.user, pw)) return true;
    io.write('Permission denied, please try again.\r\n');
  }
  return false;
}

// depth: the shell

async function shell(ssh, io) {
  const decoder = new TextDecoder('utf-8', { fatal: false });
  const encoder = new TextEncoder();
  let size = io.size();
  let ended;
  const finished = new Promise((resolve) => (ended = resolve));
  await ssh.shell(
    size.cols,
    size.rows,
    (bytes) => io.write(decoder.decode(bytes, { stream: true })),
    (status) => ended(status),
  );
  // `~.` at the start of a line disconnects; `~~` sends one `~`. OpenSSH's
  // escape, because a session whose network died ignores every other key.
  let lineStart = true;
  let tilde = false;
  const release = io.keys((data) => {
    let send = '';
    for (const c of data) {
      if (tilde) {
        tilde = false;
        if (c === '.') {
          if (send) ssh.write(encoder.encode(send));
          io.write('\r\nConnection closed (~.).\r\n');
          ssh.close();
          ended(null);
          return;
        }
        send += c === '~' ? '~' : `~${c}`;
      } else if (lineStart && c === '~') {
        tilde = true;
        continue;
      } else {
        send += c;
      }
      lineStart = c === '\r' || c === '\n';
    }
    if (send) ssh.write(encoder.encode(send));
  });
  const resizing = setInterval(() => {
    const now = io.size();
    if (now.cols > 0 && now.rows > 0 && (now.cols !== size.cols || now.rows !== size.rows)) {
      size = now;
      ssh.resize(now.cols, now.rows);
    }
  }, RESIZE_MS);
  const beat = setInterval(() => ssh.keepalive().catch(() => {}), KEEPALIVE_MS);
  try {
    return await finished;
  } finally {
    clearInterval(resizing);
    clearInterval(beat);
    release();
  }
}

function memory() {
  const m = new Map();
  return { get: async (k) => m.get(k), set: async (k, v) => void m.set(k, v) };
}
