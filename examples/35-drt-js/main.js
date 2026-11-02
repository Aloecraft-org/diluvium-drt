// drt.js in a page: a terminal running drt, and a panel that watches it.
import Alpine from 'https://cdn.jsdelivr.net/npm/alpinejs@3.14.9/dist/module.esm.js';
import { loadRuntime } from './drt.min.js';  // copied here from the build, see README.md
import { startTerminal } from './terminal.js';
import { inspector } from './inspector.js';

const drt = await loadRuntime();                 // the browser build of drt, unpacked from drt.min.js
const cli = startTerminal(drt, document.getElementById('terminal'));

Alpine.data('inspector', () => inspector(cli));
Alpine.start();
