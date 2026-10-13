const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

// Exercise the shipped main.js through DOM events and the Tauri IPC boundary.
// Only the browser, clock and native command boundary are substituted.
const root = path.join(__dirname, '../apps/mixer-gui/dist');
const unescape = text => text.replace(/&(?:amp|lt|gt|quot);/g,
  token => ({ '&amp;': '&', '&lt;': '<', '&gt;': '>', '&quot;': '"' }[token]));

class Element {
  constructor(tagName, document) {
    this.tagName = tagName.toUpperCase();
    this.document = document;
    this.children = [];
    this.parentElement = null;
    this.attributes = {};
    this.dataset = {};
    this.style = {};
    this.listeners = new Map();
    this.className = '';
    this.value = '';
    this.checked = false;
    this.disabled = false;
    this.textContent = '';
    this.classList = {
      contains: name => this.className.split(/\s+/).includes(name),
      add: (...names) => { for (const name of names) this.classList.toggle(name, true); },
      remove: (...names) => { for (const name of names) this.classList.toggle(name, false); },
      toggle: (name, force) => {
        const names = new Set(this.className.split(/\s+/).filter(Boolean));
        const on = force ?? !names.has(name);
        if (on) names.add(name); else names.delete(name);
        this.className = [...names].join(' ');
        return on;
      },
    };
  }
  setAttribute(name, value) {
    this.attributes[name] = String(value);
    if (name === 'class') this.className = value;
    if (name === 'value') this.value = value;
    if (name === 'id' || name === 'title' || name === 'type') this[name] = value;
    if (name === 'checked') this.checked = true;
    if (name.startsWith('data-')) this.dataset[name.slice(5)] = value;
  }
  getAttribute(name) { return this.attributes[name] ?? null; }
  set value(value) { this._value = String(value ?? ''); }
  get value() { return this._value; }
  appendChild(child) { child.parentElement = this; this.children.push(child); return child; }
  get options() { return this.children.filter(child => child.tagName === 'OPTION'); }
  get selectedOptions() { return this.options.filter(option => option.value === this.value); }
  set innerHTML(html) {
    this.children = [];
    this.textContent = '';
    const stack = [this];
    for (const token of String(html).match(/<[^>]*>|[^<]+/g) || []) {
      if (/^<\//.test(token)) { if (stack.length > 1) stack.pop(); continue; }
      if (token.startsWith('<')) {
        const tag = token.match(/^<([a-z][\w-]*)/i)?.[1];
        if (!tag) continue;
        const child = new Element(tag, this.document);
        for (const match of token.matchAll(/([\w-]+)(?:="([^"]*)")?/g)) {
          if (match.index === 1) continue;
          child.setAttribute(match[1], unescape(match[2] ?? ''));
        }
        stack.at(-1).appendChild(child);
        if (!/^(input|meta|link|br|img|hr)$/i.test(tag)) stack.push(child);
      } else {
        for (const element of stack) element.textContent += unescape(token);
      }
    }
  }
  matches(selector) {
    if (selector.startsWith('.')) return this.classList.contains(selector.slice(1));
    if (selector.startsWith('#')) return this.id === selector.slice(1);
    return this.tagName.toLowerCase() === selector.toLowerCase();
  }
  querySelectorAll(selector) {
    const parts = selector.split(/\s+/);
    const result = [];
    const visit = element => {
      for (const child of element.children) {
        if (child.matches(parts.at(-1))) {
          let ancestor = child.parentElement;
          let matched = true;
          for (let i = parts.length - 2; i >= 0; i--) {
            while (ancestor && !ancestor.matches(parts[i])) ancestor = ancestor.parentElement;
            if (!ancestor) { matched = false; break; }
            ancestor = ancestor.parentElement;
          }
          if (matched) result.push(child);
        }
        visit(child);
      }
    };
    visit(this);
    return result;
  }
  querySelector(selector) { return this.querySelectorAll(selector)[0] ?? null; }
  addEventListener(type, listener) {
    if (!this.listeners.has(type)) this.listeners.set(type, []);
    this.listeners.get(type).push(listener);
  }
  async emit(type, details = {}) {
    const event = { type, target: this, stopPropagation() {}, preventDefault() {}, ...details };
    await Promise.all((this.listeners.get(type) || []).map(listener => listener(event)));
  }
  focus() { this.document.activeElement = this; }
  select() {}
  blur() { return this.emit('blur'); }
}

const items = [
  { id: 'sound-A', file: '/sounds/A.wav', name: 'Alpha', gain: 0, tags: ['a,b'] },
  { id: 'sound-B', file: '/sounds/B.wav', name: 'Beta', gain: -6, tags: [] },
];
const flush = async () => { for (let i = 0; i < 25; i++) await Promise.resolve(); };
const playbackStatus = (patch = {}) => ({
  running: true, passthrough: true, device: 'Fake output', uptime_secs: 1,
  slots: items.map((item, index) => ({ index, source_index: index, id: item.id,
    name: item.name, file: item.file, gain_db: item.gain, hotkey: index ? 'F2' : '', duration: 2 })),
  overruns: 0, underruns: 0, playing: false, active_slot: -1, playhead_secs: 0,
  preview_sound_id: null, preview_playing: false, preview_playhead_secs: 0, preview_duration_secs: 0,
  out_peak_db: -18, out_rms_db: -24, limiter_db: 0, ring_ms: 20, remote_urls: [],
  ...patch,
});

async function boot({ running = false, initialLoop, onInvoke } = {}) {
  const document = new Element('document', null);
  document.document = document;
  document.createElement = tag => new Element(tag, document);
  document.getElementById = id => document.querySelector('#' + id);
  document.innerHTML = fs.readFileSync(path.join(root, 'index.html'), 'utf8');
  if (initialLoop !== undefined) document.getElementById('inLoop').checked = initialLoop;
  const calls = [];
  const timers = new Map();
  const intervals = new Map();
  let nextTimer = 1;
  let nextOperation = 1;
  const window = {
    addEventListener() {}, innerWidth: 920, innerHeight: 660,
    __TAURI__: { core: { invoke(command, args) {
      calls.push({ command, args: args == null ? args : JSON.parse(JSON.stringify(args)) });
      if (onInvoke) {
        const result = onInvoke(command, args);
        if (result !== undefined) return Promise.resolve(result);
      }
      if (command === 'load_config') return Promise.resolve({ items, dir: '', hotkeys: ',F2' });
      if (command === 'list_devices') return Promise.resolve([
        { id: 'fake-output', name: 'Fake output', direction: '输出', is_virtual: true,
          is_default: true, sample_rate: 48000, channels: 2 },
      ]);
      if (command === 'guess_output') return Promise.resolve('fake-output');
      if (command === 'status') return Promise.resolve({
        running, passthrough: running, device: 'Fake output', uptime_secs: 1,
        slots: running ? items.map((item, index) => ({ index, source_index: index, id: item.id,
          name: item.name, file: item.file, gain_db: item.gain, hotkey: index ? 'F2' : '', duration: 2 })) : [],
        overruns: 0, underruns: 0, playing: false, out_peak_db: -18, out_rms_db: -24,
        limiter_db: 0, ring_ms: 20, remote_urls: [],
      });
      if (command === 'autostart') return Promise.resolve(false);
      if (command === 'begin_playback_operation') return Promise.resolve(nextOperation++);
      return Promise.resolve(true);
    } } },
  };
  const context = vm.createContext({ window, document, console,
    navigator: { clipboard: { writeText: async () => {} } },
    setTimeout(callback, delay) { const id = nextTimer++; timers.set(id, { callback, delay }); return id; },
    clearTimeout(id) { timers.delete(id); },
    setInterval(callback, delay) { const id = nextTimer++; intervals.set(id, { callback, delay }); return id; },
  });
  vm.runInContext(fs.readFileSync(path.join(root, 'library-model.js'), 'utf8'), context);
  vm.runInContext(fs.readFileSync(path.join(root, 'main.js'), 'utf8'), context);
  await flush();
  assert.equal(document.querySelectorAll('.card').length, 2,
    'the shipped script must initialize and render the loaded library');
  assert.deepEqual(calls.filter(call => call.command === 'js_error'), [], 'frontend must boot without errors');
  return {
    document, calls,
    card(index) { return document.querySelectorAll('.card')[index]; },
    async pollStatus() {
      for (const timer of intervals.values()) timer.callback();
      await flush();
    },
    async advanceTimers(milliseconds) {
      for (const [id, timer] of [...timers]) {
        if (timer.delay > milliseconds || !timers.has(id)) continue;
        timers.delete(id); timer.callback();
      }
      await flush();
    },
  };
}

test('全局音量控件说明人声与音效用途并关联可访问说明', async () => {
  const app = await boot();
  const mic = app.document.getElementById('inMicGain');
  const sfx = app.document.getElementById('inSfxGain');
  assert.match(mic.parentElement.parentElement.textContent, /麦克风音量/);
  assert.match(sfx.parentElement.parentElement.textContent, /音效总音量/);
  assert.equal(mic.getAttribute('aria-label'), '麦克风音量');
  assert.equal(sfx.getAttribute('aria-label'), '音效总音量');
  const micHelp = app.document.getElementById(mic.getAttribute('aria-describedby').split(' ')[0]);
  const sfxHelp = app.document.getElementById(sfx.getAttribute('aria-describedby').split(' ')[0]);
  assert.match(micHelp?.textContent ?? '', /对方.*人声/);
  assert.match(sfxHelp?.textContent ?? '', /试听、正式播放和监听/);
  assert.match(sfxHelp?.textContent ?? '', /卡片.*叠加/);
});

test('保存的总音量零值在首次试听前同步到后端', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'load_config') return { items, dir: '', mic_gain_db: -3, sfx_gain_db: 0 };
  } });
  assert.deepEqual(app.calls.filter(c => c.command === 'set_gains'), [
    { command: 'set_gains', args: { micDb: -3, sfxDb: 0 } },
  ]);
  await app.card(0).querySelector('.prev').emit('click');
  assert.equal(app.calls.findIndex(c => c.command === 'set_gains') <
    app.calls.findIndex(c => c.command === 'preview'), true);
  assert.equal(app.calls.some(c => c.command === 'save_config'), false, 'restoring must not rewrite config');
});

