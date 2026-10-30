const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

// Exercise the shipped main.js through DOM events and the Tauri IPC boundary.
// Only the browser, clock and native command boundary are substituted.
const root = path.join(__dirname, '../apps/mixer-gui/dist');
// 静态源码只读一次；每个用例仍使用独立 DOM 与 VM 上下文。
const sources = {
  index: fs.readFileSync(path.join(root, 'index.html'), 'utf8'),
  theme: fs.existsSync(path.join(root, 'theme.js'))
    ? fs.readFileSync(path.join(root, 'theme.js'), 'utf8') : null,
  library: fs.readFileSync(path.join(root, 'library-model.js'), 'utf8'),
  main: fs.readFileSync(path.join(root, 'main.js'), 'utf8'),
};
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
    for (const child of this.children) child.parentElement = null;
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
    if (selector === '*') return true;
    if (selector.startsWith('.')) return this.classList.contains(selector.slice(1));
    if (selector.startsWith('#')) return this.id === selector.slice(1);
    if (selector.startsWith('[')) return this.getAttribute(selector.slice(1, -1)) !== null;
    return this.tagName.toLowerCase() === selector.toLowerCase();
  }
  querySelectorAll(selector) {
    if (selector.includes(',')) {
      const selected = new Set(selector.split(',').flatMap(part => this.querySelectorAll(part.trim())));
      return this.querySelectorAll('*').filter(node => selected.has(node));
    }
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
  contains(node) { for (let parent = node; parent; parent = parent.parentElement) if (parent === this) return true; return false; }
  closest(selector) { for (let parent = this; parent; parent = parent.parentElement) if (parent.matches(selector)) return parent; return null; }
  click() { if (!this.disabled) return this.emit('click'); }
  select() {}
  blur() { return this.emit('blur'); }
}

const items = [
  { id: 'sound-A', file: '/sounds/A.wav', name: 'Alpha', gain: 0, tags: ['a,b'] },
  { id: 'sound-B', file: '/sounds/B.wav', name: 'Beta', gain: -6, tags: [] },
];
const flush = async () => { for (let i = 0; i < 25; i++) await Promise.resolve(); };
// Boot performs voice status/preset IPC before its first render, so it needs a deeper microtask drain.
const flushBoot = async () => { for (let i = 0; i < 100; i++) await Promise.resolve(); };
const playbackStatus = (patch = {}) => ({
  running: true, passthrough: true, device: 'Fake output', uptime_secs: 1,
  slots: items.map((item, index) => ({ index, source_index: index, id: item.id,
    name: item.name, file: item.file, gain_db: item.gain, hotkey: index ? 'F2' : '', duration: 2 })),
  overruns: 0, underruns: 0, playing: false, active_slot: -1, playhead_secs: 0,
  preview_sound_id: null, preview_playing: false, preview_playhead_secs: 0, preview_duration_secs: 0,
  out_peak_db: -18, out_rms_db: -24, limiter_db: 0, ring_ms: 20, remote_urls: [],
  ...patch,
});

// Voice fixture mirrors VoiceParamsDto so assertions can require a complete snapshot.
const voiceParams = (patch = {}) => ({
  pitch_semitones: 0, formant_semitones: 0,
  gate_enabled: false, gate_db: -45,
  eq_low_enabled: false, eq_low_db: 0,
  eq_mid_enabled: false, eq_mid_db: 0,
  eq_high_enabled: false, eq_high_db: 0,
  drive_enabled: false, drive: 0,
  robot_enabled: false, robot_hz: 50, robot_mix: 0.8,
  ...patch,
});
const voiceSnapshot = (patch = {}) => ({
  enabled: false, bypassed: false, ab_bypassed: false, phase: 'off',
  preset_id: 'builtin:female', preset_name: '女声',
  modified: false, draft_available: false,
  params: voiceParams(), preview: { state: 'idle', elapsed_ms: 0, reason: null },
  metrics: null, save_error: null, generation: 1,
  ...patch,
});
const voicePresetList = [
  { id: 'builtin:female', name: '女声', builtin: true,
    params: voiceParams({ pitch_semitones: 4, formant_semitones: 3 }) },
  { id: 'builtin:male', name: '男声', builtin: true,
    params: voiceParams({ pitch_semitones: -4, formant_semitones: -2 }) },
];
const voiceFixture = command => {
  if (command === 'voice_status') return voiceSnapshot();
  if (command === 'voice_presets') return voicePresetList;
  return undefined;
};
const userPresetFixture = (patch = {}) => ({
  id: 'user:live', name: '直播女声', builtin: false,
  params: voiceParams({ pitch_semitones: 2 }),
  ...patch,
});

async function boot({ running = false, initialLoop, onInvoke, now, dark = false, storedTheme, storageFailure } = {}) {
  const document = new Element('document', null);
  document.document = document;
  document.createElement = tag => new Element(tag, document);
  document.getElementById = id => document.querySelector('#' + id);
  document.innerHTML = sources.index;
  document.documentElement = document.querySelector('html');
  if (initialLoop !== undefined) document.getElementById('inLoop').checked = initialLoop;
  const calls = [];
  const timers = new Map();
  const intervals = new Map();
  let nextTimer = 1;
  let nextOperation = 1;
  const themeStorage = new Map(storedTheme === undefined ? [] : [['mixer.ui.theme', storedTheme]]);
  const mediaListeners = [];
  const windowListeners = new Map();
  const themeMedia = { matches: dark, addEventListener(type, listener) { if (type === 'change') mediaListeners.push(listener); } };
  const window = {
    addEventListener(type, listener) {
      if (!windowListeners.has(type)) windowListeners.set(type, []);
      windowListeners.get(type).push(listener);
    },
    innerWidth: 920, innerHeight: 660,
    matchMedia() { return themeMedia; },
    localStorage: {
      getItem(key) { if (storageFailure === 'read') throw new Error('storage unavailable'); return themeStorage.get(key) ?? null; },
      setItem(key, value) { if (storageFailure === 'write') throw new Error('storage unavailable'); themeStorage.set(key, value); },
    },
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
    Date: now ? class extends Date { static now() { return now(); } } : Date,
    navigator: { clipboard: { writeText: async () => {} } },
    setTimeout(callback, delay) { const id = nextTimer++; timers.set(id, { callback, delay }); return id; },
    clearTimeout(id) { timers.delete(id); },
    setInterval(callback, delay) { const id = nextTimer++; intervals.set(id, { callback, delay }); return id; },
  });
  if (sources.theme !== null) vm.runInContext(sources.theme, context);
  vm.runInContext(sources.library, context);
  vm.runInContext(sources.main, context);
  await flushBoot();
  assert.equal(document.querySelectorAll('.card').length, 2,
    'the shipped script must initialize and render the loaded library');
  assert.deepEqual(calls.filter(call => call.command === 'js_error'), [], 'frontend must boot without errors');
  return {
    document, calls, themeStorage,
    systemTheme(dark) {
      themeMedia.matches = dark;
      for (const listener of mediaListeners) listener({ matches: dark });
    },
    card(index) { return document.querySelectorAll('.card')[index]; },
    volume(index) {
      document.querySelectorAll('.card')[index].querySelector('.sound-settings').click();
      return document.getElementById('soundGain');
    },
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
    async emitWindow(type, details = {}) {
      const event = { type, target: window, stopPropagation() {}, preventDefault() {}, ...details };
      await Promise.all((windowListeners.get(type) || []).map(listener => listener(event)));
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
  assert.equal(app.volume(0).value, '12');
  assert.equal(app.document.getElementById('soundGainValue').textContent, '+12 dB');
  assert.equal(app.volume(1).value, '-40');
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
  assert.equal(app.document.activeElement, app.document.getElementById('toolsMenu').querySelector('summary'), 'closing must restore the keyboard focus');
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
    assert.equal(app.document.activeElement, app.document.getElementById('toolsMenu').querySelector('summary'));
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
    await app.volume(0).emit('change');
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
  assert.equal(app.card(0).querySelector('.dur').textContent, '0:02');
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
  const slider = app.volume(0);
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
  assert.match(app.card(0).querySelector('.playback-state').textContent, /播放中.*虚拟设备/);
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
  const first = app.volume(0);
  first.value = '-12'; await first.emit('input');
  const second = app.volume(1);
  second.value = '-24'; await second.emit('input');
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

test('音效详情显示音量用途与实时dB并按稳定ID保存', async () => {
  const app = await boot();
  const card = app.card(0);
  const slider = app.volume(0);
  const detail = app.document.getElementById('soundOverlay');
  assert.match(detail.querySelector('.volume-label')?.textContent || '', /音效音量/);
  assert.match(slider.getAttribute('aria-label') || '', /Alpha.*音效音量/);
  assert.match(detail.querySelector('.volume-help')?.textContent || '', /试听.*播放.*生效/);
  assert.equal(detail.querySelector('.volume-value')?.textContent, '0 dB');
  slider.value = '-40';
  await slider.emit('input');
  assert.equal(detail.querySelector('.volume-value')?.textContent, '-40 dB');
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
  assert.equal(app.volume(0).value, '-40');
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
  const a = app.volume(0);
  for (const db of [-3, -6, -9, -12]) { a.value = String(db); await a.emit('input'); }
  const b = app.volume(1);
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
  const slider = app.volume(0);
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

const audioCommands = app => app.calls.filter(call =>
  ['start', 'play', 'preview', 'stop_all', 'stop', 'release', 'begin_playback_operation'].includes(call.command));

test('简洁卡片提供独立语义按钮并将音量放入详情', async () => {
  const app = await boot();
  const card = app.card(0);
  assert.equal(card.tagName, 'ARTICLE');
  assert.equal(card.querySelector('.play-trigger').tagName, 'BUTTON');
  assert.equal(card.querySelector('.prev').tagName, 'BUTTON');
  assert.equal(card.querySelector('.vol'), null, '卡片不常驻滑杆');
  assert.equal(card.querySelector('.sound-settings').getAttribute('aria-haspopup'), 'dialog');
  const trigger = card.querySelector('.sound-settings');
  trigger.focus();
  await trigger.emit('click');
  assert.equal(app.document.getElementById('soundOverlay').classList.contains('hidden'), false);
  assert.equal(app.document.getElementById('soundTitle').textContent, 'Alpha');
  assert.equal(app.document.getElementById('soundFile').textContent, '/sounds/A.wav');
  assert.deepEqual(audioCommands(app), [], '打开详情不能触发播放');
  await app.document.getElementById('btnSoundClose').emit('click');
  assert.equal(app.document.activeElement, trigger);
});

test('侧栏滑杆与数字框同步并保存最新全局音量', async () => {
  const app = await boot();
  const slider = app.document.getElementById('sfxGainSlider');
  const number = app.document.getElementById('inSfxGain');
  slider.value = '0';
  await slider.emit('input');
  assert.equal(number.value, '0');
  assert.equal(slider.getAttribute('aria-valuetext'), '0 dB');
  await app.advanceTimers(120);
  assert.deepEqual(app.calls.filter(c => c.command === 'set_gains').at(-1).args, { micDb: 0, sfxDb: 0 });
  number.value = '-18';
  await number.emit('input');
  assert.equal(slider.value, '-18');
  await app.card(0).querySelector('.prev').emit('click');
  assert.equal(app.calls.filter(c => c.command === 'set_gains').at(-1).args.sfxDb, -18);
  assert.equal(app.calls.filter(c => c.command === 'save_config').at(-1).args.sfxGainDb, -18);
});

test('恢复配置同时恢复侧栏两种音量输入并保留显式false', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'load_config') return { items, mic_gain_db: -7, sfx_gain_db: 0, monitor: false, looping: false };
  } });
  assert.equal(app.document.getElementById('micGainSlider').value, '-7');
  assert.equal(app.document.getElementById('sfxGainSlider').value, '0');
  assert.equal(app.document.getElementById('inMonitor').checked, false);
  assert.equal(app.document.getElementById('inLoop').checked, false);
});

