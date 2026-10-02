// Types for drt_browser_access.js: the browser half of DRT browser access
// (doc/BrowserAccess.md §2-§6). Signaling is the caller's; see the module.

/** A peer's record (§2), as parseRecord returns it: exactly these keys. */
export interface BrowserAccessRecord {
  v: 1;
  /** ICE ufrag, 4-32 ice-chars. */
  u: string;
  /** ICE password, 22-64 ice-chars. */
  p: string;
  /** SHA-256 of the DTLS certificate, standard base64, 44 characters. */
  f: string;
  /** 0-8 `candidate:` lines (§2.1). */
  c: string[];
}

/** The host's `hello` on `control` (§5). */
export interface Hello {
  v: 1;
  t: 'hello';
  service?: string;
  default?: ScopeEntry;
  /** What the peer lets a caller name by address; empty when it names services only (doc/P2P.md §7.2). */
  scope: ScopeEntry[];
  /** Named services the peer serves (§10.3). */
  services?: string[];
  /** Capability names a program or REPL behind the peer may hold (doc/P2P.md §7.2). */
  caps?: string[];
  /** True when the peer is a relay that calls a destination the caller names (doc/P2P.md §4.1). */
  forwarding?: boolean;
  limits: { max_streams: number };
  /** The peer understands END (§6): a closed writable is a half-close, not a close. */
  half_close?: boolean;
  [key: string]: unknown;
}

export interface ScopeEntry {
  /** What the page speaks over the stream; the host enforces host and port only. */
  scheme: 'http' | 'https' | 'ssh' | string;
  host: string;
  port: number;
  label?: string;
}

/** What a side serves (§10.3): a name, and what to do with a stream to it. */
export type Services = Record<string, (stream: Stream, session: Session) => void>;

export interface OfferOptions {
  /** Services this side serves to the peer (§10.2). */
  services?: Services;
  /** STUN servers. v1 has no relay, so TURN entries buy nothing. */
  iceServers?: RTCIceServer[];
  /** How long to wait for ICE gathering before publishing (§3.1). Default 2000. */
  gatherTimeoutMs?: number;
  /** For a runtime without a global RTCPeerConnection. */
  RTCPeerConnection?: typeof RTCPeerConnection;
}

/**
 * The answerer's DTLS fingerprint, checked before the answer is applied:
 * `SHA256:<base64>` to compare against, or a function asked with it.
 */
export type Fingerprint = string | ((fingerprint: string) => boolean | Promise<boolean>);

export interface AcceptOptions {
  fingerprint?: Fingerprint;
  /** How long to wait for both channels, `hello` and the first CONTINUE. Default 15000. */
  timeoutMs?: number;
}

/** An offer made and gathered, waiting for the host's record. */
export interface Pending {
  /** The browser's record, to send to the host through signaling. */
  record: BrowserAccessRecord;
  /** The same record as compact JSON text. */
  recordText: string;
  /** The underlying peer connection. */
  pc: RTCPeerConnection;
  /**
   * Take the host's record (object or text, §2) and connect. Resolves once
   * both channels are open and `hello` and the initial credit have
   * arrived; rejects, and closes the connection, otherwise. Once only.
   */
  accept(hostRecord: BrowserAccessRecord | string | object, options?: AcceptOptions): Promise<Session>;
  /** Give up before accepting. */
  close(): void;
}