test('全局音量输入和旧配置限制在有限范围内且保存显示值一致', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'load_config') return { items, dir: '', mic_gain_db: -120, sfx_gain_db: 760 };
  } });
  const mic = app.document.getElementById('inMicGain');
  const total = app.document.getElementById('inSfxGain');
  assert.equal(mic.value, '-60');
  assert.equal(total.value, '36');
  for (const control of [mic, total]) {
    assert.equal(control.getAttribute('min'), '-60');
    assert.equal(control.getAttribute('max'), '36');
  }
  assert.deepEqual(app.calls.filter(c => c.command === 'set_gains').at(-1).args,
    { micDb: -60, sfxDb: 36 });
  total.value = '760';
  await total.emit('input');
  assert.equal(total.value, '36');
  await app.advanceTimers(120);
  assert.equal(app.calls.filter(c => c.command === 'save_config').at(-1).args.sfxGainDb, 36);
  assert.equal(app.calls.some(c => c.command === 'set_gains' && c.args.sfxDb > 36), false);
});

test('旧配置卡片增益恢复到滑杆范围并统一试听启动与保存', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'load_config') return { items: items.map((item, index) =>
      ({ ...item, gain: index ? -120 : 760 })), dir: '' };
  } });
  assert.equal(app.card(0).querySelector('.vol').value, '12');
  assert.equal(app.card(0).querySelector('.volume-value').textContent, '+12 dB');
  assert.equal(app.card(1).querySelector('.vol').value, '-40');
  await app.card(0).querySelector('.prev').emit('click');
  assert.equal(app.calls.filter(c => c.command === 'preview').at(-1).args.gainDb, 12);
  await app.document.getElementById('btnToggle').emit('click');
  assert.deepEqual(app.calls.filter(c => c.command === 'start').at(-1).args.cfg.gains, [12, -40]);
  assert.deepEqual(app.calls.filter(c => c.command === 'save_config').at(-1).args.items.map(item => item.gain), [12, -40]);
});

test('未启动时调节总音量保存零值并立即控制下一次试听', async () => {
  const app = await boot();
  const total = app.document.getElementById('inSfxGain');
  const mic = app.document.getElementById('inMicGain');
  total.value = '0';
  mic.value = '-3';
  await total.emit('input');
  await app.advanceTimers(120);
  assert.deepEqual(app.calls.filter(c => c.command === 'set_gains').at(-1)?.args,
    { micDb: -3, sfxDb: 0 });
  assert.equal(app.calls.filter(c => c.command === 'save_config').at(-1)?.args.sfxGainDb, 0);
  await app.card(1).querySelector('.prev').emit('click');
  assert.equal(app.calls.filter(c => c.command === 'preview').at(-1).args.gainDb, -6,
    'the preview command keeps the card gain; the Host applies the shared total gain');
  assert.equal(app.calls.some(c => ['start', 'play', 'activate'].includes(c.command)), false);
});

test('输入总音量后立即试听会先提交尚未到期的音量调整', async () => {
  let totalAtPreview;
  let nativeTotal;
  const app = await boot({ onInvoke(command, args) {
    if (command === 'set_gains') { nativeTotal = args.sfxDb; return true; }
    if (command === 'preview') { totalAtPreview = nativeTotal; return true; }
  } });
  const total = app.document.getElementById('inSfxGain');
  total.value = '-12';
  await total.emit('input');
  await app.card(0).querySelector('.prev').emit('click');
  assert.equal(totalAtPreview, -12, 'preview must not start with the previous total gain');
  assert.equal(app.calls.filter(c => c.command === 'save_config').at(-1)?.args.sfxGainDb, -12);
  await app.advanceTimers(120);
  assert.equal(app.calls.filter(c => c.command === 'set_gains' && c.args.sfxDb === -12).length, 1,
    'flushing must cancel the pending timer instead of sending a duplicate update');
});

test('输入总音量后立即正式播放也使用最新设置并同步监听', async () => {
  let nativeTotal;
  let totalAtPlay;
  const app = await boot({ running: true, onInvoke(command, args) {
    if (command === 'set_gains') { nativeTotal = args.sfxDb; return true; }
    if (command === 'play') { totalAtPlay = nativeTotal; return true; }
  } });
  const total = app.document.getElementById('inSfxGain');
  total.value = '-12';
  await total.emit('input');
  await app.card(0).emit('click');
  assert.equal(totalAtPlay, -12, 'formal playback must flush the same total gain as preview');
  assert.equal(app.calls.filter(c => c.command === 'save_config').at(-1)?.args.sfxGainDb, -12);
});

for (const action of ['preview', 'play']) {
  test(`等待总音量请求期间停止不会让迟到的${action}重新出声`, async () => {
    let finishGain;
    const app = await boot({ running: true, onInvoke(command, args) {
      if (command === 'set_gains' && args.sfxDb === -12) {
        return new Promise(resolve => { finishGain = resolve; });
      }
    } });
    const total = app.document.getElementById('inSfxGain');
    total.value = '-12';
    await total.emit('input');
    const card = app.card(0);
    const choosing = (action === 'preview' ? card.querySelector('.prev') : card).emit('click');
    await flush();
    assert.equal(typeof finishGain, 'function');
    await app.document.getElementById('btnStopAll').emit('click');
    finishGain(true);
    await choosing;
    assert.equal(app.calls.filter(c => c.command === action).length, 0,
      'a stopped choice cannot dispatch after gain synchronization finishes');
    assert.equal(app.calls.filter(c => c.command === 'stop_all').length, 1);
  });
}

test('慢音量请求按输入顺序完成并保存最后一次设置', async () => {
  let finishFirst;
  const app = await boot({ running: true, onInvoke(command, args) {
    if (command === 'set_gains' && args.sfxDb === -12) {
      return new Promise(resolve => { finishFirst = resolve; });
    }
  } });
  const total = app.document.getElementById('inSfxGain');
  total.value = '-12';
  await total.emit('input');
  await app.advanceTimers(120);
  total.value = '6';
  await total.emit('input');
  await app.advanceTimers(120);
  assert.equal(app.calls.some(c => c.command === 'set_gains' && c.args.sfxDb === 6), false);
  finishFirst(true);
  await flush();
  assert.deepEqual(app.calls.filter(c => c.command === 'set_gains').map(c => c.args.sfxDb), [-6, -12, 6]);
  assert.deepEqual(app.calls.filter(c => c.command === 'save_config').map(c => c.args.sfxGainDb), [-12, 6]);
  assert.equal(total.value, '6');
});

