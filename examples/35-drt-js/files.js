// The filesystem panel: the kernel's aloelite volume, in the file explorer
// aloelite ships. Both are copied in beside drt.min.js (README.md); without
// them the terminal runs on an empty memory filesystem and the panel says so.
// example: omits OPFS, which needs a Worker (crates/drt-web/browser-test/opfs-worker.js)
import { aloelite } from './drt.min.js';

export async function files(elementId) {
  const panel = document.getElementById(elementId);
  let engine, renderExplorer;
  try {
    [engine, { renderExplorer }] = await Promise.all([import('./aloelite_wasm.js'), import('./aloelite-explorer.js')]);
  } catch {
    panel.textContent = 'no aloelite build beside this page: README.md says how to make one';
    return null;
  }
  await engine.default();
  const file = await aloelite.open(engine, { store: 'memory', name: 'drt' });
  const backing = aloelite.attach(file, { volume: 'drt' });      // what the kernel reads and writes
  const explorer = await renderExplorer(panel, file.fs, {        // the same file, so edits here are files there
    volume: 'drt',
    editable: true,
    mountArgs: { allow_overlap: true },                           // the kernel already holds the rw mount
  });
  return {
    backing,
    // A command may have written files: show them once it ends.
    follow(cli) {
      cli.onCommand((session) => session || explorer.refresh());
    },
  };
}