test('底部播放器在筛选隐藏当前卡片后仍显示原生音效和进度', async () => {
  let status = playbackStatus({ playing: true, active_slot: 0, playhead_secs: 1 });
  const app = await boot({ onInvoke(command) { if (command === 'status') return status; } });
  const search = app.document.getElementById('inSearch');
  search.value = 'Beta'; await search.emit('input');
  assert.equal(app.document.querySelectorAll('.card').length, 1);
  assert.equal(app.document.getElementById('playerName').textContent, 'Alpha');
  assert.equal(app.document.getElementById('player').dataset.kind, 'main');
  assert.equal(app.document.getElementById('playerProgress').style.width, '50%');
  assert.equal(app.document.getElementById('playerTime').textContent, '0:01 / 0:02');
  status = playbackStatus({ preview_playing: true, preview_sound_id: 'sound-B', preview_playhead_secs: .5, preview_duration_secs: 2 });
  await app.pollStatus();
  assert.equal(app.document.getElementById('playerName').textContent, 'Beta');
  assert.equal(app.document.getElementById('player').dataset.kind, 'preview');
  assert.match(app.document.getElementById('playerMode').textContent, /仅自己可听.*只播一次/);
  assert.equal(app.document.getElementById('playerProgress').style.width, '25%');
  status = playbackStatus(); await app.pollStatus();
  assert.equal(app.document.getElementById('player').dataset.kind, 'idle');
  assert.equal(app.document.getElementById('playerProgress').style.width, '0%');
});

test('停止确认后状态读取失败也立即清除底部与卡片反馈', async () => {
  let failed = false;
  const app = await boot({ onInvoke(command) {
    if (command === 'status') return failed ? Promise.reject(new Error('status unavailable'))
      : playbackStatus({ playing: true, active_slot: 0, playhead_secs: 1 });
    if (command === 'stop_all') { failed = true; return true; }
  } });
  assert.equal(app.document.getElementById('player').dataset.kind, 'main');
  await app.document.getElementById('btnStopAll').emit('click');
  assert.equal(app.document.getElementById('player').dataset.kind, 'idle');
  assert.equal(app.card(0).classList.contains('playing'), false);
});

test('运行中的监听明确只读且待机继续显示真实混合电平', async () => {
  let status = playbackStatus();
  const app = await boot({ onInvoke(command) { if (command === 'status') return status; } });
  assert.equal(app.document.getElementById('inMonitor').disabled, true);
  assert.match(app.document.getElementById('monitorHelp').textContent, /已开启.*释放设备/);
  status = playbackStatus({ running: false, passthrough: true, slots: [] }); await app.pollStatus();
  assert.equal(app.document.getElementById('meter').classList.contains('off'), false);
  assert.equal(app.document.getElementById('signalLabel').textContent, '混合信号输出中');
  assert.match(app.document.getElementById('playerMode').textContent, /麦克风仍在转发/);
  status = playbackStatus({ running: false, passthrough: false, slots: [] }); await app.pollStatus();
  assert.equal(app.document.getElementById('inMonitor').disabled, false);
  assert.equal(app.document.getElementById('meter').classList.contains('off'), true);
});

test('详情保存名称标签和空热键时保持条目身份及原始标点标签', async () => {
  const app = await boot();
  app.volume(0);
  app.document.getElementById('soundName').value = '新的名称';
  app.document.getElementById('soundHotkey').value = '';
  await app.document.getElementById('btnSoundSave').emit('click');
  const saved = app.calls.filter(c => c.command === 'save_config').at(-1).args;
  assert.equal(saved.items[0].id, 'sound-A');
  assert.equal(saved.items[0].name, '新的名称');
  assert.deepEqual(saved.items[0].tags, ['a,b']);
  assert.equal(saved.hotkeys, ',F2');
  assert.equal(app.card(0).querySelector('.name').textContent, '新的名称');
});

test('筛选后的详情增益使用稳定ID而非可见顺序', async () => {
  const app = await boot({ running: true });
  const search = app.document.getElementById('inSearch');
  search.value = 'Beta'; await search.emit('input');
  const gain = app.volume(0); gain.value = '-18'; await gain.emit('input'); await gain.emit('change');
  assert.deepEqual(app.calls.filter(c => c.command === 'set_sound_gain').at(-1).args, { soundId: 'sound-B', db: -18 });
  const saved = app.calls.filter(c => c.command === 'save_config').at(-1).args;
  assert.equal(saved.items[0].gain, 0);
  assert.equal(saved.items[1].gain, -18);
  assert.equal(app.document.getElementById('soundName').disabled, true);
});

test('详情前移按条目移动热键并恢复重建后的详情按钮焦点', async () => {
  const app = await boot();
  app.card(1).querySelector('.sound-settings').focus();
  app.volume(1);
  await app.document.getElementById('btnSoundLeft').emit('click');
  assert.equal(app.card(0).querySelector('.name').textContent, 'Beta');
  assert.equal(app.document.getElementById('soundHotkey').value, 'F2');
  await app.document.getElementById('btnSoundClose').emit('click');
  assert.equal(app.document.activeElement, app.card(0).querySelector('.sound-settings'));
  const saved = app.calls.filter(c => c.command === 'save_config').at(-1).args;
  assert.deepEqual(saved.items.map(item => item.id), ['sound-B', 'sound-A']);
  assert.equal(saved.hotkeys, 'F2,');
});

for (const panel of ['setup', 'soundOverlay']) {
  test(`${panel}约束焦点并阻止数字与空格播放快捷键`, async () => {
    const app = await boot({ running: true });
    if (panel === 'setup') await app.document.getElementById('btnSetup').emit('click');
    else app.volume(0);
    const first = app.document.getElementById(panel === 'setup' ? 'btnSetupClose' : 'btnSoundClose');
    const last = app.document.getElementById(panel === 'setup' ? 'btnSetupSave' : 'btnSoundSave');
    assert.equal(app.document.getElementById('appShell').inert, true);
    first.focus(); await app.document.emit('keydown', { key: 'Tab', shiftKey: true, target: first });
    assert.equal(app.document.activeElement, last);
    await app.document.emit('keydown', { key: 'Tab', target: last });
    assert.equal(app.document.activeElement, first);
    for (const key of ['1', '2', ' ']) await app.document.emit('keydown', { key, target: first });
    assert.deepEqual(audioCommands(app), []);
    await app.document.emit('keydown', { key: 'Escape', target: first });
    assert.equal(app.document.getElementById(panel).classList.contains('hidden'), true);
    assert.equal(app.document.getElementById('appShell').inert, false);
  });
}

test('旧详情保存回复不能关闭新打开的面板', async () => {
  let finish;
  const app = await boot({ onInvoke(command) {
    if (command === 'save_config') return new Promise(resolve => { finish = resolve; });
  } });
  app.volume(0);
  const saved = app.document.getElementById('btnSoundSave').emit('click');
  await flush();
  await app.document.getElementById('btnSoundClose').emit('click');
  app.volume(1);
  finish(true); await saved;
  assert.equal(app.document.getElementById('soundOverlay').classList.contains('hidden'), false);
  assert.equal(app.document.getElementById('soundTitle').textContent, 'Beta');
});

for (const os of ['macos', 'windows']) {
  test(`${os}使用同一混音控件与平台连接说明及搜索快捷键`, async () => {
    const app = await boot({ onInvoke(command) { if (command === 'platform_info') return { os }; } });
    assert.equal(app.document.getElementById('searchShortcut').textContent, os === 'windows' ? 'Ctrl K' : '⌘ K');
    assert.match(app.document.getElementById('routeHelp').textContent, os === 'windows' ? /CABLE Input.*CABLE Output/ : /BlackHole.*同一设备/);
    const key = { key: 'k', target: app.document, ...(os === 'windows' ? { ctrlKey: true } : { metaKey: true }) };
    await app.document.emit('keydown', key);
    assert.equal(app.document.activeElement, app.document.getElementById('inSearch'));
    assert.deepEqual(audioCommands(app), []);
  });
}

test('非播放按钮空格保持原生激活且组合键和长按不会重复触发音效', async () => {
  const app = await boot({ running: true });
  await app.document.emit('keydown', { key: ' ', target: app.document.getElementById('btnAdd') });
  await app.document.emit('keydown', { key: '1', ctrlKey: true, target: app.document });
  await app.document.emit('keydown', { key: '1', repeat: true, target: app.document });
  assert.deepEqual(audioCommands(app), []);
});

test('旧自检失败不能将结果插入新打开的遥控面板', async () => {
  let reject;
  const app = await boot({ onInvoke(command) {
    if (command === 'self_test') return new Promise((_, fail) => { reject = fail; });
  } });
  const selftest = app.document.getElementById('btnSelfTest').emit('click');
  await flush();
  await app.document.getElementById('btnUtilityClose').emit('click');
  await app.document.getElementById('btnQr').emit('click');
  reject(new Error('older failed')); await selftest;
  assert.equal(app.document.getElementById('utilityTitle').textContent, '手机遥控');
  assert.equal(app.document.getElementById('qrPanel').classList.contains('hidden'), false);
  assert.equal(app.document.getElementById('testPanel').classList.contains('hidden'), true);
});

test('配置文件会拒绝保存时启动即显示常驻提示并带出原因', async () => {
  const notice = '配置文件 /cfg/config.toml 不是有效 TOML（第 3 行：invalid escape），已拒绝写入';
  const app = await boot({ onInvoke(command) {
    if (command === 'load_config') {
      return { items, dir: '', hotkeys: ',F2', config_notices: [notice], config_write_blocked: true };
    }
  } });
  const banner = app.document.getElementById('notice');
  assert.equal(banner.classList.contains('hidden'), false, '会阻止保存的问题必须常驻显示');
  assert.match(banner.textContent, /配置文件需要处理/);
  assert.match(banner.textContent, /不是有效 TOML/);
});

test('只是数值被规范化的配置提示不占用常驻提示条', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'load_config') {
      return { items, dir: '', hotkeys: ',F2', config_write_blocked: false,
        config_notices: ['用户预设「甲」参数越界，已钳制'] };
    }
  } });
  assert.equal(app.document.getElementById('notice').classList.contains('hidden'), true);
  assert.match(app.document.getElementById('toast').textContent, /已钳制/);
});

test('配置没有问题时不显示配置提示', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'load_config') {
      return { items, dir: '', hotkeys: ',F2', config_notices: [], config_write_blocked: false };
    }
  } });
  assert.equal(app.document.getElementById('notice').classList.contains('hidden'), true);
  assert.doesNotMatch(app.document.getElementById('toast').textContent, /配置文件/);
});