test('启动期间修改总音量后最终会话使用界面的最新音量', async () => {
  let finishStart;
  let nativeTotal = -6;
  let running = false;
  const app = await boot({ onInvoke(command, args) {
    if (command === 'set_gains') { nativeTotal = args.sfxDb; return true; }
    if (command === 'status') return playbackStatus({ running, passthrough: running });
    if (command === 'start') return new Promise(resolve => {
      finishStart = () => { nativeTotal = args.cfg.sfx_gain_db; running = true; resolve(true); };
    });
  } });
  const starting = app.document.getElementById('btnToggle').emit('click');
  await flush();
  assert.equal(typeof finishStart, 'function');
  const total = app.document.getElementById('inSfxGain');
  total.value = '4';
  await total.emit('input');
  await app.advanceTimers(120);
  finishStart();
  await starting;
  assert.equal(nativeTotal, 4, 'the completed start must not restore its older gain snapshot');
  assert.equal(app.calls.filter(c => c.command === 'save_config').at(-1).args.sfxGainDb, 4);
  assert.equal(app.calls.filter(c => c.command === 'start').length, 1, 'gain edits do not restart streams');
});

test('About explains playback, preview and arbitrary audio folders with the packaged name and version', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'app_info') return { name: '麦克风混音助手', version: '0.1.0' };
  } });
  const button = app.document.getElementById('btnAbout');
  const overlay = app.document.getElementById('aboutOverlay');
  button.focus();
  await button.emit('click');
  assert.equal(overlay.classList.contains('hidden'), false);
  assert.equal(app.document.activeElement, app.document.getElementById('btnAboutClose'));
  assert.equal(app.document.getElementById('aboutTitle').textContent, '麦克风混音助手');
  assert.equal(app.document.getElementById('aboutVersion').textContent, '版本 0.1.0 · 支持 macOS 和 Windows');
  assert.match(overlay.textContent, /将本地音频与麦克风人声混合/);
  assert.match(overlay.textContent, /试听.*仅自己可听/s);
  assert.match(overlay.textContent, /任意有读取权限的文件夹/);
  assert.match(overlay.textContent, /本软件输出选 CABLE Input，目标应用麦克风选 CABLE Output/,
    'Windows must choose the capture endpoint, whose name differs from the output endpoint');
  assert.doesNotMatch(overlay.textContent, /队友/);
  await overlay.emit('click', { target: app.document.getElementById('aboutSummary') });
  assert.equal(overlay.classList.contains('hidden'), false, 'selecting the description must not dismiss About');
  await app.document.getElementById('btnAboutClose').emit('click');
  assert.equal(overlay.classList.contains('hidden'), true);
  assert.equal(app.document.activeElement, button, 'closing must restore the keyboard focus');
});

for (const running of [false, true]) {
  test(`About keyboard navigation cannot play or stop audio with running=${running}`, async () => {
    const app = await boot({ running });
    const button = app.document.getElementById('btnAbout');
    const close = app.document.getElementById('btnAboutClose');
    const overlay = app.document.getElementById('aboutOverlay');
    const controls = () => app.calls.filter(call =>
      ['begin_playback_operation', 'activate', 'play', 'preview', 'stop_all', 'stop_preview', 'stop'].includes(call.command));
    button.focus();
    await button.emit('click');
    for (const key of ['1', '2', ' ', 'Tab']) await app.document.emit('keydown', { key, target: close });
    await app.document.emit('keydown', { key: 'Tab', shiftKey: true, target: close });
    assert.equal(app.document.activeElement, close, 'Tab and Shift+Tab remain in the modal');
    assert.deepEqual(controls(), [], 'reading About must preserve the playing or idle session');
    const qrPanel = app.document.getElementById('qrPanel');
    qrPanel.classList.remove('hidden');
    await app.document.emit('keydown', { key: 'Escape', target: close });
    assert.equal(overlay.classList.contains('hidden'), true);
    assert.equal(qrPanel.classList.contains('hidden'), false, 'Escape closes only the active modal');
    assert.equal(app.document.activeElement, button);
    assert.deepEqual(controls(), []);
    await app.document.emit('keydown', { key: ' ', target: app.document });
    await flush();
    assert.equal(controls().some(call => call.command === 'stop_all'), true, 'normal shortcuts resume after closing About');
  });
}

test('panic diagnostics do not direct Windows users to a macOS-only log path', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'status') return playbackStatus({ last_panic: 'fixture panic' });
  } });
  const message = app.document.getElementById('setupHint').textContent;
  assert.match(message, /fixture panic.*诊断日志/);
  assert.doesNotMatch(message, /Library\/Logs|~\//);
});

test('About remains readable and can close when version lookup fails or completes after dismissal', async () => {
  let resolveInfo;
  const app = await boot({ onInvoke(command) {
    if (command === 'app_info') return new Promise(resolve => { resolveInfo = resolve; });
  } });
  const opening = app.document.getElementById('btnAbout').emit('click');
  await flush();
  const overlay = app.document.getElementById('aboutOverlay');
  assert.equal(overlay.classList.contains('hidden'), false);
  await overlay.emit('click');
  resolveInfo({ name: '麦克风混音助手', version: '0.1.0' });
  await opening;
  assert.equal(overlay.classList.contains('hidden'), true, 'late metadata must not reopen the dialog');
  const failed = await boot({ onInvoke(command) {
    if (command === 'app_info') return Promise.reject(new Error('unavailable'));
  } });
  await failed.document.getElementById('btnAbout').emit('click');
  assert.match(failed.document.getElementById('aboutSummary').textContent, /虚拟音频设备/);
  assert.equal(failed.document.getElementById('aboutVersion').classList.contains('hidden'), true);
  await failed.document.getElementById('btnAboutClose').emit('click');
  assert.equal(failed.document.getElementById('aboutOverlay').classList.contains('hidden'), true);
  assert.deepEqual(failed.calls.filter(call => call.command === 'js_error'), []);
});

test('looping changes while a sound is playing update native audio immediately and save both values', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'load_config') return { items, dir: '', hotkeys: ',F2', looping: true };
    if (command === 'status') return playbackStatus({ playing: true, active_slot: 0, playhead_secs: 1 });
  } });
  const loop = app.document.getElementById('inLoop');
  // Start from the visible checked state to isolate the missing live change path.
  loop.checked = true;
  loop.checked = false;
  await loop.emit('change');
  await flush();
  assert.deepEqual(app.calls.filter(call => call.command === 'set_looping'), [
    { command: 'set_looping', args: { looping: false } },
  ], 'unchecking loop must immediately update the currently playing engine');
  assert.equal(app.calls.filter(call => call.command === 'save_config').at(-1)?.args.looping, false);
  loop.checked = true;
  await loop.emit('change');
  await flush();
  assert.deepEqual(app.calls.filter(call => call.command === 'set_looping').map(call => call.args.looping), [false, true]);
  assert.deepEqual(app.calls.filter(call => call.command === 'save_config').map(call => call.args.looping), [false, true]);
  assert.equal(app.calls.some(call => ['play', 'stop_all', 'stop'].includes(call.command)), false,
    'changing loop must preserve the current pass instead of stopping or restarting it');
});

for (const looping of [false, true]) {
  test(`looping=${looping} restores from config and remains present in later saved settings`, async () => {
    const app = await boot({ initialLoop: !looping, onInvoke(command) {
      if (command === 'load_config') return { items, dir: '', hotkeys: ',F2', looping };
    } });
    assert.equal(app.document.getElementById('inLoop').checked, looping,
      'both false and true must be applied as stored settings');
    await app.card(0).querySelector('.vol').emit('change');
    assert.equal(app.calls.filter(call => call.command === 'save_config').at(-1)?.args.looping, looping,
      'saving another control must not discard the loop preference');
  });
}

