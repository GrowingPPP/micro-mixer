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
  const intervals = [];
  const classList = () => {
    const classes = new Set();
    return {
      add: name => classes.add(name),
      remove: name => classes.delete(name),
      contains: name => classes.has(name),
    };
  };
  const element = () => ({ classList: classList() });
  const buttons = [element(), element()];
  const presetButtons = [
    { dataset: { id: 'builtin:female' }, classList: classList(), textContent: '女声' },
    { dataset: { id: 'builtin:male' }, classList: classList(), textContent: '男声' },
  ];
  const fields = {
    m: messages,
    venable: { checked: false },
    vpreset: { textContent: '' },
    vbypass: { hidden: true },
  };
  const context = vm.createContext({
    document: {
      body: element(),
      getElementById: id => fields[id] || messages,
      querySelectorAll: selector => selector === '.vp' ? presetButtons : buttons,
      addEventListener: (name, fn) => listeners.set(name, fn),
    },
    setTimeout: () => 0,
    setInterval: (fn, ms) => {
      intervals.push({ fn, ms });
      return intervals.length;
    },
    fetch: request => new Promise((resolve, reject) => {
      requests.push({
        path: request,
        finish(ok = true, state = {}) {
          executed.push(request);
          resolve({ json: async () => ({ ok, error: ok ? undefined : '拒绝', ...state }) });
        },
        reject,
      });
    }),
  });
  vm.runInContext(shippedScript(), context, { filename: 'shipped-phone-controls.js' });
  buttons.forEach((button, i) => {
    button.click = () => context.hit(`/play/${i + 1}`, button);
  });
  return {
    context, buttons, presetButtons, listeners, requests, executed, messages, fields, intervals,
  };
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

test('变声启用与切预设按点击顺序提交且选预设不启用', async () => {
  const h = harness();
  const enable = h.context.voiceEnable(true);
  await flush();
  assert.deepEqual(h.requests.map(r => r.path), ['/voice/enable/1']);
  h.requests[0].finish(true, {
    enabled: true, bypassed: false, preset_id: 'builtin:female', preset_name: '女声',
  });
  await enable;
  assert.equal(h.fields.venable.checked, true);
  assert.equal(h.messages.textContent, '已启用变声');

  const pick = h.context.voicePick(h.presetButtons[1]);
  await flush();
  assert.deepEqual(h.requests.map(r => r.path),
    ['/voice/enable/1', '/voice/preset/builtin%3Amale'],
    '选择预设只提交预设命令，不能顺带启用或关闭');
  h.requests[1].finish(true, {
    enabled: true, bypassed: false, preset_id: 'builtin:male', preset_name: '男声',
  });
  await pick;
  assert.equal(h.fields.vpreset.textContent, '男声');
  assert.ok(h.presetButtons[1].classList.contains('on'));
  assert.ok(!h.presetButtons[0].classList.contains('on'));
  assert.equal(h.messages.textContent, '已切换预设');
});

test('变声快速启用切预设关闭保持点击顺序', async () => {
  const h = harness();
  const pending = [
    h.context.voiceEnable(true),
    h.context.voicePick(h.presetButtons[1]),
    h.context.voiceEnable(false),
  ];
  const paths = ['/voice/enable/1', '/voice/preset/builtin%3Amale', '/voice/enable/0'];
  for (let i = 0; i < paths.length; i++) {
    await flush();
    assert.deepEqual(h.requests.map(r => r.path), paths.slice(0, i + 1));
    h.requests[i].finish(true, {
      enabled: i < 2,
      bypassed: false,
      preset_id: i === 1 ? 'builtin:male' : 'builtin:female',
      preset_name: i === 1 ? '男声' : '女声',
    });
  }
  await Promise.all(pending);
  assert.deepEqual(h.executed, paths);
  assert.equal(h.messages.textContent, '已关闭变声');
  assert.equal(h.fields.venable.checked, false);
});

test('变声状态轮询更新开关旁路与当前预设', async () => {
  const h = harness();
  assert.equal(h.intervals.length, 1);
  assert.equal(h.intervals[0].ms, 1000);
  const poll = h.context.pollVoice();
  await flush();
  assert.deepEqual(h.requests.map(r => r.path), ['/voice/status']);
  h.requests[0].finish(true, {
    enabled: true, bypassed: true, preset_id: 'builtin:male', preset_name: '男声',
  });
  await poll;
  assert.equal(h.fields.venable.checked, true);
  assert.equal(h.fields.vbypass.hidden, false);
  assert.equal(h.fields.vpreset.textContent, '男声');
  assert.ok(h.presetButtons[1].classList.contains('on'));
});

test('点击后启动的轮询不能覆盖先完成的命令结果', async () => {
  const h = harness();
  const enable = h.context.voiceEnable(true);
  await flush();
  const poll = h.context.pollVoice();
  await flush();
  assert.deepEqual(h.requests.map(r => r.path), ['/voice/enable/1', '/voice/status']);

  h.requests[0].finish(true, {
    enabled: true, bypassed: false, preset_id: 'builtin:male', preset_name: '男声',
  });
  await enable;
  assert.equal(h.fields.venable.checked, true);
  assert.equal(h.fields.vpreset.textContent, '男声');

  h.requests[1].finish(true, {
    enabled: false, bypassed: true, preset_id: 'builtin:female', preset_name: '女声',
  });
  await poll;
  assert.equal(h.fields.venable.checked, true, '命令完成后到达的旧轮询必须丢弃');
  assert.equal(h.fields.vpreset.textContent, '男声');
  assert.equal(h.fields.vbypass.hidden, true);
});

test('退休会话的旧轮询回复不覆盖新的变声状态', async () => {
  const h = harness();
  const poll = h.context.pollVoice();
  await flush();
  const enable = h.context.voiceEnable(true);
  await flush();
  assert.deepEqual(h.requests.map(r => r.path), ['/voice/status', '/voice/enable/1']);
  h.requests[1].finish(true, {
    enabled: true, bypassed: false, preset_id: 'builtin:male', preset_name: '男声',
  });
  await enable;
  assert.equal(h.fields.venable.checked, true);
  assert.equal(h.fields.vpreset.textContent, '男声');

  h.requests[0].finish(true, {
    enabled: false, bypassed: true, preset_id: 'builtin:female', preset_name: '女声',
  });
  await poll;
  assert.equal(h.fields.venable.checked, true, '旧会话回复不能覆盖新状态');
  assert.equal(h.fields.vpreset.textContent, '男声');
  assert.equal(h.fields.vbypass.hidden, true);
});

test('变声命令失败显示原因且不改变开关', async () => {
  const h = harness();
  const pick = h.context.voicePick(h.presetButtons[0]);
  await flush();
  h.requests[0].finish(false, {
    enabled: false, bypassed: false, preset_id: 'builtin:female', preset_name: '女声',
  });
  await pick;
  assert.equal(h.messages.textContent, '失败：拒绝');
  assert.equal(h.fields.venable.checked, false);
});
