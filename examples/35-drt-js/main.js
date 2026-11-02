// drt.js in a page: a terminal running drt, and panels that watch it.
import Alpine from 'https://cdn.jsdelivr.net/npm/alpinejs@3.14.9/dist/module.esm.js';
import { terminal } from './drt.min.js';  // copied here from the build, see README.md
import { inspector } from './inspector.js';
import { files } from './files.js';

const fs = await files('files');           // an aloelite volume, shown in <section id="files">
const cli = await terminal('terminal', fs ? { backings: { fs: fs.backing } } : {});
cli.run('drt');                            // bare drt prints its usage, as the binary does
fs?.follow(cli);

Alpine.data('inspector', () => inspector(cli));
Alpine.start();