test('looping changes before startup save the preference without starting, playing or stopping audio', async () => {
  const app = await boot();
  const loop = app.document.getElementById('inLoop');
  loop.checked = true;
  await loop.emit('change');
  await flush();
  assert.equal(app.calls.filter(call => call.command === 'save_config').at(-1)?.args.looping, true,
    'an idle loop edit must persist without requiring engine startup');
  loop.checked = false;
  await loop.emit('change');
  await flush();
  assert.deepEqual(app.calls.filter(call => call.command === 'save_config').map(call => call.args.looping), [true, false]);
  assert.equal(app.calls.some(call => ['start', 'activate', 'play', 'preview', 'stop', 'stop_all'].includes(call.command)), false);
  assert.equal(app.calls.some(call => call.command === 'set_looping'), false,
    'a released engine has no live loop state to update');
  await app.pollStatus();
  assert.equal(loop.checked, false);
});

test('looping updates a standby passthrough session without activating or stopping it', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'load_config') return { items, dir: '', hotkeys: ',F2', looping: true };
    if (command === 'status') return playbackStatus({ running: false, passthrough: true, looping: true });
  } });
  const loop = app.document.getElementById('inLoop');
  loop.checked = false;
  await loop.emit('change');
  assert.deepEqual(app.calls.filter(call => call.command === 'set_looping'), [
    { command: 'set_looping', args: { looping: false } },
  ]);
  assert.equal(app.calls.filter(call => call.command === 'save_config').at(-1).args.looping, false);
  await app.pollStatus();
  assert.equal(loop.checked, false, 'an older running or standby snapshot must not undo the selection');
  assert.equal(app.calls.some(call => ['start', 'activate', 'play', 'stop', 'stop_all'].includes(call.command)), false);
});

test('slow looping updates preserve fast false then true selection order without stale checkbox rollback', async () => {
  let finishFirst;
  let first = true;
  const app = await boot({ onInvoke(command) {
    if (command === 'load_config') return { items, dir: '', hotkeys: ',F2', looping: true };
    if (command === 'status') return playbackStatus({ playing: true, active_slot: 0, looping: false });
    if (command === 'set_looping' && first) {
      first = false;
      return new Promise(resolve => { finishFirst = resolve; });
    }
  } });
  const loop = app.document.getElementById('inLoop');
  loop.checked = false;
  const firstChange = loop.emit('change');
  await flush();
  loop.checked = true;
  const secondChange = loop.emit('change');
  await flush();
  assert.deepEqual(app.calls.filter(call => call.command === 'set_looping').map(call => call.args.looping), [false]);
  await app.pollStatus();
  assert.equal(loop.checked, true, 'a stale status must not override a newer pending choice');
  finishFirst(true);
  await Promise.all([firstChange, secondChange]);
  assert.deepEqual(app.calls.filter(call => call.command === 'set_looping').map(call => call.args.looping), [false, true]);
  assert.deepEqual(app.calls.filter(call => call.command === 'save_config').map(call => call.args.looping), [false, true]);
  assert.equal(loop.checked, true);
  assert.match(app.document.getElementById('loopHelp').textContent, /循环已开启/);
  assert.equal(app.calls.some(call => ['play', 'stop', 'stop_all'].includes(call.command)), false);
});

test('looping before startup stays selected across stopped polls and never alters an active one-shot preview', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'status') return playbackStatus({ running: false, passthrough: false, slots: [], looping: false,
      preview_playing: true, preview_sound_id: 'sound-A', preview_playhead_secs: 0.5, preview_duration_secs: 2 });
  } });
  const loop = app.document.getElementById('inLoop');
  loop.checked = true;
  await loop.emit('change');
  await app.pollStatus();
  assert.equal(loop.checked, true, 'stopped status.looping=false must not discard a saved preference');
  assert.equal(app.card(0).classList.contains('previewing'), true);
  assert.equal(app.card(0).querySelector('.bar').style.width, '25%');
  assert.equal(app.calls.filter(call => call.command === 'save_config').at(-1).args.looping, true);
  assert.equal(app.calls.some(call => ['set_looping', 'start', 'play', 'preview', 'stop', 'stop_all'].includes(call.command)), false);
});

test('looping save after release cannot be overwritten by an earlier slow native change', async () => {
  let status = playbackStatus({ looping: true });
  let savedLoop = true;
  let finishNative;
  const app = await boot({ onInvoke(command, args) {
    if (command === 'load_config') return { items, dir: '', hotkeys: ',F2', looping: true };
    if (command === 'status') return status;
    if (command === 'set_looping') return new Promise(resolve => { finishNative = resolve; })
      .then(() => { savedLoop = args.looping; return false; });
    if (command === 'save_config') { savedLoop = args.looping; return true; }
  } });
  const loop = app.document.getElementById('inLoop');
  loop.checked = false;
  const earlier = loop.emit('change');
  await flush();
  status = playbackStatus({ running: false, passthrough: false, slots: [], looping: false });
  await app.pollStatus();
  loop.checked = true;
  const latest = loop.emit('change');
  await flush();
  finishNative(false);
  await Promise.all([earlier, latest]);
  assert.equal(savedLoop, true, 'the final stored preference must match the latest idle selection');
  assert.deepEqual(app.calls.filter(call => call.command === 'set_looping').map(call => call.args.looping), [false]);
  assert.deepEqual(app.calls.filter(call => call.command === 'save_config').map(call => call.args.looping), [false, true]);
  assert.equal(loop.checked, true);
});

test('looping control explains cancellation at the current pass end and the one-shot preview scope', async () => {
  const app = await boot();
  const loop = app.document.getElementById('inLoop');
  assert.match(loop.parentElement.textContent, /循环播放/);
  assert.match(loop.parentElement.title, /取消后当前一遍结束即停止/);
  assert.equal(loop.getAttribute('aria-label'), '循环播放');
  assert.equal(loop.getAttribute('aria-describedby'), 'loopHelp');
  assert.match(app.document.getElementById('loopHelp').textContent, /正式播放.*监听.*试听.*一次/);
  loop.checked = true;
  await loop.emit('change');
  assert.match(app.document.getElementById('loopHelp').textContent, /循环已开启.*取消后当前一遍结束即停止.*试听.*一次/);
  loop.checked = false;
  await loop.emit('change');
  assert.match(app.document.getElementById('loopHelp').textContent, /循环已关闭.*当前一遍结束.*试听.*一次/);
});

test('a loop edit during slow startup reaches the new session before startup completes', async () => {
  let status = playbackStatus({ running: false, passthrough: false, slots: [] });
  let finishStart;
  let startedLoop;
  const app = await boot({ onInvoke(command, args) {
    if (command === 'load_config') return { items, dir: '', hotkeys: ',F2', looping: true };
    if (command === 'status') return status;
    if (command === 'start') {
      startedLoop = args.cfg.looping;
      return new Promise(resolve => { finishStart = () => {
        status = playbackStatus({ looping: startedLoop }); resolve(true);
      }; });
    }
    if (command === 'set_looping') {
      status = playbackStatus({ looping: args.looping }); return true;
    }
  } });
  const starting = app.document.getElementById('btnToggle').emit('click');
  await flush();
  assert.equal(startedLoop, true);
  const loop = app.document.getElementById('inLoop');
  loop.checked = false;
  await loop.emit('change');
  finishStart();
  await starting;
  await flush();
  assert.equal(status.looping, false,
    'the startup snapshot must not leave the engine looping after the user unchecked during decode');
  assert.equal(loop.checked, false);
});

test('a failed startup loop reconciliation reports that setting error while preserving the opened native session',async()=>{
  let status=playbackStatus({running:false,passthrough:false,slots:[]});
  let finishStart;
  const app=await boot({onInvoke(command){
    if(command==='load_config')return{items,dir:'',hotkeys:',F2',looping:true};
    if(command==='status')return status;
    if(command==='start')return new Promise(resolve=>{finishStart=()=>{
      status=playbackStatus({looping:true});resolve(true);
    };});
    if(command==='set_looping')return Promise.reject(new Error('loop update transport failed'));
  }});
  const starting=app.document.getElementById('btnToggle').emit('click');
  await flush();
  const loop=app.document.getElementById('inLoop');
  loop.checked=false;
  await loop.emit('change');
  finishStart();
  await starting;
  assert.equal(app.document.getElementById('btnToggle').textContent,'关闭音效',
    'an opened audio session must not be declared off because only its subsequent loop update failed');
  assert.match(app.document.getElementById('setupHint').textContent,/循环.*失败/);
  assert.doesNotMatch(app.document.getElementById('setupHint').textContent,/启动失败/);
});