export interface Session {
  /** The peer's hello; null when the peer serves nothing (§10.2). */
  readonly hello: Hello;
  readonly pc: RTCPeerConnection;
  /** 'caller' opens odd stream ids, 'answerer' even ones (§10.2). */
  readonly role: 'caller' | 'answerer';
  /**
   * A TCP stream to `host:port`, which must match an entry in
   * `hello.scope`; with no port, a stream to the named service `host`
   * (§10.3); with no arguments, or a port alone, a stream to whatever the
   * peer forwards to, at that port (doc/P2P.md §5.1). Usable at once: Wisp
   * v1 has no "connected" packet, so a refusal arrives as `closed`
   * rejecting with a StreamClosed.
   */
  connect(host?: string | number, port?: number): Stream;
  /** Report a stream's terminal size over `control` (doc/P2P.md §5.2). */
  resize(stream: Stream | number, cols: number, rows: number): void;
  /** End the session: every stream fails, and the connection closes. */
  close(): void;
  /** Resolves, with why, when the session ends for any reason. */
  readonly closed: Promise<string>;
}

export interface Stream {
  readonly id: number;
  /** What the far side granted this stream, once it said (doc/P2P.md §7.2); null until then. */
  readonly caps: string[] | null;
  /** Resolves with `caps` when the far side says; rejects if the stream ends first. */
  readonly granted: Promise<string[]>;
  /** This side will write no more (END, §6): reads go on until the peer ends; `close()` where the peer lacks END. */
  end(): void;
  /** Bytes from the target. Ends when the target closes cleanly. */
  readonly readable: ReadableStream<Uint8Array>;
  /** Bytes to the target, split at 16379 per packet and paced by Wisp credit. */
  readonly writable: WritableStream<Uint8Array | ArrayBuffer>;
  /** Close from this side (sends CLOSE 0x02). */
  close(): void;
  /**
   * Resolves when the target closed cleanly or this side closed it;
   * rejects with a StreamClosed otherwise.
   */
  readonly closed: Promise<void>;
}

export class RecordError extends Error {
  readonly code:
    | 'TooLong' | 'NotJson' | 'NotAnObject' | 'Version' | 'Missing' | 'WrongType'
    | 'Ufrag' | 'Pwd' | 'Fingerprint' | 'TooManyCandidates' | 'Candidate';
}

export class StreamClosed extends Error {
  /** The host's CLOSE byte (see CLOSE_REASON), or null when the session ended under the stream. */
  readonly reason: number | null;
}

export function offer(options?: OfferOptions): Promise<Pending>;

export interface AnswerOptions extends OfferOptions, AcceptOptions {
  /** The `service` label this side's hello carries. */
  label?: string;
  /** A certificate the page keeps, so its fingerprint holds across sessions. */
  certificates?: RTCCertificate[];
}
/** A page answering (§10.4): its record, for signaling, and the session. */
export interface Answering {
  record: BrowserAccessRecord;
  recordText: string;
  pc: RTCPeerConnection;
  session: Promise<Session>;
  close(): void;
}
export function answer(callerRecord: BrowserAccessRecord | string | object, options?: AnswerOptions): Promise<Answering>;

/** A call as a signalling server lists it (doc/DRT-Signalling.md §4.1). */
export interface IncomingCall {
  id: string;
  /** The caller's record, as text. */
  record: string;
  /** Seconds until the server stops holding it. */
  expires_in: number;
}
export interface ListenOptions extends AnswerOptions {
  /** The name's answerer token: a bearer header on requests, `?k=` on the event stream. */
  token?: string;
  /** Poll interval while no call notification stream is held; LISTEN_POLL_MS. */
  pollMs?: number;
  /** false polls only, never opening the call notification stream. */
  events?: boolean;
  /** Return false to refuse a call; the server tells its caller 410. */
  accept?(call: IncomingCall): boolean | Promise<boolean>;
  /** A call answered and connected. */
  onSession?(session: Session, call: IncomingCall): void;
  /** A failure that did not stop listening; `call` is null for a poll's own. */
  onError?(error: unknown, call: IncomingCall | null): void;
  fetch?: typeof fetch;
  EventSource?: typeof EventSource;
}
/** What `listen` returns. */
export interface Listening {
  /** The cursor the next poll passes back. */
  readonly cursor: string;
  /** Whether the call notification stream is open now. */
  readonly streaming: boolean;
  /** Poll now, after any poll already running. */
  poll(): Promise<void>;
  /** Stop listening. Sessions already made stay up. */
  close(): void;
}
/**
 * Answer every call a signalling server holds for one name:
 * `base` is `…/v1/<name>` (doc/DRT-Signalling.md).
 */
