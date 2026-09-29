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
  scope: ScopeEntry[];
  limits: { max_streams: number };
  [key: string]: unknown;
}

export interface ScopeEntry {
  /** What the page speaks over the stream; the host enforces host and port only. */
  scheme: 'http' | 'https' | 'ssh' | string;
  host: string;
  port: number;
  label?: string;
}

export interface OfferOptions {
  /** STUN servers. v1 has no relay, so TURN entries buy nothing. */
  iceServers?: RTCIceServer[];
  /** How long to wait for ICE gathering before publishing (§3.1). Default 2000. */
  gatherTimeoutMs?: number;
  /** For a runtime without a global RTCPeerConnection. */
  RTCPeerConnection?: typeof RTCPeerConnection;
}

export interface AcceptOptions {
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
  readonly hello: Hello;
  readonly pc: RTCPeerConnection;
  /**
   * A TCP stream to `host:port`, which must match an entry in
   * `hello.scope`. Usable at once: Wisp v1 has no "connected" packet, so
   * a refusal arrives as `closed` rejecting with a StreamClosed.
   */
  connect(host: string, port: number): Stream;
  /** End the session: every stream fails, and the connection closes. */
  close(): void;
  /** Resolves, with why, when the session ends for any reason. */
  readonly closed: Promise<string>;
}

export interface Stream {
  readonly id: number;
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

export function parseRecord(input: string | object): BrowserAccessRecord;
export function recordFromSdp(sdp: string): BrowserAccessRecord;
export function answerSdp(hostRecord: string | object, mid: string): string;
export function fingerprintHex(f: string): string;
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
export const MESSAGE_MAX: 16384;
export const DATA_MAX: 16379;
export const WISP: { readonly CONNECT: 1; readonly DATA: 2; readonly CONTINUE: 3; readonly CLOSE: 4 };
export const CLOSE_REASON: Readonly<Record<number, string>>;