test('startup rebuilds cards from loaded stable slots after an earlier bad file was skipped', async () => {
  let status = playbackStatus({ running: false, passthrough: false, slots: [] });
  const app = await boot({ onInvoke(command, args) {
    if (command === 'status') return status;
    if (command === 'start') {
      status = playbackStatus({ slots: [{index:0,source_index:1,id:'sound-B',name:'Beta',
        file:'/sounds/B.wav',gain_db:-6,hotkey:'F2',duration:2}],skipped:['A.wav'] });
      return true;
    }
    if (command === 'play') {
      if (args.slot !== 0) return false;
      status = { ...status, playing:true,active_slot:0,playhead_secs:0.5 }; return true;
    }
  } });
  assert.equal(app.document.querySelectorAll('.card').length, 2);
  await app.document.getElementById('btnToggle').emit('click');
  assert.equal(app.document.querySelectorAll('.card').length, 1,
    'startup must replace the idle source-index cards with the actual loaded slot list');
  assert.equal(app.card(0).querySelector('.name').textContent, 'Beta');
  assert.equal(app.card(0).querySelector('.dur').textContent, '2.0s');
  assert.equal(app.card(0).querySelector('.key').textContent, 'F2');
  await app.card(0).emit('click');
  assert.equal(app.calls.filter(call=>call.command==='play').at(-1).args.slot, 0);
  assert.equal(app.card(0).classList.contains('playing'), true);
  assert.equal(app.card(0).querySelector('.bar').style.width, '25%');
});

test('a slow native startup cancelled by another controller cannot force false running feedback or loop reconciliation', async () => {
  let finishStart;
  let status = playbackStatus({running:false,passthrough:false,slots:[]});
  const app = await boot({ onInvoke(command) {
    if (command === 'status') return status;
    if (command === 'start') return new Promise(resolve => { finishStart = resolve; });
  } });
  const starting = app.document.getElementById('btnToggle').emit('click');
  await flush();
  const loop = app.document.getElementById('inLoop');
  loop.checked = true;
  await loop.emit('change');
  finishStart(false);
  await starting;
  assert.equal(app.calls.some(call=>call.command==='set_looping'),false,
    'a rejected startup has no new session to reconcile');
  assert.equal(app.document.getElementById('btnToggle').textContent,'启动');
  assert.equal(app.document.getElementById('btnToggle').disabled,false);
  assert.doesNotMatch(app.document.getElementById('toast').textContent,/已启动/);
});

for (const command of ['preview','stop','release','stop_all']) {
  test(`native cancellation of ${command} follows the new external playback without announcing old success`, async () => {
    let status=playbackStatus({playing:true,active_slot:0,playhead_secs:0.5});
    const app=await boot({onInvoke(cmd){
      if(cmd==='status')return status;
      if(cmd===command){
        status=playbackStatus({playing:true,active_slot:1,playhead_secs:0.5});
        return false;
      }
    }});
    await (command==='preview'?app.card(0).querySelector('.prev'):
      app.document.getElementById({stop:'btnToggle',release:'btnRelease',stop_all:'btnStopAll'}[command])).emit('click');
    assert.equal(app.document.getElementById('btnToggle').textContent,'关闭音效',
      'a cancelled standby/release must not force the new external playback session off');
    assert.equal(app.card(0).classList.contains('playing'),false);
    assert.equal(app.card(1).classList.contains('playing'),true);
    assert.equal(app.card(1).classList.contains('previewing'),false);
    assert.equal(app.card(1).querySelector('.bar').style.width,'25%');
    assert.doesNotMatch(app.document.getElementById('toast').textContent,/已停止|已关闭|已彻底释放|试听中/);
  });
}

for (const command of ['stop','release']) {
  test(`successful ${command} follows a newer external B playback observed by its refresh`, async()=>{
    let status=playbackStatus({playing:true,active_slot:0,playhead_secs:0.5});
    const app=await boot({onInvoke(cmd){
      if(cmd==='status')return status;
      if(cmd===command){
        // The native command succeeded, then another controller started B
        // before the refresh captured the authoritative status.
        status=playbackStatus({playing:true,active_slot:1,playhead_secs:0.5});return true;
      }
    }});
    await app.document.getElementById(command==='stop'?'btnToggle':'btnRelease').emit('click');
    assert.equal(app.document.getElementById('btnToggle').textContent,'关闭音效',
      'a confirmed older command must not overwrite the newer state returned by status');
    assert.equal(app.card(0).classList.contains('playing'),false);
    assert.equal(app.card(1).classList.contains('playing'),true);
    assert.equal(app.card(1).querySelector('.bar').style.width,'25%');
    assert.doesNotMatch(app.document.getElementById('toast').textContent,/已关闭|已彻底释放/);
  });
}

test('successful startup follows a newer standby status instead of announcing stale running success',async()=>{
  let status=playbackStatus({running:false,passthrough:false,slots:[]});
  const app=await boot({onInvoke(command){
    if(command==='status')return status;
    if(command==='start'){
      status=playbackStatus({running:false,passthrough:true,slots:[]});return true;
    }
  }});
  await app.document.getElementById('btnToggle').emit('click');
  assert.equal(app.document.getElementById('btnToggle').textContent,'开启音效');
  assert.doesNotMatch(app.document.getElementById('toast').textContent,/已启动/,
    'the native status after startup must take priority over its earlier successful reply');
  assert.equal(app.document.getElementById('setup').classList.contains('hidden'),false);
});

test('a slow looping update cannot replace a newer stop confirmation', async () => {
  let finishLoop;
  const app = await boot({running:true,onInvoke(command){
    if(command==='set_looping')return new Promise(resolve=>{finishLoop=resolve;});
  }});
  const loop=app.document.getElementById('inLoop');
  loop.checked=true;
  const updating=loop.emit('change');
  await flush();
  await app.document.getElementById('btnStopAll').emit('click');
  const stoppedToast=app.document.getElementById('toast').textContent;
  finishLoop(true);
  await updating;
  assert.equal(app.document.getElementById('toast').textContent,stoppedToast,
    'a completed earlier preference save must not replace the latest playback confirmation');
});

for (const command of ['play', 'preview']) {
  test(`a late ${command} completion cannot replace a newer stop confirmation`, async () => {
    let finishOlder;
    let status = playbackStatus();
    const app = await boot({ onInvoke(cmd) {
      if (cmd === 'status') return status;
      if (cmd === command) return new Promise(resolve => { finishOlder = resolve; });
      if (cmd === 'stop_all') { status = playbackStatus(); return true; }
    } });
    const older = command === 'play'
      ? app.card(0).emit('click') : app.card(0).querySelector('.prev').emit('click');
    await flush();
    await app.document.getElementById('btnStopAll').emit('click');
    const stoppedToast = app.document.getElementById('toast').textContent;
    assert.match(stoppedToast, /已停止/);
    finishOlder(true);
    await older;
    await flush();
    assert.equal(app.document.getElementById('toast').textContent, stoppedToast,
      'an older successful IPC reply must not tell the user that stopped audio is playing');
    assert.equal(app.document.querySelectorAll('.card').some(card => card.classList.contains('playing')), false);
  });
}

test('a failed older play cannot override the newer successful switch feedback', async () => {
  let rejectOlder;
  let status = playbackStatus();
  const app = await boot({ onInvoke(command, args) {
    if (command === 'status') return status;
    if (command === 'play' && args.slot === 0) return new Promise((resolve, reject) => { rejectOlder = reject; });
    if (command === 'play') {
      status = playbackStatus({ playing: true, active_slot: args.slot, playhead_secs: 0.5 }); return true;
    }
  } });
  const older = app.card(0).emit('click');
  await flush();
  await app.card(1).emit('click');
  const switchedToast = app.document.getElementById('toast').textContent;
  rejectOlder(new Error('old A decode failed'));
  await older;
  await flush();
  assert.equal(app.document.getElementById('toast').textContent, switchedToast,
    'an older failure should not report that the latest successful user choice failed');
  assert.equal(app.card(1).classList.contains('playing'), true);
});

