// drt.js in a page: a terminal that runs drt, and a panel that watches it.
import Alpine from 'https://cdn.jsdelivr.net/npm/alpinejs@3.14.9/dist/module.esm.js';
import { loadRuntime } from './drt.min.js';
import { startTerminal } from './terminal.js';
import { inspector } from './inspector.js';

const drt = await loadRuntime();                 // the browser build of drt, unpacked from drt.min.js
const cli = startTerminal(drt, document.getElementById('terminal'));

for (const name of ['app.json', 'app.dlua']) {   // the program the terminal will start
  const bytes = await (await fetch(`app/${name}`)).arrayBuffer();
  cli.term.putFile(name, new Uint8Array(bytes));
}

Alpine.data('inspector', () => inspector(cli));
Alpine.start();
