// Runs the exact JavaScript shipped in the phone control page. No network or
// audio device is used: fetch is held so command execution can be reordered.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

function shippedScript() {
  const dir = path.join(__dirname, '../crates/remote/src');
  const rust = fs.readFileSync(path.join(dir, 'lib.rs'), 'utf8');
  assert.ok(rust.includes('include_str!("control-page.js")'),
    'the tested phone controls must be embedded in the delivered page');
  return fs.readFileSync(path.join(dir, 'control-page.js'), 'utf8');
}

function harness() {
  const messages = { textContent: '' };
  const requests = [];
  const executed = [];
  const listeners = new Map();
  const element = () => ({ classList: { add() {}, remove() {} } });
  const buttons = [element(), element()];
  const context = vm.createContext({
    document: {
      body: element(),
      getElementById: () => messages,
      querySelectorAll: () => buttons,
      addEventListener: (name, fn) => listeners.set(name, fn),
    },
    setTimeout: () => 0,
    fetch: request => new Promise((resolve, reject) => {
      requests.push({
        path: request,
        finish(ok = true) {
          executed.push(request);
          resolve({ json: async () => ({ ok, error: ok ? undefined : '拒绝' }) });
        },
        reject,
      });
    }),
  });
  vm.runInContext(shippedScript(), context, { filename: 'shipped-phone-controls.js' });
  buttons.forEach((button, i) => {
    button.click = () => context.hit(`/play/${i + 1}`, button);
  });
  return { context, buttons, listeners, requests, executed, messages };
}

async function flush() {
  for (let i = 0; i < 16; i++) await Promise.resolve();
}

test('phone play then stop preserves execution order when the first fetch is slow', async () => {
  const h = harness();
  const play = h.context.hit('/play/1', h.buttons[0]);
  await flush();
  const stop = h.context.hit('/stop', h.context.document.body);
  await flush();
  assert.deepEqual(h.requests.map(r => r.path), ['/play/1'],
    'stop must not overtake an already issued play request');
  h.requests[0].finish();
  await flush();
  assert.deepEqual(h.requests.map(r => r.path), ['/play/1', '/stop']);
  h.requests[1].finish();
  await Promise.all([play, stop]);
  assert.deepEqual(h.executed, ['/play/1', '/stop']);
  assert.equal(h.messages.textContent, '已停止');
});

test('phone rapid switches stop and replay all use the same request order', async () => {
  const h = harness();
  const paths = ['/play/1', '/play/2', '/stop', '/play/1'];
  const pending = paths.map(p => h.context.hit(p, h.buttons[0]));
  for (let i = 0; i < paths.length; i++) {
    await flush();
    assert.deepEqual(h.requests.map(r => r.path), paths.slice(0, i + 1));
    h.requests[i].finish();
  }
  await Promise.all(pending);
  assert.deepEqual(h.executed, paths);
  assert.equal(h.messages.textContent, '已触发');
});

test('an older phone failure cannot replace the latest stop message or break later commands', async () => {
  const h = harness();
  const play = h.context.hit('/play/1', h.buttons[0]);
  await flush();
  const stop = h.context.hit('/stop', h.context.document.body);
  await flush();
  h.requests[0].reject(new Error('injected network failure'));
  await flush();
  assert.equal(h.messages.textContent, '', 'an older request cannot report over a newer action');
  assert.deepEqual(h.requests.map(r => r.path), ['/play/1', '/stop']);
  h.requests[1].finish();
  await Promise.all([play, stop]);
  assert.equal(h.messages.textContent, '已停止');
  const replay = h.context.hit('/play/2', h.buttons[1]);
  await flush();
  h.requests[2].finish();
  await replay;
  assert.equal(h.messages.textContent, '已触发');
});

test('phone number and space keys share the click command ordering', async () => {
  const h = harness();
  const key = h.listeners.get('keydown');
  key({ key: '2' });
  await flush();
  let prevented = false;
  key({ key: ' ', preventDefault() { prevented = true; } });
  await flush();
  assert.equal(prevented, true);
  assert.deepEqual(h.requests.map(r => r.path), ['/play/2']);
  h.requests[0].finish();
  await flush();
  assert.deepEqual(h.requests.map(r => r.path), ['/play/2', '/stop']);
  h.requests[1].finish();
  await flush();
  assert.equal(h.messages.textContent, '已停止');
});