test('保存被宿主拒绝时说明未保存与原因而不虚报成功', async () => {
  const reason = '配置文件 /cfg/config.toml 不是有效 TOML，已拒绝写入以免覆盖原内容';
  const app = await boot({ onInvoke(command) {
    if (command === 'save_config') return Promise.reject(new Error(reason));
  } });
  await app.document.getElementById('btnSetup').emit('click');
  await app.document.getElementById('btnSetupSave').emit('click');
  assert.equal(app.document.getElementById('setup').classList.contains('hidden'), false);
  const toast = app.document.getElementById('toast').textContent;
  assert.match(toast, /保存失败/);
  assert.match(toast, /已拒绝写入/);
  assert.doesNotMatch(toast, /已保存/);
});

test('连接保存失败保留面板并说明未保存而不虚报成功', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'save_config') return Promise.reject(new Error('permission denied'));
  } });
  await app.document.getElementById('btnSetup').emit('click');
  await app.document.getElementById('btnSetupSave').emit('click');
  assert.equal(app.document.getElementById('setup').classList.contains('hidden'), false);
  assert.match(app.document.getElementById('toast').textContent, /保存失败/);
  assert.doesNotMatch(app.document.getElementById('toast').textContent, /已保存/);
});

for (const target of ['.play-trigger', '.prev']) {
  test(`${target}点击后空格仍停止而不是原生再次激活`, async () => {
    const app = await boot({ running: true });
    const button = app.card(0).querySelector(target);
    button.focus();
    let prevented = false;
    await app.document.emit('keydown', { key: ' ', target: button, preventDefault() { prevented = true; } });
    await flush();
    assert.equal(prevented, true, '阻止按钮原生空格激活');
    assert.deepEqual(audioCommands(app).filter(c => c.command !== 'begin_playback_operation').map(c => c.command), ['stop_all']);
  });
}


test('校准应用成功但配置保存失败时明确说明未保存', async () => {
  let now = 0;
  const app = await boot({ running: true, now: () => now, onInvoke(command) {
    if (command === 'save_config') return Promise.reject(new Error('permission denied'));
  } });
  const calibration = app.document.getElementById('btnCalib').emit('click');
  await flush();
  await app.advanceTimers(400);
  for (let i = 0; i < 27; i++) { now += 200; await app.advanceTimers(200); }
  await calibration;
  assert.match(app.document.getElementById('calibHint').textContent, /已生效.*未保存/);
  assert.doesNotMatch(app.document.getElementById('calibHint').textContent, /设置已保存/);
  assert.match(app.document.getElementById('toast').textContent, /保存失败/);
});

// 校准读取送往虚拟麦克风的电平。变声启用时那是变声之后的信号，所以校准必须先把变声
// 暂停成原声，并且只采纳变声链确认回到 off 的读数。
function calibrationClock() {
  const clock = { now: 0 };
  clock.settle = async (app, promise) => {
    let done = false;
    promise.then(() => { done = true; }, () => { done = true; });
    for (let i = 0; i < 400 && !done; i++) {
      clock.now += 100;
      await app.advanceTimers(400);
      await app.pollStatus();
    }
    await promise;
  };
  return clock;
}

const calibrationCommands = app => app.calls.map(call => call.command)
  .filter(command => ['voice_calibration_begin', 'voice_calibration_end', 'stop_all',
    'set_gains', 'save_config'].includes(command));

test('校准先把变声暂停成原声再测量并在结束后解除', async () => {
  const clock = calibrationClock();
  let paused = false;
  const app = await boot({ running: true, now: () => clock.now, onInvoke(command) {
    if (command === 'voice_calibration_begin') { paused = true; return undefined; }
    if (command === 'voice_calibration_end') { paused = false; return undefined; }
    if (command === 'status') {
      return playbackStatus({
        // 变声启用时电平表读到的是变声之后的 -3 dBFS，原声才是 -20 dBFS。
        out_peak_db: paused ? -20 : -3,
        voice: voiceSnapshot({ enabled: true, phase: paused ? 'off' : 'wet', bypassed: paused }),
      });
    }
  } });
  app.calls.length = 0;
  await clock.settle(app, app.document.getElementById('btnCalib').emit('click'));

  const commands = calibrationCommands(app);
  assert.equal(commands[0], 'voice_calibration_begin', '测量前必须先暂停变声');
  assert.ok(commands.indexOf('voice_calibration_begin') < commands.indexOf('stop_all'));
  assert.ok(commands.indexOf('voice_calibration_begin') < commands.indexOf('set_gains'));
  assert.equal(commands.at(-1), 'voice_calibration_end', '结束后必须解除暂停');
  assert.equal(app.calls.filter(c => c.command === 'voice_calibration_begin').length, 1);
  assert.equal(app.calls.filter(c => c.command === 'voice_calibration_end').length, 1);
  const gain = app.calls.filter(c => c.command === 'set_gains').at(-1).args.micDb;
  assert.equal(gain, 5, '补偿必须按原声 -20 dBFS 计算，而不是变声后的 -3 dBFS');
  assert.match(app.document.getElementById('calibHint').textContent, /峰值 -20\.0 dBFS/);
  assert.equal(paused, false);
});

test('变声链尚未回到原声的读数不参与校准', async () => {
  const clock = calibrationClock();
  let paused = false;
  let polls = 0;
  const app = await boot({ running: true, now: () => clock.now, onInvoke(command) {
    if (command === 'voice_calibration_begin') { paused = true; polls = 0; return undefined; }
    if (command === 'voice_calibration_end') { paused = false; return undefined; }
    if (command === 'status') {
      if (!paused) {
        return playbackStatus({ out_peak_db: -3, voice: voiceSnapshot({ enabled: true, phase: 'wet' }) });
      }
      polls++;
      // 淡出阶段与采样中途离开 off 的读数都更响，必须被丢弃。
      const dry = polls > 4 && polls !== 12;
      return playbackStatus({
        out_peak_db: dry ? -20 : -1,
        voice: voiceSnapshot({ enabled: true, phase: dry ? 'off' : 'fading_out' }),
      });
    }
  } });
  app.calls.length = 0;
  await clock.settle(app, app.document.getElementById('btnCalib').emit('click'));

  const gain = app.calls.filter(c => c.command === 'set_gains').at(-1).args.micDb;
  assert.equal(gain, 5, '只有 phase 为 off 的读数（-20 dBFS）可以参与补偿');
  assert.match(app.document.getElementById('calibHint').textContent, /峰值 -20\.0 dBFS/);
  assert.equal(calibrationCommands(app).at(-1), 'voice_calibration_end');
});

test('变声始终没能切回原声时放弃校准且不改增益并解除暂停', async () => {
  const clock = calibrationClock();
  const app = await boot({ running: true, now: () => clock.now, onInvoke(command) {
    if (command === 'status') {
      return playbackStatus({ out_peak_db: -3, voice: voiceSnapshot({ enabled: true, phase: 'wet' }) });
    }
  } });
  app.calls.length = 0;
  await clock.settle(app, app.document.getElementById('btnCalib').emit('click'));

  assert.match(app.document.getElementById('calibHint').textContent, /变声没能切回原声/);
  assert.equal(app.document.getElementById('calibHint').className.includes('bad'), true);
  const gains = app.calls.filter(c => c.command === 'set_gains').map(c => c.args.micDb);
  assert.deepEqual(gains, [0, 0], '只有测量前归零与失败后恢复原增益，不能写入新补偿');
  assert.equal(app.calls.some(c => c.command === 'save_config'), false, '失败的校准不保存设置');
  assert.equal(calibrationCommands(app).at(-1), 'voice_calibration_end');
});

test('暂停变声失败时校准中止且不测量并仍发送结束', async () => {
  const clock = calibrationClock();
  const app = await boot({ running: true, now: () => clock.now, onInvoke(command) {
    if (command === 'voice_calibration_begin') return Promise.reject(new Error('host busy'));
  } });
  app.calls.length = 0;
  await clock.settle(app, app.document.getElementById('btnCalib').emit('click'));

  assert.match(app.document.getElementById('calibHint').textContent, /校准失败.*host busy/);
  assert.equal(app.calls.some(c => c.command === 'stop_all' || c.command === 'set_gains'), false,
    '无法保证原声时不能继续测量');
  assert.deepEqual(calibrationCommands(app), ['voice_calibration_begin', 'voice_calibration_end'],
    '宿主可能已部分生效，仍要发送结束');
});

test('结束校准失败时提示变声稍后自动恢复并释放校准按钮', async () => {
  const clock = calibrationClock();
  const app = await boot({ running: true, now: () => clock.now, onInvoke(command) {
    if (command === 'voice_calibration_end') return Promise.reject(new Error('ipc closed'));
  } });
  await clock.settle(app, app.document.getElementById('btnCalib').emit('click'));

  assert.match(app.document.getElementById('toast').textContent, /变声暂停未能解除/);
  assert.equal(app.document.getElementById('btnCalib').disabled, false);
  app.calls.length = 0;
  const again = app.document.getElementById('btnCalib').emit('click');
  await clock.settle(app, again);
  assert.equal(app.calls.filter(c => c.command === 'voice_calibration_begin').length, 1,
    '失败后可以再次校准');
});

test('校准进行中再次点击不会重复暂停变声', async () => {
  const clock = calibrationClock();
  const app = await boot({ running: true, now: () => clock.now });
  app.calls.length = 0;
  const first = app.document.getElementById('btnCalib').emit('click');
  await flush();
  await app.document.getElementById('btnCalib').emit('click');
  await clock.settle(app, first);
  assert.equal(app.calls.filter(c => c.command === 'voice_calibration_begin').length, 1);
  assert.equal(app.calls.filter(c => c.command === 'voice_calibration_end').length, 1);
});

test('播放按钮长按空格仅停止一次并阻止重复原生激活', async () => {
  const app = await boot({ running: true });
  const button = app.card(0).querySelector('.play-trigger');
  let prevented = 0;
  for (const repeat of [false, true, true]) {
    await app.document.emit('keydown', { key: ' ', target: button, repeat, preventDefault() { prevented++; } });
  }
  await flush();
  assert.equal(prevented, 3);
  assert.equal(app.calls.filter(call => call.command === 'stop_all').length, 1);
});

for (const dark of [false, true]) {
  test(`未选择主题时跟随系统${dark ? '深色' : '浅色'}并响应系统变化`, async () => {
    const app = await boot({ dark });
    const select = app.document.getElementById('selTheme');
    assert.ok(select, '更多功能中应有明确主题入口');
    assert.equal(select.value, 'system');
    assert.equal(app.document.documentElement.dataset.theme, dark ? 'dark' : 'light');
    app.systemTheme(!dark);
    assert.equal(app.document.documentElement.dataset.theme, dark ? 'light' : 'dark');
    assert.equal(app.themeStorage.size, 0, '系统变化不改写用户偏好');
  });
}

for (const theme of ['light', 'dark']) {
  test(`手动${theme}立即生效并恢复保存偏好且系统改变不覆盖`, async () => {
    const app = await boot({ running: true, dark: theme === 'light' });
    const select = app.document.getElementById('selTheme');
    assert.ok(select);
    app.calls.length = 0;
    select.value = theme;
    await select.emit('change');
    assert.equal(app.document.documentElement.dataset.theme, theme);
    assert.equal(app.themeStorage.get('mixer.ui.theme'), theme);
    app.systemTheme(theme === 'light');
    assert.equal(app.document.documentElement.dataset.theme, theme);
    assert.deepEqual(app.calls, [], '主题切换不发送音频或配置 IPC');
    const reopened = await boot({ storedTheme: app.themeStorage.get('mixer.ui.theme'), dark: theme === 'light' });
    assert.equal(reopened.document.getElementById('selTheme').value, theme);
    assert.equal(reopened.document.documentElement.dataset.theme, theme);
  });
}

