import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';

const dist = new URL('../dist/', import.meta.url);
// A deployment must contain only these public assets, even if local files
// or credentials have accidentally been placed elsewhere in the project.
assert.deepEqual((await readdir(dist)).sort(), [
  '404.html', '_headers', 'favicon.png', 'index.html', 'licenses.html',
  'robots.txt', 'screenshot_mac.png', 'styles.css',
]);
const css = await readFile(new URL('styles.css', dist), 'utf8');
assert.ok(css.includes('#222831') && css.includes('#76abae'), 'App palette missing');
for (const name of ['index.html', '404.html', 'licenses.html']) {
  const html = await readFile(new URL(name, dist), 'utf8');
  assert.ok(!html.includes('{{'), `Unrendered template in ${name}`);
  for (const [, url] of html.matchAll(/(?:href|src)="(\/[^"#]*)"/g)) {
    const file = url === '/' ? 'index.html' : url === '/licenses' ? 'licenses.html' : url.slice(1);
    await readFile(new URL(file, dist));
  }
}
console.log('Verified public asset boundary, palette, and local page links.');