test('short operation reservations stay ordered while an obsolete play is skipped and stop is admitted', async () => {
  let finishOlderReservation;
  let nextOperation = 70;
  const app = await boot({ running: true, onInvoke(command) {
    if (command === 'begin_playback_operation') {
      const operation = nextOperation++;
      if (operation === 70) return new Promise(resolve => { finishOlderReservation = () => resolve(operation); });
      return operation;
    }
  } });
  const playing = app.card(0).emit('click');
  await flush();
  const stopping = app.document.getElementById('btnStopAll').emit('click');
  await flush();
  assert.equal(app.calls.filter(call => call.command === 'begin_playback_operation').length, 1,
    'only short host reservations are serialized, so an earlier reply cannot obtain a later ticket');
  assert.equal(typeof finishOlderReservation, 'function');
  finishOlderReservation();
  await Promise.all([playing, stopping]);
  assert.equal(app.calls.some(call => call.command === 'play'), false,
    'a superseded user play must not be dispatched after the newer stop request');
  assert.deepEqual(app.calls.filter(call => call.command === 'stop_all').map(call => call.args.operation), [71]);
});

test('stop during pre-start config saving cancels startup without leaving its toggle disabled', async () => {
  let finishSave;
  const app = await boot({ onInvoke(command) {
    if (command === 'save_config') return new Promise(resolve => { finishSave = resolve; });
  } });
  const starting = app.document.getElementById('btnToggle').emit('click');
  await flush();
  assert.equal(app.document.getElementById('btnToggle').disabled, true);
  await app.document.getElementById('btnStopAll').emit('click');
  assert.equal(app.calls.filter(call => call.command === 'begin_playback_operation').length, 1,
    'stop is reserved immediately instead of waiting for the older disk save');
  assert.equal(app.calls.filter(call => call.command === 'stop_all').length, 1);
  finishSave(true);
  await starting;
  assert.equal(app.calls.some(call => call.command === 'start'), false,
    'the cancelled startup must not open a session after the latest stop');
  assert.equal(app.document.getElementById('btnToggle').disabled, false,
    'a skipped startup must release the UI busy state so the user can start later');
});

test('a slow startup does not delay stop admission and the latest stop feedback survives its late response', async () => {
  let finishStart;
  let status = playbackStatus({ running: false, passthrough: false, slots: [] });
  const app = await boot({ onInvoke(command) {
    if (command === 'status') return status;
    if (command === 'start') return new Promise(resolve => { finishStart = resolve; });
    if (command === 'stop_all') return true;
  } });
  const starting = app.document.getElementById('btnToggle').emit('click');
  await flush();
  assert.equal(typeof finishStart, 'function');
  await app.document.getElementById('btnStopAll').emit('click');
  const operations = app.calls.filter(call => ['start', 'stop_all'].includes(call.command));
  assert.deepEqual(operations.map(call => call.command), ['start', 'stop_all']);
  assert.ok(operations[1].args.operation > operations[0].args.operation,
    'the newer stop must receive its native ticket before decode/start returns');
  assert.equal(app.document.getElementById('btnToggle').disabled, false);
  const stoppedToast = app.document.getElementById('toast').textContent;
  finishStart(true);
  await starting;
  assert.equal(app.document.getElementById('toast').textContent, stoppedToast);
  assert.equal(app.document.getElementById('btnToggle').textContent, '启动');
});

test('slow play A then stop then play B dispatches all latest operations without waiting or reopening the engine', async () => {
  let finishA;
  let status = playbackStatus();
  const app = await boot({ onInvoke(command, args) {
    if (command === 'status') return status;
    if (command === 'play' && args.slot === 0) return new Promise(resolve => { finishA = resolve; });
    if (command === 'stop_all') { status = playbackStatus(); return true; }
    if (command === 'play') {
      status = playbackStatus({ playing: true, active_slot: args.slot, playhead_secs: 0.5 }); return true;
    }
  } });
  const older = app.card(0).emit('click');
  await flush();
  await app.document.getElementById('btnStopAll').emit('click');
  await app.card(1).emit('click');
  const operations = app.calls.filter(call => ['play', 'stop_all'].includes(call.command));
  assert.deepEqual(operations.map(call => call.command), ['play', 'stop_all', 'play']);
  assert.deepEqual(operations.map(call => call.args.operation), [1, 2, 3]);
  assert.equal(app.calls.some(call => ['start', 'stop', 'release'].includes(call.command)), false);
  const latestToast = app.document.getElementById('toast').textContent;
  finishA(true);
  await older;
  assert.equal(app.document.getElementById('toast').textContent, latestToast);
  assert.equal(app.card(1).classList.contains('playing'), true);
});

for (const initialLoop of [false, true]) {
  test(`all 49 play/preview/stop/switch/loop operation pairs keep one current stable card (loop=${initialLoop})`, async () => {
    const operations = ['play-A', 'play-B', 'preview-A', 'preview-B', 'stop', 'loop-on', 'loop-off'];
    for (const first of operations) for (const second of operations) {
      let status = playbackStatus({ looping: initialLoop });
      let expectedSound = null;
      let expectedPreview = false;
      let expectedLoop = initialLoop;
      const app = await boot({ onInvoke(command, args) {
        if (command === 'load_config') return { items, dir: '', hotkeys: ',F2', looping: initialLoop };
        if (command === 'status') return status;
        if (command === 'play') {
          status = playbackStatus({ looping: status.looping, playing: true, active_slot: args.slot, playhead_secs: 0.5 });
          return true;
        }
        if (command === 'preview') {
          status = playbackStatus({ looping: status.looping, preview_sound_id: args.soundId,
            preview_playing: true, preview_playhead_secs: 0.5, preview_duration_secs: 2 });
          return true;
        }
        if (command === 'stop_all') { status = playbackStatus({ looping: status.looping }); return true; }
        if (command === 'set_looping') { status = { ...status, looping: args.looping }; return true; }
      } });
      for (const operation of [first, second]) {
        if (operation.startsWith('play-') || operation.startsWith('preview-')) {
          expectedSound = operation.endsWith('A') ? 0 : 1;
          expectedPreview = operation.startsWith('preview-');
          await (expectedPreview
            ? app.card(expectedSound).querySelector('.prev') : app.card(expectedSound)).emit('click');
        } else if (operation === 'stop') {
          expectedSound = null;
          expectedPreview = false;
          await app.document.getElementById('btnStopAll').emit('click');
        } else {
          expectedLoop = operation === 'loop-on';
          const loop = app.document.getElementById('inLoop');
          loop.checked = expectedLoop;
          await loop.emit('change');
        }
        await app.pollStatus();
        for (let index = 0; index < 2; index++) {
          const context = `${first} → ${second} after ${operation}, sound ${index}`;
          assert.equal(app.card(index).classList.contains('playing'), index === expectedSound, context);
          assert.equal(app.card(index).classList.contains('previewing'), index === expectedSound && expectedPreview, context);
          assert.equal(app.card(index).querySelector('.bar').style.width, index === expectedSound ? '25%' : '0%', context);
        }
        assert.equal(app.document.getElementById('inLoop').checked, expectedLoop);
        assert.equal(status.looping, expectedLoop);
        assert.equal(app.calls.some(call => ['start', 'stop', 'release'].includes(call.command)), false,
          'loop and playback operations must not reopen the engine or interrupt mic forwarding');
      }
    }
  });
}

test('a late successful stop cannot replace a newer play selection or confirmation', async () => {
  let finishStop;
  let status = playbackStatus();
  const app = await boot({ onInvoke(command, args) {
    if (command === 'status') return status;
    if (command === 'stop_all') return new Promise(resolve => { finishStop = resolve; });
    if (command === 'play') {
      status = playbackStatus({ playing: true, active_slot: args.slot, playhead_secs: 0.5 }); return true;
    }
  } });
  const stopping = app.document.getElementById('btnStopAll').emit('click');
  await flush();
  await app.card(1).emit('click');
  const latestToast = app.document.getElementById('toast').textContent;
  finishStop(true);
  await stopping;
  assert.equal(app.document.getElementById('toast').textContent, latestToast);
  assert.equal(app.card(1).classList.contains('playing'), true);
});

test('an in-flight old poll cannot update the cards while a newer audio command is pending', async () => {
  let status = playbackStatus({ playing: true, active_slot: 0, playhead_secs: 0.5 });
  let delayStatus = false;
  let finishOldPoll;
  let finishPlay;
  const app = await boot({ onInvoke(command) {
    if (command === 'status') {
      if (delayStatus) { delayStatus = false; return new Promise(resolve => { finishOldPoll = resolve; }); }
      return status;
    }
    if (command === 'play') return new Promise(resolve => { finishPlay = resolve; });
  } });
  delayStatus = true;
  await app.pollStatus();
  const switching = app.card(1).emit('click');
  await flush();
  finishOldPoll(playbackStatus({ playing: true, active_slot: 0, playhead_secs: 1.8 }));
  await flush();
  assert.equal(app.card(0).querySelector('.bar').style.width, '25%',
    'a pre-command snapshot must be discarded even before the newer command can return');
  status = playbackStatus({ playing: true, active_slot: 1, playhead_secs: 0.5 });
  finishPlay(true);
  await switching;
  assert.equal(app.card(1).classList.contains('playing'), true);
});