test('手动主题切回跟随系统后使用当前系统外观并持续同步', async () => {
  const app = await boot({ storedTheme: 'light', dark: true });
  const select = app.document.getElementById('selTheme');
  assert.ok(select);
  select.value = 'system'; await select.emit('change');
  assert.equal(app.document.documentElement.dataset.theme, 'dark');
  app.systemTheme(false);
  assert.equal(app.document.documentElement.dataset.theme, 'light');
  assert.equal(app.themeStorage.get('mixer.ui.theme'), 'system');
});

for (const options of [{storedTheme: 'invalid'}, {storageFailure: 'read'}]) {
  test(`主题${options.storedTheme ? '存储值无效' : '存储不可读取'}时安全回退系统且界面正常加载`, async () => {
    const app = await boot({ ...options, dark: true });
    assert.equal(app.document.documentElement.dataset.theme, 'dark');
    assert.equal(app.document.getElementById('selTheme')?.value, 'system');
  });
}

test('主题存储写入失败仍即时切换并明确提示未保存', async () => {
  const app = await boot({ dark: true, storageFailure: 'write' });
  const select = app.document.getElementById('selTheme');
  assert.ok(select);
  select.value = 'light'; await select.emit('change');
  assert.equal(app.document.documentElement.dataset.theme, 'light');
  assert.equal(select.value, 'light');
  assert.match(app.document.getElementById('themeHelp').textContent, /未能保存/);
  assert.match(app.document.getElementById('toast').textContent, /未能保存/);
});

// ── 人声效果侧栏与基础面板 ───────────────────────────────────

test('人声侧栏渲染启用预设与编辑入口', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'voice_status') return voiceSnapshot({ enabled: true });
    if (command === 'voice_presets') return voicePresetList;
  } });
  const enabled = app.document.getElementById('voiceEnabled');
  const preset = app.document.getElementById('voicePreset');
  const preview = app.document.getElementById('voicePreview');
  const edit = app.document.getElementById('voiceEdit');
  assert.ok(enabled, '侧栏应有启用开关');
  assert.ok(preset, '侧栏应有预设下拉');
  assert.ok(preview, '侧栏应有试听按钮');
  assert.ok(edit, '侧栏应有编辑入口');
  assert.equal(enabled.getAttribute('aria-label'), '启用变声');
  assert.equal(preset.getAttribute('aria-label'), '当前变声预设');
  assert.equal(preview.textContent, '试听 3 秒');
  assert.equal(edit.textContent, '编辑');
  assert.equal(edit.getAttribute('aria-haspopup'), 'dialog');
  assert.deepEqual(preset.options.map(option => option.value), ['builtin:female', 'builtin:male']);
  assert.equal(preset.value, 'builtin:female');
  assert.equal(enabled.checked, true);
  const micBlock = app.document.getElementById('inMicGain').closest('.gain-control');
  const sfxBlock = app.document.getElementById('inSfxGain').closest('.gain-control');
  const voiceBlock = app.document.getElementById('voiceBlock');
  assert.ok(voiceBlock, '侧栏应有人声效果分组');
  const siblings = voiceBlock.parentElement.children;
  assert.ok(siblings.indexOf(micBlock) < siblings.indexOf(voiceBlock),
    '人声效果应排在麦克风音量之后');
  assert.ok(siblings.indexOf(voiceBlock) < siblings.indexOf(sfxBlock),
    '人声效果应排在音效总音量之前');
  assert.equal(app.calls.filter(call => call.command === 'voice_status').length, 1,
    '启动时应读取一次变声状态');
  assert.equal(app.calls.filter(call => call.command === 'voice_presets').length, 1,
    '启动时应读取一次预设列表');
});

test('启用开关发送变声命令', async () => {
  const app = await boot({ onInvoke(command) {
    const voice = voiceFixture(command);
    if (voice !== undefined) return voice;
    if (command === 'voice_set_enabled') return voiceSnapshot({ enabled: true, generation: 2 });
  } });
  const toggle = app.document.getElementById('voiceEnabled');
  toggle.checked = true;
  await toggle.emit('change');
  await flush();
  assert.deepEqual(app.calls.filter(call => call.command === 'voice_set_enabled'), [
    { command: 'voice_set_enabled', args: { enabled: true } },
  ], '启用开关只能调用变声命令');
  assert.equal(app.calls.some(call => call.command === 'save_config'), false,
    '变声启用状态不能混入全量配置保存');
  assert.equal(toggle.checked, true);
});

test('人声效果热键输入只发送变声热键命令', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'load_config') {
      return { items, dir: '', hotkeys: ',F2',
        voice_hotkeys: { bypass: 'F9', next: 'F10', prev: 'F8' } };
    }
    const voice = voiceFixture(command);
    if (voice !== undefined) return voice;
    if (command === 'voice_set_hotkeys') return voiceSnapshot({ generation: 2 });
  } });
  const bypass = app.document.getElementById('voiceKeyBypass');
  const next = app.document.getElementById('voiceKeyNext');
  const prev = app.document.getElementById('voiceKeyPrev');
  assert.ok(bypass && next && prev, '设置面板应有人声效果热键组');
  assert.equal(bypass.value, 'F9');
  assert.equal(next.value, 'F10');
  assert.equal(prev.value, 'F8');

  const before = app.calls.length;
  bypass.value = 'F11';
  await bypass.emit('change');
  await flush();
  const later = app.calls.slice(before);
  assert.deepEqual(later.filter(call => call.command === 'voice_set_hotkeys'), [
    { command: 'voice_set_hotkeys', args: { bypass: 'F11', next: 'F10', prev: 'F8' } },
  ], '输入变化只发送人声热键命令');
  assert.equal(later.some(call => call.command === 'save_config'), false,
    '人声热键不能混入全量配置保存');
  assert.equal(later.some(call => ['start', 'stop', 'release'].includes(call.command)), false,
    '人声热键不能重启音频会话');
});

test('人声热键提示在运行与待机时都使用普通提示', async () => {
  let status = playbackStatus({
    voice_notice: '变声未开启，旁路热键不生效', voice_notice_generation: 1,
  });
  const app = await boot({ onInvoke(command) {
    if (command === 'status') return status;
    return voiceFixture(command);
  } });
  const toast = app.document.getElementById('toast');
  const notice = app.document.getElementById('notice');

  await app.pollStatus();
  assert.equal(toast.textContent, '变声未开启，旁路热键不生效', '运行中提示用普通 toast');
  assert.equal(notice.textContent, '', '不能写入启动错误提示区');

  status = playbackStatus({
    running: false, passthrough: true,
    voice_notice: '音频未启动，旁路热键不生效', voice_notice_generation: 2,
  });
  await app.pollStatus();
  assert.equal(toast.textContent, '音频未启动，旁路热键不生效', '待机提示同样用普通 toast');
  assert.equal(notice.textContent, '', '待机也不能显示红色启动错误');
});

test('同一代次人声提示只提示一次且恢复后清除', async () => {
  let status = playbackStatus({ voice_notice: '音频未启动', voice_notice_generation: 1 });
  const app = await boot({ onInvoke(command) {
    if (command === 'status') return status;
    return voiceFixture(command);
  } });
  const toast = app.document.getElementById('toast');

  await app.pollStatus();
  assert.equal(toast.textContent, '音频未启动');
  await app.advanceTimers(3000);
  assert.equal(toast.textContent, '');
  await app.pollStatus();
  assert.equal(toast.textContent, '', '同一代次不能重复提示');

  status = playbackStatus({ voice_notice: null, voice_notice_generation: 2 });
  await app.pollStatus();
  assert.equal(toast.textContent, '', '恢复后清除提示');

  status = playbackStatus({ voice_notice: '新的提示', voice_notice_generation: 3 });
  await app.pollStatus();
  assert.equal(toast.textContent, '新的提示', '新一代次仍可提示');
});

test('预设选择不自动启用', async () => {
  const app = await boot({ onInvoke(command) {
    const voice = voiceFixture(command);
    if (voice !== undefined) return voice;
    if (command === 'voice_select_preset') {
      return voiceSnapshot({ preset_id: 'builtin:male', preset_name: '男声', generation: 2 });
    }
  } });
  const toggle = app.document.getElementById('voiceEnabled');
  const preset = app.document.getElementById('voicePreset');
  assert.equal(toggle.checked, false);
  preset.value = 'builtin:male';
  await preset.emit('change');
  await flush();
  assert.deepEqual(app.calls.filter(call => call.command === 'voice_select_preset'), [
    { command: 'voice_select_preset', args: { id: 'builtin:male' } },
  ]);
  assert.equal(app.calls.some(call => call.command === 'voice_set_enabled'), false,
    '选择预设不能隐式开启变声');
  assert.equal(app.calls.some(call => call.command === 'save_config'), false,
    '选择预设不能写入全量配置');
  assert.equal(toggle.checked, false);
  assert.match(app.document.getElementById('voiceState').textContent, /男声.*已关闭/);
});

test('音高共振峰输入合并发送', async () => {
  const app = await boot({ onInvoke: voiceFixture });
  await app.document.getElementById('voiceEdit').emit('click');
  const pitch = app.document.getElementById('pitch');
  const formant = app.document.getElementById('formant');
  assert.equal(pitch.getAttribute('min'), '-24');
  assert.equal(pitch.getAttribute('max'), '24');
  assert.equal(formant.getAttribute('min'), '-12');
  assert.equal(formant.getAttribute('max'), '12');
  for (const value of [3, 5, 7]) {
    pitch.value = String(value);
    await pitch.emit('input');
  }
  formant.value = '4';
  await formant.emit('input');
  assert.equal(app.calls.filter(call => call.command === 'voice_set_params').length, 0,
    '输入合并窗口内不应逐次发送');
  assert.equal(app.document.getElementById('pitchOut').textContent, '+7 半音');
  assert.equal(app.document.getElementById('formantOut').textContent, '+4 半音');
  await app.advanceTimers(50);
  await flush();
  const snapshots = app.calls.filter(call => call.command === 'voice_set_params');
  assert.equal(snapshots.length, 1, '一次合并只发送一个完整参数快照');
  assert.deepEqual(Object.keys(snapshots[0].args.params).sort(), Object.keys(voiceParams()).sort(),
    '参数快照必须包含完整 VoiceParams 字段');
  assert.equal(snapshots[0].args.params.pitch_semitones, 7);
  assert.equal(snapshots[0].args.params.formant_semitones, 4);
  pitch.value = '-2';
  await pitch.emit('input');
  await app.advanceTimers(50);
  await flush();
  const later = app.calls.filter(call => call.command === 'voice_set_params');
  assert.equal(later.length, 2);
  assert.equal(later[1].args.params.pitch_semitones, -2);
  assert.equal(later[1].args.params.formant_semitones, 4, '后续快照仍保持完整参数');
});

