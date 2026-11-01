// A page that runs drt and answers one connector family itself.
import { kernel } from '../../crates/drt-web/cdn/dist/drt.min.js';  // built by script/drt-js.sh
import { storage } from './storage.js';

const out = document.querySelector('pre');
const decoder = new TextDecoder();
const drt = await kernel((fd, bytes) => out.append(decoder.decode(bytes)), { storage });

for (const name of ['app.dlua', 'app.json', 'locked.json']) {
  drt.putFile(name, new Uint8Array(await (await fetch(name)).arrayBuffer()));
}

for (const argv of [['drt', 'run', '--config', 'app.json'], ['drt', 'run', '--config', 'locked.json']]) {
  out.append(`$ ${argv.join(' ')}\n`);
  const session = drt.exec(argv);
  // example: omits a page's own scheduling; a tick runs the program until it waits
  for (let step = session.tick(); !step.done; step = session.tick()) {
    await new Promise((r) => setTimeout(r, step.sleepMs ?? 0));
  }
  out.append('\n');
}
document.body.dataset.done = 'true';
