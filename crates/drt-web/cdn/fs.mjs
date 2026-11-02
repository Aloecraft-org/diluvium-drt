// fs.mjs: an aloelite volume as a kernel's filesystem. The page loads the
// aloelite engine itself (crates aloelite-wasm in Aloecraft-org/aloelite;
// drt.js carries none of it), `open` says where the volume file lives, and
// `attach` mounts a volume and answers the seven calls a kernel's
// filesystem makes:
//
//   import * as engine from './aloelite_wasm.js';
//   await engine.default();
//   const file = await open(engine, { store: 'opfs', name: 'node.fs' });
//   const shared = attach(file, { volume: 'docs' });
//   const a = await kernel(sinkA, { fs: shared });           // a and b share it
//   const b = await kernel(sinkB, { fs: shared });
//   const c = await kernel(sinkC, { fs: attach(file, { volume: 'c' }) }); // its own
//
// ## surface block
//
// - Entry points:
//   - `open(engine, options)` -> a volume file. `engine` is aloelite-wasm's
//     module. Options: `store`, one of STORES; `name`, the file's name in
//     the pool; `directory`, the pool's OPFS directory (aloelite's default
//     when absent); `bytes`, a whole file to open first (an `export`),
//     for either store. The file has `fs`, `export()`, `close()`,
//     `delete()`, and `addCapacity(n)`; the last two are the pool's, and
//     a memory file refuses them with `unsupported`. An OPFS file another
//     Worker holds fails with `busy`, by name, at once. A pool without
//     room for the file fails with `opfs` and says to call `addCapacity`.
//   - `attach(file, options)` -> a filesystem for `{ fs }`
//     (crates/drt-web/src/js.rs), over a file from `open` or an
//     aloelite-wasm `Fs`. Options: `volume`, the volume's name, made when
//     the file has none by that name (DEFAULT_VOLUME); `mountPoint`, the
//     volume path the kernel's `/` is (the volume root); `access`, `rw` or
//     `ro`; `pin`, the volume's PIN. The result carries `mount` and
//     `unmount()`. One result handed to several kernels is one filesystem
//     they share; attach again for one of a kernel's own.
//   Every call is synchronous, so the engine must be on drt's own thread:
//   the page for `memory`, the Worker drt runs in for `opfs`. An engine
//   behind postMessage answers with promises and cannot back a kernel.
// - Configurable: DEFAULT_VOLUME, STORES.
// - Fan-out: STORES, where a file lives; CALLS, the seven calls drt makes,
//   each by the Mount API operations it takes. Every path drt passes is
//   absolute and folded. An aloelite error passes through with its `code`
//   (`not_found`, `not_a_container`, `not_an_entry`, ...), which drt reads
//   as the matching io error.
//
// Not here: checkpoint timing for a memory file (`export()` is the
// whole image, taken when the page says); how files are named; and
// anything `host.fs.*` already answers.

export const DEFAULT_VOLUME = 'drt';

//   memory   Fs.openMemory(bytes?): gone with the handle unless exported; works anywhere
//   opfs     Pool.open(name) under its Web Lock: survives a reload; a Worker
export const STORES = ['memory', 'opfs'];

//   stat(path)                 stat         -> null when absent, else { dir, size }
//   read(path)                 read_all     -> Uint8Array
//   write(path, bytes, append) write_all, append, or create_entry when absent
//   list(path)                 list         -> names, sorted
//   remove(path)               remove       a file; a container is refused
//   mkdirs(path)               create_container for each missing level
//   removeAll(path)            remove_recursive; at `/`, each child of it
export const CALLS = ['stat', 'read', 'write', 'list', 'remove', 'mkdirs', 'removeAll'];