test('人声面板焦点与 Escape 恢复', async () => {
  const app = await boot({ onInvoke: voiceFixture });
  const edit = app.document.getElementById('voiceEdit');
  const overlay = app.document.getElementById('voiceOverlay');
  edit.focus();
  await edit.emit('click');
  assert.equal(overlay.classList.contains('hidden'), false);
  assert.equal(app.document.getElementById('appShell').inert, true);
  assert.equal(app.document.activeElement, app.document.getElementById('voiceClose'));
  assert.equal(app.calls.filter(call => call.command === 'voice_presets').length, 2,
    '打开面板时应重新读取预设');
  const first = app.document.getElementById('voiceClose');
  const last = app.document.getElementById('voiceDone');
  first.focus();
  await app.document.emit('keydown', { key: 'Tab', shiftKey: true, target: first });
  assert.equal(app.document.activeElement, last, 'Shift+Tab 从首个控件回到末个控件');
  await app.document.emit('keydown', { key: 'Tab', target: last });
  assert.equal(app.document.activeElement, first, 'Tab 从末个控件回到首个控件');
  await app.document.emit('keydown', { key: 'Escape', target: first });
  assert.equal(overlay.classList.contains('hidden'), true);
  assert.equal(app.document.getElementById('appShell').inert, false);
  assert.equal(app.document.activeElement, edit, '关闭后恢复触发按钮焦点');
  await edit.emit('click');
  await overlay.emit('click', { target: overlay });
  assert.equal(overlay.classList.contains('hidden'), true, '点击遮罩空白处应关闭面板');
  assert.equal(app.document.activeElement, edit);
});

test('旧变声状态回复不覆盖新选择', async () => {
  let holdNextStatus = false;
  let finishOlderStatus;
  const initial = voiceSnapshot();
  const app = await boot({ onInvoke(command) {
    if (command === 'voice_status') return initial;
    if (command === 'voice_presets') return voicePresetList;
    if (command === 'status') {
      if (holdNextStatus) {
        holdNextStatus = false;
        return new Promise(resolve => { finishOlderStatus = resolve; });
      }
      return playbackStatus({ voice: initial });
    }
    if (command === 'voice_select_preset') {
      return voiceSnapshot({ preset_id: 'builtin:male', preset_name: '男声', generation: 2 });
    }
  } });
  await app.document.getElementById('voiceEdit').emit('click');
  const pitch = app.document.getElementById('pitch');
  pitch.value = '6';
  await pitch.emit('input');
  holdNextStatus = true;
  await app.pollStatus();
  assert.equal(typeof finishOlderStatus, 'function', '应有一份旧状态回复仍在途中');
  const preset = app.document.getElementById('voicePreset');
  preset.value = 'builtin:male';
  await preset.emit('change');
  await app.advanceTimers(50);
  await flush();
  finishOlderStatus(playbackStatus({ voice: initial }));
  await flush();
  await app.pollStatus();
  assert.equal(preset.value, 'builtin:male', '旧轮询不得回退用户选择的预设');
  assert.equal(pitch.value, '6', '旧轮询不得回退正在编辑的滑杆');
  assert.match(app.document.getElementById('voiceCurrentPreset').textContent, /男声/);
});

test('音频未启动时试听显示原因', async () => {
  const idle = await boot({ onInvoke: voiceFixture });
  const idlePreview = idle.document.getElementById('voicePreview');
  assert.equal(idlePreview.disabled, true);
  assert.equal(idlePreview.textContent, '试听 3 秒');
  assert.match(idle.document.getElementById('previewHint').textContent, /音频未启动/);

  const busy = await boot({ running: true, onInvoke(command) {
    if (command === 'status') {
      return playbackStatus({ preview_playing: true, preview_sound_id: 'sound-A',
        preview_playhead_secs: 0.5, preview_duration_secs: 2, voice: voiceSnapshot() });
    }
    return voiceFixture(command);
  } });
  const busyPreview = busy.document.getElementById('voicePreview');
  assert.equal(busyPreview.disabled, true);
  assert.match(busy.document.getElementById('previewHint').textContent, /本机试听占用中/);

  const ready = await boot({ running: true, onInvoke(command) {
    if (command === 'status') return playbackStatus({ voice: voiceSnapshot() });
    return voiceFixture(command);
  } });
  const readyPreview = ready.document.getElementById('voicePreview');
  assert.equal(readyPreview.disabled, false);
  assert.match(ready.document.getElementById('previewHint').textContent, /试听只在本机回放/);
});

test('临时旁路状态显示为独立提示', async () => {
  const snapshot = voiceSnapshot({ enabled: true, bypassed: true,
    preset_id: 'builtin:male', preset_name: '男声' });
  const app = await boot({ running: true, onInvoke(command) {
    if (command === 'voice_status') return snapshot;
    if (command === 'voice_presets') return voicePresetList;
    if (command === 'status') return playbackStatus({ voice: snapshot });
  } });
  const toggle = app.document.getElementById('voiceEnabled');
  const badge = app.document.getElementById('bypassBadge');
  assert.equal(toggle.checked, true, '临时旁路不能把启用开关改成关闭');
  assert.equal(badge.classList.contains('hidden'), false);
  assert.equal(badge.textContent, '临时旁路');
  const status = app.document.getElementById('voiceStatus').textContent;
  assert.match(status, /男声/);
  assert.match(status, /临时旁路/);
  await app.document.getElementById('voiceEdit').emit('click');
  const modalBadge = app.document.getElementById('modalBypassBadge');
  assert.equal(modalBadge.classList.contains('hidden'), false);
  assert.equal(modalBadge.textContent, '临时旁路');
  assert.equal(toggle.checked, true);
});

test('人声变声状态进入启动参数且配置保存不含变声字段', async () => {
  const params = voiceParams({ pitch_semitones: 5, formant_semitones: 2 });
  const app = await boot({ onInvoke(command) {
    if (command === 'voice_status') {
      return voiceSnapshot({ enabled: true, preset_id: 'builtin:male',
        preset_name: '男声', params });
    }
    if (command === 'voice_presets') return voicePresetList;
  } });
  await app.document.getElementById('btnToggle').emit('click');
  const start = app.calls.filter(call => call.command === 'start').at(-1);
  assert.ok(start, '启动命令应携带变声状态');
  assert.deepEqual(start.args.cfg.voice, { enabled: true, params, block_ms: 10 });
  const saved = app.calls.filter(call => call.command === 'save_config').at(-1);
  assert.ok(saved, '启动前应保存普通配置');
  assert.equal('voice' in saved.args, false, '全量配置保存不能包含变声字段');
});

// ── 人声效果完整面板 ─────────────────────────────────────────

const hidden = element => element.classList.contains('hidden');
const paramsCalls = app => app.calls.filter(call => call.command === 'voice_set_params');

test('效果开关与数值分别发送', async () => {
  const app = await boot({ onInvoke(command, args) {
    const voice = voiceFixture(command);
    if (voice !== undefined) return voice;
    if (command === 'voice_set_params') {
      return voiceSnapshot({ params: args.params, modified: true, generation: 2 });
    }
  } });
  await app.document.getElementById('voiceEdit').emit('click');
  const eqOn = app.document.getElementById('eqMidOn');
  const eq = app.document.getElementById('eqMid');
  const gateOn = app.document.getElementById('gateOn');
  const gate = app.document.getElementById('gate');
  assert.equal(eqOn.checked, false);
  assert.equal(eq.value, '0');

  // 数值保持 0 dB，仅开关状态不同，快照必须可区分。
  eq.value = '0';
  await eq.emit('input');
  await app.advanceTimers(50);
  await flushBoot();
  eqOn.checked = true;
  await eqOn.emit('change');
  await app.advanceTimers(50);
  await flushBoot();

  gate.value = '-20';
  await gate.emit('input');
  gateOn.checked = true;
  await gateOn.emit('change');
  await app.advanceTimers(50);
  await flushBoot();

  const snapshots = paramsCalls(app);
  assert.equal(snapshots.length, 3, '每组效果修改合并为一个完整快照');
  assert.equal(snapshots[0].args.params.eq_mid_enabled, false);
  assert.equal(snapshots[0].args.params.eq_mid_db, 0);
  assert.equal(snapshots[1].args.params.eq_mid_enabled, true);
  assert.equal(snapshots[1].args.params.eq_mid_db, 0);
  assert.notDeepEqual(snapshots[0].args.params, snapshots[1].args.params,
    '0 dB 启用与 0 dB 关闭不能发送相同参数');
  assert.equal(snapshots[2].args.params.gate_enabled, true);
  assert.equal(snapshots[2].args.params.gate_db, -20);
  assert.equal(Object.keys(snapshots[0].args.params).length, Object.keys(voiceParams()).length);
});

test('另存为用户预设显示名称与当前状态', async () => {
  let saved = false;
  const userPreset = userPresetFixture();
  const app = await boot({ onInvoke(command) {
    if (command === 'voice_status') return voiceSnapshot();
    if (command === 'voice_presets') {
      return saved ? [...voicePresetList, userPreset] : voicePresetList;
    }
    if (command === 'voice_save_as_user_preset') {
      saved = true;
      return voiceSnapshot({ preset_id: 'user:live', preset_name: '直播女声',
        params: userPreset.params, modified: false, generation: 2 });
    }
  } });
  await app.document.getElementById('voiceEdit').emit('click');
  await app.document.getElementById('voiceSaveAs').emit('click');
  const editor = app.document.getElementById('voiceNameEditor');
  assert.equal(hidden(editor), false, '另存前应显示名称输入');
  const name = app.document.getElementById('voicePresetName');
  name.value = '直播女声';
  await app.document.getElementById('voiceNameConfirm').emit('click');
  await flushBoot();

  assert.deepEqual(app.calls.filter(call => call.command === 'voice_save_as_user_preset'), [
    { command: 'voice_save_as_user_preset', args: { name: '直播女声' } },
  ], '另存命令只传名称');
  assert.equal(hidden(editor), true, '保存后收起名称输入');
  assert.equal(app.document.getElementById('voicePreset').value, 'user:live');
  assert.equal(app.document.getElementById('voiceCurrentPreset').textContent, '直播女声');
  assert.match(app.document.getElementById('voiceState').textContent, /直播女声.*已关闭/);
  assert.equal(app.document.getElementById('voicePreset').options
    .some(option => option.value === 'user:live'), true, '保存后预设下拉应包含新预设');
  assert.ok(app.calls.filter(call => call.command === 'voice_presets').length >= 3,
    '保存成功后应刷新预设列表');
});

test('内置预设只显示另存操作', async () => {
  const builtin = await boot({ onInvoke: voiceFixture });
  await builtin.document.getElementById('voiceEdit').emit('click');
  assert.equal(hidden(builtin.document.getElementById('voiceSaveAs')), false);
  assert.equal(hidden(builtin.document.getElementById('voiceSaveChanges')), true);
  assert.equal(hidden(builtin.document.getElementById('voiceRename')), true);
  assert.equal(hidden(builtin.document.getElementById('voiceDelete')), true);

  const userPreset = userPresetFixture();
  const userSnapshot = voiceSnapshot({ enabled: true, preset_id: 'user:live',
    preset_name: '直播女声', modified: true, params: userPreset.params });
  const user = await boot({ onInvoke(command) {
    if (command === 'voice_status') return userSnapshot;
    if (command === 'voice_presets') return [...voicePresetList, userPreset];
  } });
  await user.document.getElementById('voiceEdit').emit('click');
  for (const id of ['voiceSaveChanges', 'voiceRename', 'voiceDelete']) {
    assert.equal(hidden(user.document.getElementById(id)), false, `${id} 应在用户预设下可用`);
  }
  assert.equal(user.document.getElementById('voiceSaveChanges').disabled, false);
});

