// drt.js in a page: a terminal running drt, and a panel that watches it.
import Alpine from 'https://cdn.jsdelivr.net/npm/alpinejs@3.14.9/dist/module.esm.js';
import { terminal } from './drt.min.js';  // copied here from the build, see README.md
import { inspector } from './inspector.js';

const cli = await terminal('terminal');    // xterm.js running drt, inside <section id="terminal">
cli.run('drt');                            // bare drt prints its usage, as the binary does

Alpine.data('inspector', () => inspector(cli));
Alpine.start();