export function listen(base: string, options?: ListenOptions): Listening;
/** The caller's record as the offer a page that answers applies (§10.4). */
export function offerSdp(callerRecord: BrowserAccessRecord | string | object, mid?: string): string;
/** Whether `name` can name a service (§10.3). */
export function isServiceName(name: string): boolean;
/**
 * Direct mode (§3.4): a session from the host's record alone, for a host
 * with `direct` on. The browser chooses its own ICE credentials; nothing is
 * signaled.
 */
export function direct(
  hostRecord: BrowserAccessRecord | string | object,
  options?: Omit<OfferOptions, 'gatherTimeoutMs'> & AcceptOptions,
): Promise<Session>;
/** `sdp` with every `a=ice-ufrag` and `a=ice-pwd` line replaced. */
export function withIceCredentials(sdp: string, ufrag: string, pwd: string): string;

export function parseRecord(input: string | object): BrowserAccessRecord;
export function recordFromSdp(sdp: string): BrowserAccessRecord;
export function answerSdp(hostRecord: string | object, mid: string): string;
export function fingerprintHex(f: string): string;
/** A record's `f` as `drt p2p` prints and takes it: `SHA256:` and unpadded base64. */
export function fingerprintText(f: string): string;

/** A peer address of doc/P2P.md §3 read as `drt p2p` reads it; see `canonicalPeer`. */
export interface PeerAddress {
  kind: 'signal' | 'record' | 'ws';
  /** One spelling per peer, what `drt p2p --show` prints: the key to store credentials under. */
  canonical: string;
  /** Where the caller's request goes, query included; null for a record. */
  url: string | null;
  /** The service a `drt+<service>://` address opens. */
  service: string | null;
  /** The name at a signalling server, when the address has one. */
  name: string | null;
  record: BrowserAccessRecord | null;
}
/** Read a peer address: a `drt://` form, a bare host, an http(s) URL, a record or its text, or a relay URL. Throws when none fits. */
export function canonicalPeer(address: string | object): PeerAddress;
/** Whether v1 can use a candidate line (§2.1); parseRecord drops the rest. */
export function isUsableCandidate(line: string): boolean;

export interface WispPacket {
  type: number;
  stream: number;
  payload: Uint8Array;
  /** On CONTINUE. */
  buffer?: number;
  /** On CLOSE. */
  reason?: number;
  /** On CONNECT: the stream type, port (0 for a named service) and hostname. */
  kind?: number;
  port?: number;
  host?: string | null;
}
export function encodeWisp(
  type: number,
  stream: number,
  body?: { host?: string; port?: number; data?: Uint8Array; buffer?: number; reason?: number },
): Uint8Array;
export function decodeWisp(bytes: Uint8Array | ArrayBuffer): WispPacket | null;

export const GATHER_CAP_MS: number;
export const ACCEPT_TIMEOUT_MS: number;
export const SEND_HIGH_WATER: number;
export const RECORD_VERSION: 1;
export const RECORD_MAX_BYTES: 512;
export const MAX_CANDIDATES: 8;
export const UFRAG_LEN: [number, number];
export const PWD_LEN: [number, number];
export const DIRECT_UFRAG_LEN: 32;
export const SERVE_BUFFER: number;
export const LISTEN_POLL_MS: number;
export const SERVE_MAX_STREAMS: number;
export const MESSAGE_MAX: 16384;
export const DATA_MAX: 16379;
export const WISP: { readonly CONNECT: 1; readonly DATA: 2; readonly CONTINUE: 3; readonly CLOSE: 4 };
export const CLOSE_REASON: Readonly<Record<number, string>>;