test('用户预设保存修改与重命名发送对应命令', async () => {
  const userPreset = userPresetFixture();
  const app = await boot({ onInvoke(command, args) {
    if (command === 'voice_status') {
      return voiceSnapshot({ enabled: true, preset_id: 'user:live',
        preset_name: userPreset.name, modified: true, params: userPreset.params });
    }
    if (command === 'voice_presets') return [...voicePresetList, userPreset];
    if (command === 'voice_save_preset') {
      return voiceSnapshot({ enabled: true, preset_id: 'user:live',
        preset_name: userPreset.name, modified: false, params: userPreset.params,
        generation: 2 });
    }
    if (command === 'voice_rename_preset') {
      userPreset.name = args.name;
      return voiceSnapshot({ enabled: true, preset_id: 'user:live',
        preset_name: args.name, modified: false, params: userPreset.params,
        generation: 3 });
    }
  } });
  await app.document.getElementById('voiceEdit').emit('click');
  await app.document.getElementById('voiceSaveChanges').emit('click');
  await flushBoot();
  assert.deepEqual(app.calls.filter(call => call.command === 'voice_save_preset'), [
    { command: 'voice_save_preset', args: undefined },
  ], '保存修改不能带入额外参数');
  assert.equal(hidden(app.document.getElementById('dirtyBadge')), true,
    '保存成功后应清除修改状态');

  await app.document.getElementById('voiceRename').emit('click');
  const editor = app.document.getElementById('voiceNameEditor');
  assert.equal(hidden(editor), false);
  app.document.getElementById('voicePresetName').value = '直播人声';
  await app.document.getElementById('voiceNameConfirm').emit('click');
  await flushBoot();
  assert.deepEqual(app.calls.filter(call => call.command === 'voice_rename_preset'), [
    { command: 'voice_rename_preset', args: { id: 'user:live', name: '直播人声' } },
  ], '重命名只改名称，不改预设 ID');
  assert.equal(app.document.getElementById('voiceCurrentPreset').textContent, '直播人声');
  assert.equal(app.document.getElementById('voicePreset').value, 'user:live');
});

test('保存失败显示未保存并保留面板', async () => {
  const userPreset = userPresetFixture();
  const initial = voiceSnapshot({ enabled: true, preset_id: 'user:live',
    preset_name: '直播女声', params: userPreset.params });
  const failed = voiceSnapshot({ enabled: true, preset_id: 'user:live',
    preset_name: '直播女声', modified: true, params: userPreset.params,
    save_error: '保存变声配置失败: 磁盘已满', generation: 2 });
  const app = await boot({ onInvoke(command) {
    if (command === 'voice_status') return initial;
    if (command === 'voice_presets') return [...voicePresetList, userPreset];
    if (command === 'status') return playbackStatus({ voice: failed });
  } });
  const overlay = app.document.getElementById('voiceOverlay');
  await app.document.getElementById('voiceEdit').emit('click');
  assert.equal(hidden(overlay), false);
  await app.pollStatus();

  assert.equal(hidden(overlay), false, '保存失败不能关闭面板');
  const saveState = app.document.getElementById('voiceSaveState');
  assert.equal(hidden(saveState), false, '保存失败应显示未保存状态');
  assert.match(saveState.textContent, /未保存/);
  assert.match(saveState.textContent, /磁盘已满/);
  assert.equal(hidden(app.document.getElementById('dirtyBadge')), false,
    '保存失败后仍应显示修改状态');
  assert.doesNotMatch(overlay.textContent, /已保存/, '保存失败不能显示已保存');
});

test('草稿入口在切换预设后出现并可恢复', async () => {
  const app = await boot({ onInvoke(command) {
    if (command === 'voice_status') return voiceSnapshot();
    if (command === 'voice_presets') return voicePresetList;
    if (command === 'voice_select_preset') {
      return voiceSnapshot({ preset_id: 'builtin:male', preset_name: '男声',
        params: voiceParams({ pitch_semitones: -4, formant_semitones: -2 }),
        draft_available: true, generation: 2 });
    }
    if (command === 'voice_restore_draft') {
      return voiceSnapshot({ preset_id: 'builtin:female', preset_name: '女声',
        params: voiceParams({ pitch_semitones: 7, formant_semitones: 5 }),
        modified: true, draft_available: false, generation: 3 });
    }
  } });
  await app.document.getElementById('voiceEdit').emit('click');
  const restore = app.document.getElementById('voiceRestoreDraft');
  assert.equal(hidden(restore), true, '没有草稿时不显示恢复入口');

  const preset = app.document.getElementById('voicePreset');
  preset.value = 'builtin:male';
  await preset.emit('change');
  await flushBoot();
  assert.equal(hidden(restore), false, '切换预设留下草稿后应显示恢复入口');
  assert.equal(restore.textContent, '恢复未保存修改');

  await restore.emit('click');
  await flushBoot();
  const restoreCalls = app.calls.filter(call => call.command === 'voice_restore_draft');
  assert.equal(restoreCalls.length, 1, '恢复草稿应调用宿主命令');
  assert.equal(restoreCalls[0].args, undefined);
  assert.equal(app.document.getElementById('pitch').value, '7');
  assert.equal(preset.value, 'builtin:female');
  assert.equal(app.document.getElementById('voiceCurrentPreset').textContent, '女声');
  assert.equal(hidden(restore), true, '恢复后草稿入口应消失');
});

test('外部切预设刷新面板但不覆盖拖动中的滑杆', async () => {
  let externalVoice = null;
  let holdStatus = false;
  let releaseHeld;
  const app = await boot({ onInvoke(command, args) {
    if (command === 'voice_status') return externalVoice || voiceSnapshot();
    if (command === 'voice_presets') return voicePresetList;
    if (command === 'status') {
      if (holdStatus) {
        holdStatus = false;
        return new Promise(resolve => { releaseHeld = resolve; });
      }
      return playbackStatus({ voice: externalVoice || voiceSnapshot() });
    }
    if (command === 'voice_set_params') {
      return voiceSnapshot({ preset_id: 'builtin:male', preset_name: '男声',
        params: args.params, modified: true, generation: 6 });
    }
  } });
  await app.document.getElementById('voiceEdit').emit('click');
  const pitch = app.document.getElementById('pitch');
  const preset = app.document.getElementById('voicePreset');

  holdStatus = true;
  await app.pollStatus();
  assert.equal(typeof releaseHeld, 'function', '应有一份旧状态回复仍在途中');

  pitch.value = '9';
  await pitch.emit('input');
  externalVoice = voiceSnapshot({ preset_id: 'builtin:male', preset_name: '男声',
    params: voiceParams({ pitch_semitones: -4, formant_semitones: -2,
      eq_low_enabled: true, eq_low_db: 3 }),
    generation: 5 });
  await app.pollStatus();
  assert.equal(preset.value, 'builtin:male', '外部切预设应刷新面板');
  assert.match(app.document.getElementById('voiceCurrentPreset').textContent, /男声/);
  assert.equal(pitch.value, '9', '拖动中的滑杆不能被外部快照覆盖');

  releaseHeld(playbackStatus({ voice: voiceSnapshot({ generation: 1 }) }));
  await flushBoot();
  assert.equal(preset.value, 'builtin:male', '旧回复不得回退预设');
  assert.equal(pitch.value, '9', '旧回复不得回退滑杆');

  await app.advanceTimers(50);
  await flushBoot();
  const sent = paramsCalls(app).at(-1);
  assert.ok(sent, '拖动结束后应发送参数');
  assert.equal(sent.args.params.pitch_semitones, 9, '本地拖动值必须保留');
  assert.equal(sent.args.params.formant_semitones, -2, '外部参数应合入完整快照');
  assert.equal(sent.args.params.eq_low_enabled, true);
});

test('删除当前用户预设回退女声并关闭', async () => {
  let deleted = false;
  const userPreset = userPresetFixture();
  const userSnapshot = voiceSnapshot({ enabled: true, preset_id: 'user:live',
    preset_name: '直播女声', params: userPreset.params });
  const app = await boot({ onInvoke(command) {
    if (command === 'voice_presets') {
      return deleted ? voicePresetList : [...voicePresetList, userPreset];
    }
    if (command === 'voice_status') return deleted
      ? voiceSnapshot({ enabled: false, params: voiceParams(), generation: 2 })
      : userSnapshot;
    if (command === 'voice_delete_preset') {
      deleted = true;
      return voiceSnapshot({ enabled: false, preset_id: 'builtin:female',
        preset_name: '女声', params: voiceParams(), generation: 2 });
    }
  } });
  await app.document.getElementById('voiceEdit').emit('click');
  await app.document.getElementById('voiceDelete').emit('click');
  await flushBoot();

  assert.deepEqual(app.calls.filter(call => call.command === 'voice_delete_preset'), [
    { command: 'voice_delete_preset', args: { id: 'user:live' } },
  ]);
  assert.equal(app.document.getElementById('voiceEnabled').checked, false,
    '删除当前预设后启用开关应关闭');
  assert.equal(app.document.getElementById('voicePreset').value, 'builtin:female');
  assert.equal(app.document.getElementById('voicePreset').options
    .some(option => option.value === 'user:live'), false, '被删除的预设应从下拉移除');
  assert.match(app.document.getElementById('voiceStatus').textContent, /女声.*已关闭/);
  assert.match(app.document.getElementById('toast').textContent, /已删除/);
  assert.match(app.document.getElementById('toast').textContent, /关闭/);
});

test('效果控件具备可访问名称与数值', async () => {
  const app = await boot({ onInvoke: voiceFixture });
  await app.document.getElementById('voiceEdit').emit('click');
  const cases = [
    ['gateOn', 'gate', 'gateOut', '噪声门', '噪声门阈值', '-60', '0'],
    ['eqLowOn', 'eqLow', 'eqLowOut', '低频 EQ', '低频 EQ 增益', '-12', '12'],
    ['eqMidOn', 'eqMid', 'eqMidOut', '中频 EQ', '中频 EQ 增益', '-12', '12'],
    ['eqHighOn', 'eqHigh', 'eqHighOut', '高频 EQ', '高频 EQ 增益', '-12', '12'],
    ['driveOn', 'drive', 'driveOut', '软削波', '软削波量', '0', '1'],
    ['robotOn', 'robotHz', 'robotHzOut', '环形调制', '环形调制频率', '20', '1000'],
  ];
  for (const [toggleId, sliderId, outputId, label, sliderName, min, max] of cases) {
    const toggle = app.document.getElementById(toggleId);
    const slider = app.document.getElementById(sliderId);
    const output = app.document.getElementById(outputId);
    assert.equal(toggle.getAttribute('aria-label'), label);
    assert.match(toggle.parentElement.textContent, new RegExp(label));
    assert.equal(slider.getAttribute('aria-label'), sliderName);
    assert.equal(slider.getAttribute('min'), min);
    assert.equal(slider.getAttribute('max'), max);
    assert.equal(slider.getAttribute('aria-describedby'), outputId);
    assert.equal(output.tagName, 'OUTPUT');
    assert.equal(output.getAttribute('for'), sliderId);
    assert.ok(output.textContent.length > 0, `${outputId} 应显示只读数值`);
  }
  const mix = app.document.getElementById('robotMix');
  assert.equal(mix.getAttribute('aria-label'), '环形调制混合');
  assert.equal(mix.getAttribute('min'), '0');
  assert.equal(mix.getAttribute('max'), '1');
  assert.equal(app.document.getElementById('robotMixOut').textContent, '0.80');
  assert.equal(app.document.getElementById('gateOut').textContent, '−45 dB');
  assert.equal(app.document.getElementById('robotHzOut').textContent, '50 Hz');
});