for (const failedCommand of ['stop', 'release']) {
  test(`a failed ${failedCommand} preserves the native running state and allows later recovery`, async () => {
    let status = playbackStatus({ playing: true, active_slot: 0, playhead_secs: 0.5 });
    let failing = true;
    const app = await boot({ onInvoke(command) {
      if (command === 'status') return status;
      if (command === failedCommand) {
        if (failing) return Promise.reject(new Error('device operation failed'));
        status = playbackStatus({ running: false, passthrough: failedCommand === 'stop', slots: [] }); return true;
      }
    } });
    const control = app.document.getElementById(failedCommand === 'stop' ? 'btnToggle' : 'btnRelease');
    await control.emit('click');
    assert.match(app.document.getElementById('toast').textContent, /失败/);
    assert.equal(app.document.getElementById('btnToggle').textContent, '关闭音效');
    assert.equal(app.card(0).classList.contains('playing'), true);
    assert.equal(app.document.getElementById('btnToggle').disabled, false);
    failing = false;
    await control.emit('click');
    await app.pollStatus();
    assert.equal(app.document.getElementById('btnToggle').textContent, failedCommand === 'stop' ? '开启音效' : '启动');
    assert.equal(app.card(0).classList.contains('playing'), false);
  });
}

for (const command of ['stop', 'release']) for (const preview of [false, true]) {
  test(`successful ${command} clears ${preview ? 'preview' : 'main'} feedback before the next periodic poll`, async () => {
    let status = playbackStatus(preview
      ? {preview_playing:true,preview_sound_id:'sound-A',preview_playhead_secs:0.5,preview_duration_secs:2}
      : {playing:true,active_slot:0,playhead_secs:0.5});
    const app = await boot({ onInvoke(cmd) {
      if (cmd === 'status') return status;
      if (cmd === command) {
        status = playbackStatus({running:false,passthrough:command==='stop',slots:[]}); return true;
      }
    } });
    assert.equal(app.card(0).classList.contains('playing'), true);
    await app.document.getElementById(command === 'stop' ? 'btnToggle' : 'btnRelease').emit('click');
    for (const card of [app.card(0), app.card(1)]) {
      assert.equal(card.classList.contains('playing'), false,
        'a confirmed stop/release must not render the previously active sound from cached status');
      assert.equal(card.classList.contains('previewing'), false);
      assert.equal(card.querySelector('.bar').style.width, '0%');
    }
    assert.equal(app.document.getElementById('btnToggle').textContent, command === 'stop' ? '开启音效' : '启动');
  });
}

for (const command of ['stop','release']) for (const preview of [false,true]) {
  test(`confirmed ${command} clears cached ${preview?'preview':'main'} even when its status refresh fails`,async()=>{
    let unavailable=false;
    const old=playbackStatus(preview
      ? {preview_playing:true,preview_sound_id:'sound-A',preview_playhead_secs:0.5,preview_duration_secs:2}
      : {playing:true,active_slot:0,playhead_secs:0.5});
    const app=await boot({onInvoke(cmd){
      if(cmd==='status')return unavailable?Promise.reject(new Error('status transport failed')):old;
      if(cmd===command){unavailable=true;return true;}
    }});
    await app.document.getElementById(command==='stop'?'btnToggle':'btnRelease').emit('click');
    for(const card of[app.card(0),app.card(1)]){
      assert.equal(card.classList.contains('playing'),false,
        'a status outage must not retain sound feedback after a native stop was confirmed');
      assert.equal(card.classList.contains('previewing'),false);
      assert.equal(card.querySelector('.bar').style.width,'0%');
    }
    assert.equal(app.document.getElementById('btnToggle').textContent,command==='stop'?'开启音效':'启动');
  });
}

test('volume dragging changes the active preview even when the engine has not started', async () => {
  const app = await boot();
  await app.card(0).querySelector('.prev').emit('click');
  const slider = app.card(0).querySelector('.vol');
  slider.value = '-18';
  await slider.emit('input');
  await flush();
  assert.deepEqual(app.calls.filter(call => call.command === 'set_sound_gain'),
    [{ command: 'set_sound_gain', args: { soundId: 'sound-A', db: -18 } }]);
  assert.equal(app.calls.find(call => call.command === 'preview').args.soundId, 'sound-A');
});

test('native status switching A to B updates the selected card and its progress, then clears on end', async () => {
  let status = playbackStatus();
  const app = await boot({ onInvoke(command, args) {
    if (command === 'status') return status;
    if (command === 'play') {
      status = playbackStatus({ playing: true, active_slot: args.slot, playhead_secs: args.slot ? 0.5 : 1 });
      return true;
    }
  } });
  await app.card(0).emit('click');
  assert.equal(app.card(0).classList.contains('playing'), true);
  assert.equal(app.card(0).querySelector('.bar').style.width, '50%');
  await app.card(1).emit('click');
  assert.equal(app.card(0).classList.contains('playing'), false);
  assert.equal(app.card(0).querySelector('.bar').style.width, '0%');
  assert.equal(app.card(1).classList.contains('playing'), true);
  assert.equal(app.card(1).querySelector('.bar').style.width, '25%');
  assert.equal(app.document.querySelectorAll('.playing').filter(element => element.classList.contains('card')).length, 1);
  assert.deepEqual(app.calls.filter(call => call.command === 'play').map(call => call.args.slot), [0, 1]);
  assert.equal(app.calls.some(call => call.command === 'stop_all'), false,
    'switching must use the backend single-play command without a frontend stop workaround');
  status = playbackStatus();
  await app.pollStatus();
  for (const card of [app.card(0), app.card(1)]) {
    assert.equal(card.classList.contains('playing'), false);
    assert.equal(card.querySelector('.bar').style.width, '0%');
  }
});

test('preview status selects only its stable sound and keeps real progress after 400ms until stopped', async () => {
  let status = playbackStatus({ running: false, passthrough: false, slots: [] });
  const app = await boot({ onInvoke(command, args) {
    if (command === 'status') return status;
    if (command === 'preview') {
      status = playbackStatus({ running: false, passthrough: false, slots: [],
        preview_sound_id: args.soundId, preview_playing: true,
        preview_playhead_secs: args.soundId === 'sound-A' ? 1 : 0.5, preview_duration_secs: 2 });
      return true;
    }
    if (command === 'stop_all') {
      status = playbackStatus({ running: false, passthrough: false, slots: [] });
      return true;
    }
  } });
  await app.card(0).querySelector('.prev').emit('click');
  assert.equal(app.card(0).classList.contains('playing'), true,
    'a real preview must select its own card even without the engine');
  assert.equal(app.card(0).querySelector('.bar').style.width, '50%');
  await app.advanceTimers(400);
  assert.equal(app.card(0).querySelector('.prev').classList.contains('playing'), true);
  await app.card(1).querySelector('.prev').emit('click');
  assert.equal(app.card(0).classList.contains('playing'), false);
  assert.equal(app.card(0).querySelector('.prev').classList.contains('playing'), false);
  assert.equal(app.card(0).querySelector('.bar').style.width, '0%');
  assert.equal(app.card(1).classList.contains('playing'), true);
  assert.equal(app.card(1).querySelector('.bar').style.width, '25%');
  await app.document.getElementById('btnStopAll').emit('click');
  for (const card of [app.card(0), app.card(1)]) {
    assert.equal(card.classList.contains('playing'), false);
    assert.equal(card.querySelector('.prev').classList.contains('playing'), false);
    assert.equal(card.querySelector('.bar').style.width, '0%');
  }
});

