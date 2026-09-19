import assert from 'node:assert/strict';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import test from 'node:test';
import { isolatedGui } from '../research/claude-native-lab/gui_probe.mjs';

test('composer starts writable and the image lightbox only captures picture clicks', {
  skip: !process.env.HARNESS_UI_TEST_BINARY,
  timeout: 60000,
}, async () => {
  const directory = mkdtempSync('/tmp/harness-transcript-ui-');
  const picture = join(directory, 'picture.png');
  const fixture = join(directory, 'fixture.json');
  writeFileSync(fixture, JSON.stringify({ user: { markdown: 'Image interaction test' }, events: [
    { type: 'tool', kind: 'image', title: 'Viewed image', input: picture, output: picture, path: picture },
  ] }));
  const gui = await isolatedGui(resolve(process.env.HARNESS_UI_TEST_BINARY), {
    ...process.env,
    HARNESS_COMPARISON_FIXTURE: fixture,
    HARNESS_CHATGPT_DESKTOP_VERSION: '0.0.0-test',
    CLAUDE_CONFIG_DIR: join(directory, 'claude'),
    XDG_DATA_HOME: join(directory, 'data'),
    XDG_STATE_HOME: join(directory, 'state'),
  }, directory);
  try {
    await gui.run('convert', ['-size', '400x160', 'xc:#ff0000', picture]);
    const window = await gui.open();
    await gui.type(window, 'initial typing');
    const draft = () => Object.values(JSON.parse(readFileSync(join(gui.configuration(window), 'harness/drafts.json'), 'utf8')).drafts).join('\n');
    const expectDraft = async (expected, message) => {
      const deadline = Date.now() + 5000;
      while (draft() !== expected && Date.now() < deadline) await delay(100);
      assert.equal(draft(), expected, message);
    };
    await expectDraft('initial typing', 'typing must work without first pressing i');
    await gui.key(window, 'Escape');
    await gui.key(window, 'x');
    await expectDraft('initial typin', 'Escape must leave Vim in Normal mode');
    await gui.capture('startup-and-normal-mode');

    // Locate the synthetic bitmap rather than depending on font/layout coordinates.
    const capture = await gui.capture('inline-image');
    const pixels = await gui.run('convert', [capture, '-resize', '128x90!', '-depth', '8', 'txt:-']);
    const redPixel = pixels.stdout.split('\n').find(line => line.includes('#FF0000'));
    assert.ok(redPixel, `No inline image; inspect ${capture}`);
    const [left, top] = redPixel.split(':')[0].split(',').map(value => Number(value) * 10);
    await gui.click(window, left + 20, top + 20);
    await delay(300);
    const pixel = async (name, x, y) => {
      const path = await gui.capture(name);
      return (await gui.run('convert', [path, '-format', `%[pixel:p{${x},${y}}]`, 'info:'])).stdout.trim();
    };
    const center = await pixel('lightbox-open', 640, 450);
    assert.match(center, /255,0,0/);
    await gui.click(window, 640, 450);
    assert.equal(await pixel('picture-click', 640, 450), center, 'picture click must keep lightbox open');
    await gui.click(window, 900, 450);
    assert.notEqual(await pixel('backdrop-click', 640, 450), center, 'blank area inside the old container must dismiss');
    await gui.click(window, left + 20, top + 20);
    await delay(200);
    await gui.key(window, 'Escape');
    await delay(200);
    assert.notEqual(await pixel('escape', 640, 450), center, 'Escape must dismiss without changing the draft');
    assert.equal(draft(), 'initial typin');
    await gui.click(window, left + 20, top + 20);
    await delay(200);
    await gui.click(window, 1250, 30);
    assert.notEqual(await pixel('close-button', 640, 450), center, 'the close button must still work');

    await gui.run('convert', ['-size', '400x160', 'xc:#00ff00', picture]);
    writeFileSync(fixture, JSON.stringify({ user: { markdown: 'Two versions of the same file' }, events: [
      { type: 'tool', kind: 'image', title: 'First view', input: picture, output: picture, path: picture },
      { type: 'tool', kind: 'image', title: 'Second view', input: picture, output: picture, path: picture },
    ] }));
    await gui.open();
    const versions = await gui.capture('reopened-distinct-versions');
    const restored = (await gui.run('convert', [versions, '-resize', '128x90!', '-depth', '8', 'txt:-'])).stdout;
    assert.match(restored, /#FF0000/, 'reopened first event must retain its old red snapshot');
    assert.match(restored, /#00FF00/, 'new view event must capture the overwritten green file');
    console.log(`Isolated UI captures: ${directory}/ui`);
  } finally {
    await gui.close();
  }
});

test('local document links open as file URLs and queue disclosure stays separate from direct actions', {
  skip: !process.env.HARNESS_UI_TEST_BINARY,
  timeout: 60000,
}, async () => {
  const directory = mkdtempSync('/tmp/harness-links-queue-ui-');
  const fixture = join(directory, 'fixture.json');
  const document = join(directory, 'My sketch #1 100%.html');
  const opened = join(directory, 'opened-url.json');
  const executableDirectory = join(directory, 'bin');
  mkdirSync(executableDirectory);
  writeFileSync(document, '<p>Disposable link fixture</p>');
  writeFileSync(join(executableDirectory, 'xdg-open'), `#!/usr/bin/env node
import {writeFileSync} from 'node:fs';
writeFileSync(${JSON.stringify(opened)}, JSON.stringify(process.argv.slice(2)));
`, {mode: 0o700});
  const longPrompt = 'Investigate the context compaction progress display and keep steering, interrupting, and cancelling immediately accessible. The preview should expand without sending anything, and the full prompt should remain readable even when it is much longer than the available row.\n\nEXPANDED DETAIL CHECK';
  writeFileSync(fixture, JSON.stringify({user:{markdown:`Queue and link interaction checks · [UserSketch](<${document}>)`}, events:[
    {type:'message', markdown:`[OpenSketch](<${document}>) · [Website](https://example.test/?q=one%20two#preview) · [BadLink](relative/preview.html)`},
  ], queued_prompts:[
    {id:'queue-one',clientUserMessageId:'client-one',input:[{type:'text',text:longPrompt}]},
    {id:'queue-two',clientUserMessageId:'client-two',input:[{type:'text',text:'Keep a second prompt queued while inspecting the first.'}]},
  ]}));
  const gui = await isolatedGui(resolve(process.env.HARNESS_UI_TEST_BINARY), {
    ...process.env,
    PATH: `${executableDirectory}:${process.env.PATH}`,
    HARNESS_COMPARISON_FIXTURE: fixture,
    HARNESS_OPEN_THREAD: 'queue-ui-fixture',
    HARNESS_REPLAY_STREAMING: '1',
    HARNESS_CHATGPT_DESKTOP_VERSION: '0.0.0-test',
    CLAUDE_CONFIG_DIR: join(directory, 'claude'),
    XDG_DATA_HOME: join(directory, 'data'),
    XDG_STATE_HOME: join(directory, 'state'),
  }, directory);
  const configuration = join(directory, 'frontend-0/config/harness');
  mkdirSync(configuration, {recursive:true});
  writeFileSync(join(configuration, 'session.json'), JSON.stringify({workspace_mode:'codex',sidebar_open:false}));
  try {
    const window = await gui.open();
    const words = async name => {
      const capture = await gui.capture(name);
      const enlarged = join(directory, `${name}-ocr.png`);
      await gui.run('convert', [capture, '-resize', '200%', enlarged]);
      const output = (await gui.run('tesseract', [enlarged, 'stdout', 'tsv'])).stdout;
      return output.split('\n').slice(1).map(line => {
        const columns = line.split('\t');
        return {text:columns[11]??'',left:Number(columns[6])/2,top:Number(columns[7])/2,width:Number(columns[8])/2,height:Number(columns[9])/2};
      }).filter(word=>word.text);
    };
    const find = async (name, text) => {
      const found = (await words(name)).find(word=>word.text.toLowerCase().includes(text.toLowerCase()));
      assert.ok(found, `Missing ${text}; inspect ${directory}/ui/${name}.png`);
      return found;
    };
    const clickWord = async (name, text) => {
      const word = await find(name, text);
      await gui.click(window, word.left + word.width / 2, word.top + word.height / 2);
      await delay(200);
      return word;
    };
    const link = await clickWord('file-link', 'OpenSketch');
    const deadline = Date.now() + 4000;
    while (!existsSync(opened) && Date.now() < deadline) await delay(100);
    assert.ok(existsSync(opened), 'file link must reach the desktop opener');
    assert.deepEqual(JSON.parse(readFileSync(opened,'utf8')), [pathToFileURL(document).href]);
    await clickWord('web-link', 'Website');
    const webDeadline = Date.now() + 4000;
    while (JSON.parse(readFileSync(opened,'utf8'))[0] !== 'https://example.test/?q=one%20two#preview' && Date.now() < webDeadline) await delay(100);
    assert.deepEqual(JSON.parse(readFileSync(opened,'utf8')), ['https://example.test/?q=one%20two#preview']);
    await clickWord('user-file-link', 'UserSketch');
    const userDeadline = Date.now() + 4000;
    while (JSON.parse(readFileSync(opened,'utf8'))[0] !== pathToFileURL(document).href && Date.now() < userDeadline) await delay(100);
    assert.deepEqual(JSON.parse(readFileSync(opened,'utf8')), [pathToFileURL(document).href]);
    assert.ok(!(await words('queue-collapsed')).some(word=>word.text.includes('EXPANDED')));
    const row = await clickWord('queue-preview', 'Investigate');
    await find('queue-expanded', 'EXPANDED');
    const expandedRow = await find('queue-expanded-row', 'Investigate');
    await gui.click(window, 1262, expandedRow.top + expandedRow.height / 2);
    await delay(200);
    assert.ok(!(await words('queue-collapsed-again')).some(word=>word.text.includes('EXPANDED')));
    assert.ok(row.left <= link.left + 15, 'the preview no longer has a disclosure before it');
    await clickWord('bad-link', 'BadLink');
    const error = (await words('invalid-link-visible')).map(word=>word.text).join(' ');
    assert.match(error, /Could not open link/);
    console.log(`Isolated link and queue captures: ${directory}/ui`);
  } finally {
    await gui.close();
  }
});