test('窄窗口面板内部滚动且完成按钮可见', async () => {
  const css = fs.readFileSync(path.join(root, 'style.css'), 'utf8');
  assert.match(css, /\.dialog\s*\{[^}]*max-height:\s*100%[^}]*overflow-y:\s*auto/s,
    '对话框应限制高度并在内部滚动');
  assert.match(css, /\.voice-dialog \.dialog-footer\s*\{[^}]*position:\s*sticky[^}]*bottom:\s*0/s,
    '完成按钮所在底部区域应固定可见');
  assert.match(css, /\.effect-row\s*\{[^}]*minmax\(0,\s*1fr\)/s,
    '效果行应允许滑杆收缩，避免横向溢出');
  assert.match(css, /\.effect-row[^{]*\{[^}]*min-width:\s*0/s,
    '效果控件应能收缩');

  const app = await boot({ onInvoke: voiceFixture });
  await app.document.getElementById('voiceEdit').emit('click');
  assert.equal(hidden(app.document.getElementById('voiceOverlay')), false);
  const done = app.document.getElementById('voiceDone');
  const dialog = app.document.querySelector('.voice-dialog');
  assert.ok(dialog, '人声面板应使用受限宽度的对话框');
  assert.equal(done.textContent, '完成');
  assert.equal(hidden(done), false);
  // Node 替身没有布局引擎；上面的样式契约保证滚动与底部固定，这里确认按钮属于该底部区域。
  const footer = done.closest('.dialog-footer');
  assert.ok(footer && footer.parentElement === dialog, '完成按钮应位于对话框底部操作区');
});

// ── 三秒人声试听按钮 ───────────────────────────────────────

const voicePreviewSnapshot = (state, patch = {}) => voiceSnapshot({
  preview: { state, elapsed_ms: 0, reason: null, ...patch },
});

test('试听空闲时点击发送启动并按回复更新状态', async () => {
  let starts = 0;
  const app = await boot({ running: true, onInvoke(command) {
    if (command === 'voice_preview_start') {
      starts++;
      return voicePreviewSnapshot('preparing', { generation: 2 });
    }
    if (command === 'voice_status') return voiceSnapshot();
    if (command === 'status') return playbackStatus({ voice: voiceSnapshot() });
    return voiceFixture(command);
  } });
  const button = app.document.getElementById('voicePreview');
  assert.equal(button.textContent, '试听 3 秒');
  assert.equal(button.disabled, false);

  await button.emit('click');
  await flush();
  assert.equal(starts, 1, '空闲点击应发送启动命令');
  assert.equal(button.textContent, '准备试听…');
  assert.equal(app.calls.filter(call => call.command === 'voice_preview_stop').length, 0);
});

test('试听录制中显示进度并点击立即发送停止', async () => {
  let stopped = 0;
  const recording = voicePreviewSnapshot('recording', { elapsed_ms: 2000 });
  const app = await boot({ running: true, onInvoke(command) {
    if (command === 'voice_preview_stop') {
      stopped++;
      return voiceSnapshot({ generation: 3 });
    }
    if (command === 'voice_status') return recording;
    if (command === 'status') return playbackStatus({ voice: recording });
    return voiceFixture(command);
  } });
  const button = app.document.getElementById('voicePreview');
  assert.equal(button.disabled, false, '录制中按钮必须可点击取消');
  assert.match(button.textContent, /录制中 0:02/);
  assert.match(button.textContent, /点击取消/);
  assert.match(app.document.getElementById('previewHint').textContent, /录制 3 秒/);

  await button.emit('click');
  await flush();
  assert.equal(stopped, 1, '第二次点击必须立即发送停止命令');
  assert.equal(button.textContent, '试听 3 秒');
  assert.equal(app.calls.filter(call => call.command === 'voice_preview_start').length, 0);
});

test('试听回放中点击发送停止且准备中可取消', async () => {
  const playing = voicePreviewSnapshot('playing');
  const playingApp = await boot({ running: true, onInvoke(command) {
    if (command === 'voice_preview_stop') return voiceSnapshot({ generation: 3 });
    if (command === 'voice_status') return playing;
    if (command === 'status') return playbackStatus({ voice: playing });
    return voiceFixture(command);
  } });
  const playingButton = playingApp.document.getElementById('voicePreview');
  assert.equal(playingButton.textContent, '▶ 回放中 · 点击停止');
  await playingButton.emit('click');
  await flush();
  assert.equal(playingApp.calls.filter(call => call.command === 'voice_preview_stop').length, 1);

  const preparing = voicePreviewSnapshot('preparing');
  const preparingApp = await boot({ running: true, onInvoke(command) {
    if (command === 'voice_preview_stop') return voiceSnapshot({ generation: 3 });
    if (command === 'voice_status') return preparing;
    if (command === 'status') return playbackStatus({ voice: preparing });
    return voiceFixture(command);
  } });
  const preparingButton = preparingApp.document.getElementById('voicePreview');
  assert.equal(preparingButton.textContent, '准备试听…');
  await preparingButton.emit('click');
  await flush();
  assert.equal(preparingApp.calls.filter(call => call.command === 'voice_preview_stop').length, 1);
});

test('试听启动失败显示具体原因并保留重试入口', async () => {
  const failed = voicePreviewSnapshot('unavailable', {
    reason: '试听输出失败：系统没有默认输出设备（试听需要它）',
  });
  const app = await boot({ running: true, onInvoke(command) {
    if (command === 'voice_status') return failed;
    if (command === 'status') return playbackStatus({ voice: failed });
    return voiceFixture(command);
  } });
  const button = app.document.getElementById('voicePreview');
  const hint = app.document.getElementById('previewHint');
  assert.equal(button.disabled, false, '临时失败必须允许重试');
  assert.equal(button.textContent, '试听 3 秒');
  assert.match(hint.textContent, /试听输出失败/);
  assert.match(button.title, /试听输出失败/);
});

// ── 人声性能指标与按住对比 ───────────────────────────────────

const voiceMetrics = (patch = {}) => ({
  latency_ms: 22, block_ms: 10, sample_rate: 48000,
  process_us_last: 50, process_us_max: 120,
  overflow_frames: 0, gap_frames: 0,
  ...patch,
});
const abArgs = app => app.calls.filter(call => call.command === 'voice_set_ab_bypass')
  .map(call => call.args);

async function bootAb(patch = {}) {
  let generation = 1;
  const snapshot = (extra = {}) => voiceSnapshot({
    enabled: true, phase: 'wet', ...patch, ...extra,
  });
  return boot({ running: true, onInvoke(command, args) {
    if (command === 'voice_set_ab_bypass') {
      const active = args.active === true;
      return snapshot({ bypassed: active, ab_bypassed: active, generation: ++generation });
    }
    if (command === 'voice_status') return snapshot();
    if (command === 'voice_presets') return voicePresetList;
    if (command === 'status') return playbackStatus({ voice: snapshot() });
  } });
}

test('人声面板显示延迟与处理耗时', async () => {
  const snapshot = voiceSnapshot({ enabled: true, phase: 'wet', metrics: voiceMetrics() });
  const app = await boot({ running: true, onInvoke(command) {
    if (command === 'voice_status') return snapshot;
    if (command === 'status') return playbackStatus({ voice: snapshot });
    return voiceFixture(command);
  } });
  const line = app.document.getElementById('voiceMetrics');
  assert.ok(line, '人声面板应有性能行');
  assert.equal(line.getAttribute('aria-live'), 'off');
  assert.equal(line.textContent,
    '延迟 22.0 ms · 块 10.0 ms · 处理 0.05 / 0.12 ms · 溢出 0 · 缺帧 0');
  const dialog = line.closest('.voice-dialog');
  const presetSection = app.document.getElementById('voicePresetNote').closest('.settings-section');
  const footer = app.document.getElementById('voiceDone').closest('.dialog-footer');
  assert.ok(dialog, '性能行应位于人声面板');
  assert.ok(dialog.children.indexOf(presetSection) < dialog.children.indexOf(line),
    '性能行应排在预设区之后');
  assert.ok(dialog.children.indexOf(line) < dialog.children.indexOf(footer),
    '性能行应排在底部按钮之前');
  const ab = app.document.getElementById('voiceAbBypass');
  assert.equal(ab.textContent, '按住听原声');
  assert.equal(ab.classList.contains('ghost'), true);
  assert.equal(ab.parentElement, app.document.getElementById('voicePreview').parentElement,
    '按住按钮应与试听、编辑并排');
  const css = fs.readFileSync(path.join(root, 'style.css'), 'utf8');
  assert.match(css, /\.voice-metrics\s*\{[^}]*overflow-wrap:\s*anywhere/s,
    '性能行应允许换行，避免窄窗口横向滚动');
});

test('按住听原声发送开始与释放命令', async () => {
  const app = await bootAb();
  const ab = app.document.getElementById('voiceAbBypass');
  const preview = app.document.getElementById('voicePreview');
  assert.equal(ab.disabled, false, '会话就绪且为湿声时应可按住');
  assert.equal(ab.getAttribute('aria-pressed'), 'false');
  await ab.emit('pointerdown', { pointerId: 7, button: 0 });
  assert.equal(ab.getAttribute('aria-pressed'), 'true');
  assert.equal(ab.classList.contains('active'), true);
  assert.equal(preview.disabled, true, '按住对比期间试听必须禁用');
  assert.match(app.document.getElementById('voiceState').textContent, /对比原声/);
  await flush();
  assert.deepEqual(abArgs(app), [{ active: true }]);
  await ab.emit('pointerup', { pointerId: 7, button: 0 });
  assert.equal(ab.getAttribute('aria-pressed'), 'false');
  assert.equal(ab.classList.contains('active'), false);
  await flush();
  assert.deepEqual(abArgs(app), [{ active: true }, { active: false }]);
  assert.equal(preview.disabled, false, '释放后试听按钮恢复');
  assert.match(app.document.getElementById('voiceState').textContent, /已启用/);
  await ab.emit('lostpointercapture', { pointerId: 7 });
  await flush();
  assert.equal(abArgs(app).length, 2, '释放后的指针捕获丢失不能重复发送');
});

test('指针取消会释放对比', async () => {
  const app = await bootAb();
  const ab = app.document.getElementById('voiceAbBypass');
  await ab.emit('pointerdown', { pointerId: 3 });
  await flush();
  await ab.emit('pointercancel', { pointerId: 3 });
  await flush();
  assert.deepEqual(abArgs(app), [{ active: true }, { active: false }]);
  assert.equal(ab.getAttribute('aria-pressed'), 'false');

  await ab.emit('pointerdown', { pointerId: 4 });
  await flush();
  await ab.emit('lostpointercapture', { pointerId: 4 });
  await flush();
  assert.deepEqual(abArgs(app), [
    { active: true }, { active: false }, { active: true }, { active: false },
  ], '丢失指针捕获也要释放');
});