test('main and preview transitions use one highlighted card and preview completion clears it', async () => {
  let status = playbackStatus({ playing: true, active_slot: 0, playhead_secs: 1 });
  const app = await boot({ onInvoke(command, args) {
    if (command === 'status') return status;
    if (command === 'preview') {
      status = playbackStatus({ preview_sound_id: args.soundId, preview_playing: true,
        preview_playhead_secs: 0.5, preview_duration_secs: 2 });
      return true;
    }
    if (command === 'play') {
      status = playbackStatus({ playing: true, active_slot: args.slot, playhead_secs: 0.25 });
      return true;
    }
  } });
  assert.equal(app.card(0).classList.contains('playing'), true);
  await app.card(1).querySelector('.prev').emit('click');
  assert.equal(app.card(0).classList.contains('playing'), false);
  assert.equal(app.card(1).classList.contains('previewing'), true);
  assert.match(app.card(1).querySelector('.playback-state').textContent, /试听中.*仅自己/);
  assert.equal(app.card(1).querySelector('.bar').style.width, '25%');
  await app.card(0).emit('click');
  assert.equal(app.card(1).classList.contains('playing'), false);
  assert.equal(app.card(1).querySelector('.prev').classList.contains('playing'), false);
  assert.equal(app.card(0).classList.contains('previewing'), false);
  assert.match(app.card(0).querySelector('.playback-state').textContent, /播放中.*对方/);
  assert.match(app.document.getElementById('toast').textContent, /已切换播放/);
  await app.card(1).querySelector('.prev').emit('click');
  status = playbackStatus();
  await app.pollStatus();
  for (const card of [app.card(0), app.card(1)]) {
    assert.equal(card.classList.contains('playing'), false);
    assert.equal(card.classList.contains('previewing'), false);
    assert.equal(card.querySelector('.prev').textContent, '试听');
    assert.equal(card.querySelector('.playback-state').textContent, '');
    assert.equal(card.querySelector('.bar').style.width, '0%');
  }
});

test('loading a preview does not falsely highlight a card before audio starts', async () => {
  const status = playbackStatus({ running: false, passthrough: false, slots: [], preview_sound_id: 'sound-A' });
  let finishPreview;
  const app = await boot({ onInvoke(command) {
    if (command === 'status') return status;
    if (command === 'preview') return new Promise(resolve => { finishPreview = resolve; });
  } });
  const click = app.card(0).querySelector('.prev').emit('click');
  await flush();
  await app.pollStatus();
  assert.match(app.document.getElementById('toast').textContent, /加载试听/);
  assert.equal(app.card(0).classList.contains('playing'), false);
  assert.equal(app.card(0).querySelector('.prev').classList.contains('playing'), false);
  assert.equal(app.card(0).querySelector('.bar').style.width, '0%');
  finishPreview(true);
  await click;
});

test('a delayed older status cannot restore A after the play command already selected B', async () => {
  let status = playbackStatus({ playing: true, active_slot: 0, playhead_secs: 1 });
  let delayNext = false;
  let finishOlderStatus;
  const app = await boot({ onInvoke(command, args) {
    if (command === 'status') {
      if (delayNext) {
        delayNext = false;
        return new Promise(resolve => { finishOlderStatus = resolve; });
      }
      return status;
    }
    if (command === 'play') {
      status = playbackStatus({ playing: true, active_slot: args.slot, playhead_secs: 0.5 });
      return true;
    }
  } });
  const older = status;
  delayNext = true;
  await app.pollStatus();
  assert.equal(typeof finishOlderStatus, 'function');
  await app.card(1).emit('click');
  assert.equal(app.card(1).classList.contains('playing'), true);
  finishOlderStatus(older);
  await flush();
  assert.equal(app.card(0).classList.contains('playing'), false,
    'the earlier status snapshot must not replace the newer command result');
  assert.equal(app.card(1).classList.contains('playing'), true);
  assert.equal(app.card(1).querySelector('.bar').style.width, '25%');
});

test('rapid adjustments on different sound cards both reach audio while dragging', async () => {
  const app = await boot({ running: true });
  const first = app.card(0).querySelector('.vol');
  const second = app.card(1).querySelector('.vol');
  first.value = '-12'; second.value = '-24';
  await Promise.all([first.emit('input'), second.emit('input')]);
  await flush();
  assert.deepEqual(app.calls.filter(call => call.command === 'set_sound_gain'), [
    { command: 'set_sound_gain', args: { soundId: 'sound-A', db: -12 } },
    { command: 'set_sound_gain', args: { soundId: 'sound-B', db: -24 } },
  ]);
});

test('the stop playback button sends the combined stop command and confirms previews stopped', async () => {
  const app = await boot();
  await app.card(0).querySelector('.prev').emit('click');
  await app.document.getElementById('btnStopAll').emit('click');
  assert.ok(app.calls.some(call => call.command === 'stop_all'));
  assert.match(app.document.getElementById('toast').textContent, /试听/);
  assert.match(app.document.getElementById('btnStopAll').title, /试听/);
});

test('each volume slider explains its purpose, displays live dB and saves the edited gain', async () => {
  const app = await boot();
  const card = app.card(0);
  const slider = card.querySelector('.vol');
  assert.match(card.querySelector('.volume-label')?.textContent || '', /音效音量/);
  assert.match(slider.getAttribute('aria-label') || '', /Alpha.*音效音量/);
  assert.match(card.querySelector('.volume-help')?.textContent || '', /试听.*播放.*生效/);
  assert.equal(card.querySelector('.volume-value')?.textContent, '0 dB');
  slider.value = '-40';
  await slider.emit('input');
  assert.equal(card.querySelector('.volume-value')?.textContent, '-40 dB');
  assert.match(slider.getAttribute('aria-valuetext') || '', /-40 dB/);
  assert.equal(slider.classList.contains('muted'), false, '-40dB is quiet, not silence');
  await slider.emit('change');
  const saved = app.calls.filter(call => call.command === 'save_config').at(-1);
  assert.equal(saved?.args.items[0].gain, -40);
  assert.equal(saved.args.items[0].id, 'sound-A');
  assert.deepEqual(saved.args.items[0].tags, ['a,b']);
  assert.equal(saved.args.hotkeys, ',F2');
  // Rendering and the next preview must keep the local edited value.
  await app.document.getElementById('inSearch').emit('input');
  assert.equal(app.card(0).querySelector('.vol').value, '-40');
  await app.card(0).querySelector('.prev').emit('click');
  assert.equal(app.calls.filter(call => call.command === 'preview').at(-1).args.gainDb, -40);
});

test('a slow native gain update coalesces the same sound while other sounds remain live', async () => {
  let finishFirst;
  let first = true;
  const app = await boot({ onInvoke(command, args) {
    if (command === 'set_sound_gain' && args.soundId === 'sound-A' && first) {
      first = false;
      return new Promise(resolve => { finishFirst = resolve; });
    }
  } });
  const a = app.card(0).querySelector('.vol');
  const b = app.card(1).querySelector('.vol');
  for (const db of [-3, -6, -9, -12]) { a.value = String(db); await a.emit('input'); }
  b.value = '-18'; await b.emit('input');
  await flush();
  assert.deepEqual(app.calls.filter(call => call.command === 'set_sound_gain'), [
    { command: 'set_sound_gain', args: { soundId: 'sound-A', db: -3 } },
    { command: 'set_sound_gain', args: { soundId: 'sound-B', db: -18 } },
  ]);
  finishFirst(true);
  await flush();
  assert.deepEqual(app.calls.filter(call => call.command === 'set_sound_gain').at(-1),
    { command: 'set_sound_gain', args: { soundId: 'sound-A', db: -12 } });
});

test('rapid completed volume edits save in order so an older write cannot replace the latest gain', async () => {
  let finishSave;
  let first = true;
  const app = await boot({ onInvoke(command) {
    if (command === 'save_config' && first) {
      first = false;
      return new Promise(resolve => { finishSave = resolve; });
    }
  } });
  const slider = app.card(0).querySelector('.vol');
  slider.value = '-3'; await slider.emit('input');
  const firstChange = slider.emit('change');
  await flush();
  slider.value = '-18'; await slider.emit('input');
  const secondChange = slider.emit('change');
  await flush();
  assert.equal(app.calls.filter(call => call.command === 'save_config').length, 1,
    'a pending save must finish before the next snapshot is written');
  finishSave(true);
  await Promise.all([firstChange, secondChange]);
  assert.deepEqual(app.calls.filter(call => call.command === 'save_config')
    .map(call => call.args.items[0].gain), [-3, -18]);
});