export async function open(engine, options = {}) {
  const store = options.store ?? 'memory';
  if (store === 'memory') {
    const fs = engine.Fs.openMemory(options.bytes ?? null);
    const none = (what) => () => refuse('unsupported', `a 'memory' file has no ${what}; use store 'opfs'`);
    return { store, fs, export: () => fs.export(), delete: none('delete'), addCapacity: none('pool'), close: () => fs.close() };
  }
  if (store !== 'opfs') refuse('usage', `store is one of ${STORES.join(', ')}, not '${store}'`);
  const name = options.name ?? refuse('usage', "an 'opfs' file needs a name");
  const pool = await engine.Pool.install(options.directory ? { directory: options.directory } : undefined);
  if (options.bytes) pool.import(name, options.bytes);
  // A volume file and its rollback journal take two of the pool's files,
  // and the pool lists both. Checked here because a full pool otherwise
  // surfaces as SQLite failing to open the journal.
  const others = pool.list().filter((n) => n !== name && n !== `${name}-journal`).length;
  if (pool.capacity < others + 2) {
    refuse('opfs', `the pool has ${pool.capacity} files, ${others} in use, and '${name}' needs 2 (it and its journal); call addCapacity(n) first`);
  }
  const fs = await pool.open(name);
  return {
    store,
    name,
    fs,
    export: () => pool.export(name),
    close: () => fs.close(),
    async delete() {
      if (!fs.closed) await fs.close();
      return pool.delete(name);
    },
    addCapacity: (n) => pool.addCapacity(n),
  };
}

// The mounts this page made, by engine handle.
const LIVE = new WeakMap();

export function attach(file, options = {}) {
  const fs = file.fs ?? file;
  const name = options.volume ?? DEFAULT_VOLUME;
  const pin = options.pin ?? null;
  const volume =
    fs.call('resolve_volume_name', { name }) ?? fs.call('create_volume', { name, pin }).id;
  if (!LIVE.has(fs)) LIVE.set(fs, new Set());
  const live = LIVE.get(fs);
  // Mount rows are durable, so a file opened again after a reload, or
  // from an export, still holds the last session's `active` rows, and
  // aloelite admits one rw mount per subtree. The handle is single-writer
  // (an OPFS file is held under its Web Lock, a memory one by nobody
  // else), so a live row this page did not make is left over, and is
  // ended before mounting.
  for (const row of fs.call('list_mounts', { volume })) {
    if (row.state !== 'unmounted' && !live.has(row.id)) fs.call('unmount', { mount: row.id });
  }
  const mount = fs.call('mount', { volume, pin, at: options.mountPoint ?? null, access: options.access ?? null });
  live.add(mount);
  const call = (op, args) => fs.call(op, { mount, ...args });
  const stat = (path) => {
    try {
      const info = call('stat', { path });
      return { dir: info.type === 'container', size: Number(info.size ?? 0n) };
    } catch (e) {
      if (e.code === 'not_found') return null;
      throw e;
    }
  };
  const names = (path) => call('list', { path }).map((entry) => entry.name).sort();
  return {
    mount,
    unmount() {
      live.delete(mount);
      fs.call('unmount', { mount });
    },
    stat,
    read: (path) => call('read_all', { path }),
    write(path, data, append) {
      const at = stat(path);
      if (at === null) call('create_entry', { path, data });
      else if (at.dir) refuse('not_an_entry', `Is a directory: ${path}`);
      else if (append) call('append', { path, data });
      else call('write_all', { path, data });
    },
    list: names,
    remove(path) {
      if (stat(path)?.dir) refuse('not_an_entry', `Is a directory: ${path}`);
      call('remove', { path });
    },
    mkdirs(path) {
      let here = '';
      for (const part of path.split('/').filter(Boolean)) {
        here += `/${part}`;
        const at = stat(here);
        if (at === null) call('create_container', { path: here });
        else if (!at.dir) refuse('not_a_container', `Not a directory: ${here}`);
      }
    },
    removeAll(path) {
      if (stat(path) === null) return;
      // The mount point is the kernel's `/`, and it stays: what `rm -r /`
      // leaves is an empty root, as the memory filesystem has it.
      if (path === '/') for (const child of names('/')) call('remove_recursive', { path: `/${child}` });
      else call('remove_recursive', { path });
    },
  };
}

function refuse(code, message) {
  throw Object.assign(new Error(message), { code });
}