test('窗口失焦与页面隐藏会释放对比', async () => {
  const app = await bootAb();
  const ab = app.document.getElementById('voiceAbBypass');
  await ab.emit('pointerdown', { pointerId: 1 });
  await flush();
  await app.emitWindow('blur');
  assert.deepEqual(abArgs(app), [{ active: true }, { active: false }], '窗口失焦应释放');
  assert.equal(ab.getAttribute('aria-pressed'), 'false');

  await ab.emit('pointerdown', { pointerId: 2 });
  await flush();
  app.document.visibilityState = 'hidden';
  await app.document.emit('visibilitychange');
  assert.deepEqual(abArgs(app).at(-1), { active: false }, '页面隐藏应释放');
  assert.equal(ab.getAttribute('aria-pressed'), 'false');
  app.document.visibilityState = 'visible';
});

test('试听中与预热中按钮禁用并显示原因', async () => {
  const recording = voiceSnapshot({ enabled: true, phase: 'wet',
    preview: { state: 'recording', elapsed_ms: 1200, reason: null } });
  const previewApp = await boot({ running: true, onInvoke(command) {
    if (command === 'voice_status') return recording;
    if (command === 'status') return playbackStatus({ voice: recording });
    return voiceFixture(command);
  } });
  const previewButton = previewApp.document.getElementById('voiceAbBypass');
  assert.equal(previewButton.disabled, true);
  assert.equal(previewButton.title, '正在试听');
  await previewButton.emit('pointerdown', { pointerId: 1 });
  await flush();
  assert.equal(abArgs(previewApp).length, 0, '禁用时不能发送开始命令');

  const warmup = voiceSnapshot({ enabled: true, phase: 'warmup' });
  const warmupApp = await boot({ running: true, onInvoke(command) {
    if (command === 'voice_status') return warmup;
    if (command === 'status') return playbackStatus({ voice: warmup });
    return voiceFixture(command);
  } });
  const warmupButton = warmupApp.document.getElementById('voiceAbBypass');
  assert.equal(warmupButton.disabled, true);
  assert.equal(warmupButton.title, '正在预热');
});

test('用户旁路时按钮禁用', async () => {
  const snapshot = voiceSnapshot({ enabled: true, bypassed: true, phase: 'wet' });
  const app = await boot({ running: true, onInvoke(command) {
    if (command === 'voice_status') return snapshot;
    if (command === 'status') return playbackStatus({ voice: snapshot });
    return voiceFixture(command);
  } });
  const ab = app.document.getElementById('voiceAbBypass');
  assert.equal(ab.disabled, true);
  assert.equal(ab.title, '当前已临时旁路');
  await ab.emit('pointerdown', { pointerId: 1 });
  await flush();
  assert.equal(abArgs(app).length, 0, '用户旁路不能被按住对比覆盖');
  assert.match(app.document.getElementById('voiceState').textContent, /临时旁路/);
  assert.equal(ab.getAttribute('aria-pressed'), 'false');
});

test('按住对比不触发配置保存', async () => {
  const app = await bootAb();
  const ab = app.document.getElementById('voiceAbBypass');
  await ab.emit('pointerdown', { pointerId: 1 });
  await flush();
  await ab.emit('pointerup', { pointerId: 1 });
  await flush();
  assert.equal(app.calls.filter(call => call.command === 'save_config').length, 0,
    '按住对比不能写配置');
  assert.equal(app.calls.some(call => call.command === 'voice_set_enabled'), false,
    '按住对比不能改动启用开关');
  assert.deepEqual(abArgs(app), [{ active: true }, { active: false }]);
});

test('指标文本变化不重建面板', async () => {
  let snapshot = voiceSnapshot({ enabled: true, phase: 'wet', metrics: voiceMetrics() });
  const app = await boot({ running: true, onInvoke(command) {
    if (command === 'voice_status') return snapshot;
    if (command === 'status') return playbackStatus({ voice: snapshot });
    return voiceFixture(command);
  } });
  await app.document.getElementById('voiceEdit').emit('click');
  const line = app.document.getElementById('voiceMetrics');
  const preview = app.document.getElementById('voicePreview');
  const pitch = app.document.getElementById('pitch');
  const ab = app.document.getElementById('voiceAbBypass');
  const before = line.textContent;

  snapshot = voiceSnapshot({ enabled: true, phase: 'wet', generation: 1,
    metrics: voiceMetrics({ process_us_last: 80, process_us_max: 240,
      overflow_frames: 3, gap_frames: 5 }) });
  await app.pollStatus();

  assert.notEqual(line.textContent, before, '同代次指标变化也必须刷新');
  assert.match(line.textContent, /处理 0\.08 \/ 0\.24 ms/);
  assert.match(line.textContent, /溢出 3/);
  assert.match(line.textContent, /缺帧 5/);
  assert.equal(app.document.getElementById('voiceMetrics'), line, '性能行节点保持');
  assert.equal(app.document.getElementById('voicePreview'), preview, '按钮节点保持');
  assert.equal(app.document.getElementById('pitch'), pitch, '滑杆节点保持');
  assert.equal(app.document.getElementById('voiceAbBypass'), ab, '按住按钮节点保持');
});

test('无 metrics 时显示占位符', async () => {
  const app = await boot({ running: true, onInvoke(command) {
    const snapshot = voiceSnapshot({ enabled: true, phase: 'wet' });
    if (command === 'voice_status') return snapshot;
    if (command === 'status') return playbackStatus({ voice: snapshot });
    return voiceFixture(command);
  } });
  const line = app.document.getElementById('voiceMetrics');
  assert.equal(line.textContent, '—');
  assert.equal(line.textContent.includes('0 ms'), false, '没有指标时不能伪造 0');

  const zeroRate = voiceSnapshot({ enabled: true, phase: 'wet',
    metrics: voiceMetrics({ sample_rate: 0, latency_ms: 0, block_ms: 0 }) });
  const zeroApp = await boot({ running: true, onInvoke(command) {
    if (command === 'voice_status') return zeroRate;
    if (command === 'status') return playbackStatus({ voice: zeroRate });
    return voiceFixture(command);
  } });
  assert.equal(zeroApp.document.getElementById('voiceMetrics').textContent, '—',
    '采样率未知时同样显示占位符');
});

test('键盘空格与回车也能按住对比', async () => {
  const app = await bootAb();
  const ab = app.document.getElementById('voiceAbBypass');
  await ab.emit('keydown', { key: ' ' });
  await flush();
  assert.equal(ab.getAttribute('aria-pressed'), 'true');
  assert.deepEqual(abArgs(app).at(-1), { active: true });
  await ab.emit('keyup', { key: ' ' });
  await flush();
  assert.deepEqual(abArgs(app).at(-1), { active: false });

  await ab.emit('keydown', { key: 'Enter' });
  await flush();
  assert.deepEqual(abArgs(app).at(-1), { active: true });
  await ab.emit('keyup', { key: 'Enter' });
  await flush();
  assert.deepEqual(abArgs(app), [
    { active: true }, { active: false }, { active: true }, { active: false },
  ]);
});

test('关闭人声面板会释放对比', async () => {
  const app = await bootAb();
  await app.document.getElementById('voiceEdit').emit('click');
  const ab = app.document.getElementById('voiceAbBypass');
  await ab.emit('pointerdown', { pointerId: 5 });
  await flush();
  await app.document.getElementById('voiceClose').emit('click');
  await flush();
  assert.deepEqual(abArgs(app), [{ active: true }, { active: false }]);
  assert.equal(ab.getAttribute('aria-pressed'), 'false');
});

test('按住对比时切换预设与启用使用用户的新值', async () => {
  const app = await bootAb();
  const ab = app.document.getElementById('voiceAbBypass');
  const preset = app.document.getElementById('voicePreset');
  const enabled = app.document.getElementById('voiceEnabled');

  await ab.emit('pointerdown', { pointerId: 6 });
  await flush();
  preset.value = 'builtin:male';
  await preset.emit('change');
  await flush();
  assert.deepEqual(app.calls.filter(call => call.command === 'voice_select_preset')
    .map(call => call.args), [{ id: 'builtin:male' }],
  '释放对比不能把用户选择的预设改回旧值');
  assert.equal(preset.value, 'builtin:male');

  await ab.emit('pointerdown', { pointerId: 7 });
  await flush();
  enabled.checked = false;
  await enabled.emit('change');
  await flush();
  assert.deepEqual(app.calls.filter(call => call.command === 'voice_set_enabled')
    .map(call => call.args), [{ enabled: false }],
  '释放对比不能把用户的启用开关改回旧值');
  assert.equal(enabled.checked, false);
});

const watchTextWrites = element => {
  let value = element.textContent;
  let writes = 0;
  Object.defineProperty(element, 'textContent', {
    configurable: true,
    get() { return value; },
    set(next) { writes++; value = String(next); },
  });
  return () => writes;
};

test('指标刷新不重写面板与读屏文本', async () => {
  let snapshot = voiceSnapshot({ enabled: true, phase: 'wet', metrics: voiceMetrics() });
  const app = await boot({ running: true, onInvoke(command) {
    if (command === 'voice_status') return snapshot;
    if (command === 'status') return playbackStatus({ voice: snapshot });
    return voiceFixture(command);
  } });
  await app.document.getElementById('voiceEdit').emit('click');
  const previewHint = app.document.getElementById('previewHint');
  const saveState = app.document.getElementById('voiceSaveState');
  const pitchOut = app.document.getElementById('pitchOut');
  const metrics = app.document.getElementById('voiceMetrics');
  const previewWrites = watchTextWrites(previewHint);
  const saveWrites = watchTextWrites(saveState);
  const pitchWrites = watchTextWrites(pitchOut);
  const metricWrites = watchTextWrites(metrics);

  snapshot = voiceSnapshot({ enabled: true, phase: 'wet', generation: 1,
    metrics: voiceMetrics({ process_us_last: 90 }) });
  await app.pollStatus();
  assert.match(metrics.textContent, /处理 0\.09/);
  assert.equal(metricWrites(), 1, '性能行只更新一次文本');
  assert.equal(previewWrites(), 0, '指标变化不能重写试听提示');
  assert.equal(saveWrites(), 0, '指标变化不能重写保存提示');
  assert.equal(pitchWrites(), 0, '指标变化不能重建滑杆数值');

  await app.pollStatus();
  assert.equal(metricWrites(), 1, '指标未变化时不重复写文本');

  snapshot = voiceSnapshot({ enabled: true, phase: 'wet', generation: 1,
    preview: { state: 'recording', elapsed_ms: 1000, reason: null },
    metrics: voiceMetrics({ process_us_last: 90 }) });
  await app.pollStatus();
  const hintWritesAtStart = previewWrites();
  assert.equal(hintWritesAtStart, 1, '试听状态真实变化时才更新提示');
  assert.equal(saveWrites(), 0, '状态轮询不能重写隐藏的保存提示');

  snapshot = voiceSnapshot({ enabled: true, phase: 'wet', generation: 1,
    preview: { state: 'recording', elapsed_ms: 2000, reason: null },
    metrics: voiceMetrics({ process_us_last: 90 }) });
  await app.pollStatus();
  assert.equal(previewWrites(), hintWritesAtStart, '进度变化不重写相同的读屏提示');
  assert.equal(saveWrites(), 0);
});
