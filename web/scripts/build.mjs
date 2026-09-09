import { copyFile, mkdir, rm } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

const root = new URL('../', import.meta.url);
const dist = new URL('dist/', root);
await rm(dist, { recursive: true, force: true });
await mkdir(dist, { recursive: true });

// Explicit inputs only: never publish the repository, environment files,
// Wrangler state, or arbitrary files added to public/.
const assets = [
  ['src/index.html', 'index.html'],
  ['src/404.html', '404.html'],
  ['src/privacy.html', 'privacy.html'],
  ['public/licenses.html', 'licenses.html'],
  ['public/_headers', '_headers'],
  ['public/robots.txt', 'robots.txt'],
  ['../assets/screenshot_mac.png', 'screenshot_mac.png'],
  ['../packaging/icons/32.png', 'favicon.png'],
];
for (const [source, destination] of assets) {
  await copyFile(new URL(source, root), new URL(destination, dist));
}
console.log(`Built static assets in ${fileURLToPath(dist)}`);
