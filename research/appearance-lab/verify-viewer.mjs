import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { pathToFileURL } from 'node:url';

const prepared = resolve(process.argv[2] || 'research/appearance-lab/pilot');
const scratch = await mkdtemp(join(tmpdir(), 'harness-appearance-viewer-'));
const browser = spawn('chromium', [
  '--headless', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
  '--disable-background-networking', '--remote-debugging-port=0',
  `--user-data-dir=${scratch}`, 'about:blank',
], { stdio: ['ignore', 'ignore', 'pipe'] });
let errors = '';
browser.stderr.on('data', chunk => { errors += chunk; });
const pause = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
let socket;
try {
  let port;
  for (let attempt = 0; attempt < 100; attempt++) {
    if (browser.exitCode !== null) throw new Error(`Headless browser stopped: ${errors}`);
    try {
      port = (await readFile(join(scratch, 'DevToolsActivePort'), 'utf8')).split('\n')[0];
      break;
    } catch (error) {
      if (error.code !== 'ENOENT') throw error;
      await pause(100);
    }
  }
  assert(port, 'Headless browser must start');
  const targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  socket = new WebSocket(targets.find(target => target.type === 'page').webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve, { once: true });
    socket.addEventListener('error', reject, { once: true });
  });
  const pending = new Map();
  const exceptions = [];
  let sequence = 0;
  socket.addEventListener('message', event => {
    const message = JSON.parse(event.data);
    if (message.method === 'Runtime.exceptionThrown') exceptions.push(message.params.exceptionDetails);
    if (!message.id) return;
    const request = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) request.reject(new Error(JSON.stringify(message.error)));
    else request.resolve(message.result);
  });
  const call = (method, params = {}) => new Promise((resolve, reject) => {
    const id = ++sequence;
    pending.set(id, { resolve, reject });
    socket.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async expression => {
    const result = await call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    assert(!result.exceptionDetails, JSON.stringify(result.exceptionDetails));
    return result.result.value;
  };
  await call('Runtime.enable');
  await call('Page.enable');
  await call('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1200, deviceScaleFactor: 1, mobile: false });
  await call('Page.navigate', { url: pathToFileURL(join(prepared, 'viewer.html')).href });
  for (let attempt = 0; attempt < 50; attempt++) {
    if (await evaluate("document.readyState === 'complete' && !!document.querySelector('#current')?.textContent")) break;
    await pause(100);
  }
  assert.equal(await evaluate("element('current').textContent"), 'A · One Dark');
  const loaded = await evaluate(`Promise.all(entries.flatMap(entry => ['reading', 'states'].map(async scene => {
    const image = new Image(); image.src = 'captures/' + entry.id + '-' + scene + '.png';
    await image.decode(); return [image.naturalWidth, image.naturalHeight];
  })))`);
  assert.equal(loaded.length, 24);
  assert(loaded.every(size => size[0] === 1280 && size[1] === 900));
  await evaluate("document.querySelector('[data-pair=warm-paper]').click()");
  assert.equal(await evaluate("element('reference').value"), 'one-light');
  assert.equal(await evaluate("element('candidate').value"), 'warm-paper');
  assert.equal(await evaluate("element('show-b').getAttribute('aria-pressed')"), 'true');
  await evaluate("dispatchEvent(new KeyboardEvent('keydown', {key: 'a'}))");
  assert.equal(await evaluate("element('current').textContent"), 'A · One Light');
  await evaluate("element('scene').value = 'states'; element('scene').dispatchEvent(new Event('change'))");
  assert((await evaluate("element('capture').src")).endsWith('one-light-states.png'));
  await evaluate("element('scale').value = 'pixel'; element('scale').dispatchEvent(new Event('change'))");
  assert.equal(await evaluate("element('capture').style.width"), '1280px');
  await evaluate("element('scale').value = 'fit'; element('scale').dispatchEvent(new Event('change')); element('show-b').click(); element('capture').decode()");
  const screenshot = await call('Page.captureScreenshot', { format: 'png' });
  await writeFile(join(scratch, 'viewer-wide.png'), Buffer.from(screenshot.data, 'base64'));
  await call('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 1, mobile: false });
  await pause(100);
  assert(await evaluate('document.documentElement.scrollWidth <= innerWidth'), 'Page controls must fit a narrow viewport');
  const narrow = await call('Page.captureScreenshot', { format: 'png', captureBeyondViewport: true });
  await writeFile(join(scratch, 'viewer-narrow.png'), Buffer.from(narrow.data, 'base64'));
  assert.deepEqual(exceptions, []);
  console.log(`Viewer verified: 24 images, A/B controls, preset pairing, scenes, scale modes, narrow layout.\nScreenshots: ${scratch}`);
} finally {
  socket?.close();
  if (browser.exitCode === null) {
    browser.kill('SIGTERM');
    await Promise.race([new Promise(resolve => browser.once('exit', resolve)), pause(5000)]);
    if (browser.exitCode === null) browser.kill('SIGKILL');
  }
}
