/* tslint:disable */
/* eslint-disable */

/**
 * DRT's swarm, in a page.
 */
export class Swarm {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Panics on purpose, so the browser suite can prove [`guard`] works.
     * Not in `doc/Browser.md`'s table: it is a test surface, and it is
     * named so that nobody mistakes it for one.
     */
    __panicForTests(): void;
    alive(): number;
    budget(id: number): any;
    cachedSize(id: number): number;
    caps(id: number): any;
    /**
     * Explicit, because wasm has no GC hook: nothing tells Rust when JS
     * dropped its last reference.
     */
    free(): void;
    /**
     * Capability gating stays reachable from JS, which is the point of the
     * row `dvs_holds` occupies in `doc/Browser.md`'s table: a page can ask
     * what an instance may do without holding a grant itself.
     */
    holds(id: number, cap: string): boolean;
    /**
     * The roster, as ids — not a pointer, unlike `dvs_instance`.
     */
    ids(): Uint32Array;
    kill(id: number): void;
    /**
     * `host` is the JS object supplying `doc/Browser.md`'s fifteen
     * functions — the diluvium instance lives on that side, because two
     * wasm modules cannot call each other in a browser and JS is the host
     * in the middle.
     */
    constructor(host: any, max_instances: number, spawns_per_step: number);
    parent(id: number): number | undefined;
    push(id: number, queue: string, msgpack: Uint8Array): boolean;
    resident(id: number): boolean;
    /**
     * Start the root instance. Returns its id.
     */
    root(code: string, caps: any, budget: any): number;
    /**
     * One step of the drive loop. Returns the number still alive, which is
     * the loop's own termination condition.
     */
    step(): number;
}

/**
 * The dv ABI these bindings were built against.
 *
 * **Must not throw** (`doc/Browser.md`), and is the wasm equivalent of
 * `drt --version`: the one call a smoke test can make against a freshly
 * instantiated module to prove it is alive and speaks the ABI expected.
 */
export function abiVersion(): number;

/**
 * What this wasm carries, for the same reason `drt buildinfo` exists: a
 * release artifact should say what it is rather than be guessed at from
 * its filename. `BUILDINFO.txt` gains `profile.web.exports` from this.
 */
export function buildInfo(): any;

/**
 * Install a panic hook that prints to the console before the panic is
 * caught, so a developer sees the Rust backtrace as well as the thrown
 * message. Idempotent; call it once from page setup.
 */
export function setPanicHook(): void;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_swarm_free: (a: number, b: number) => void;
    readonly abiVersion: () => number;
    readonly buildInfo: () => any;
    readonly swarm___panicForTests: (a: number) => [number, number];
    readonly swarm_alive: (a: number) => [number, number, number];
    readonly swarm_budget: (a: number, b: number) => [number, number, number];
    readonly swarm_cachedSize: (a: number, b: number) => [number, number, number];
    readonly swarm_caps: (a: number, b: number) => [number, number, number];
    readonly swarm_free: (a: number) => void;
    readonly swarm_holds: (a: number, b: number, c: number, d: number) => [number, number, number];
    readonly swarm_ids: (a: number) => [number, number, number, number];
    readonly swarm_kill: (a: number, b: number) => [number, number];
    readonly swarm_new: (a: any, b: number, c: number) => [number, number, number];
    readonly swarm_parent: (a: number, b: number) => [number, number, number];
    readonly swarm_push: (a: number, b: number, c: number, d: number, e: number, f: number) => [number, number, number];
    readonly swarm_resident: (a: number, b: number) => [number, number, number];
    readonly swarm_root: (a: number, b: number, c: number, d: any, e: any) => [number, number, number];
    readonly swarm_step: (a: number) => [number, number, number];
    readonly setPanicHook: () => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
