// 麦克风混音助手前端。纯静态、无打包器 —— 通过 withGlobalTauri 暴露的 __TAURI__ 调用 Rust。

const invoke = (cmd, args) => window.__TAURI__.core.invoke(cmd, args);

const $ = (id) => document.getElementById(id);

// ── 前端错误上报 ─────────────────────────────────────────
// 打包后的 App 没有控制台，前端抛异常就完全静默（只会表现为"窗口空白"）。
// 这里把错误送回后端，`open --stdout <文件>` 就能看到。
function reportErr(msg, stack) {
  // 必须**作为方法**调用（window.__TAURI__.core.invoke(...)），
  //    不能先取出来再裸调 —— 那样会丢 this，Tauri 的 invoke 会抛异常，
  //    而异常又被这里的 catch 吞掉，结果"上报永远成功、实际什么都没发出去"。
  try {
    window.__TAURI__?.core?.invoke?.('js_error', {
      msg: String(msg),
      stack: String(stack || ''),
    });
  } catch (e) {
    console.error('上报失败', e);
  }
  console.error(msg, stack);
}
window.addEventListener('error', e => reportErr(e.message + ' @' + e.lineno, e.error?.stack));
window.addEventListener('unhandledrejection', e =>
  reportErr('未处理的 Promise: ' + (e.reason?.message || e.reason), e.reason?.stack));

const el = {
  dot: $('dot'), headline: $('headline'), btnToggle: $('btnToggle'),
  selIn: $('selIn'), selOut: $('selOut'), inBuf: $('inBuf'),
  inDir: $('inDir'), btnBrowse: $('btnBrowse'), btnScan: $('btnScan'),
  inKeys: $('inKeys'), inMicGain: $('inMicGain'), inSfxGain: $('inSfxGain'),
  voiceKeyBypass: $('voiceKeyBypass'), voiceKeyNext: $('voiceKeyNext'), voiceKeyPrev: $('voiceKeyPrev'),
  inLoop: $('inLoop'), loopHelp: $('loopHelp'), hint: $('setupHint'),
  btnAdd: $('btnAdd'), btnClear: $('btnClear'), count: $('count'),
  inRemote: $('inRemote'), remote: $('remote'), meter: $('meter'),
  inMonitor: $('inMonitor'),
  btnSelfTest: $('btnSelfTest'), testPanel: $('testPanel'),
  btnQr: $('btnQr'), qrPanel: $('qrPanel'),
  inSearch: $('inSearch'), tagbar: $('tagbar'), btnSort: $('btnSort'),
  btnRelease: $('btnRelease'), btnCalib: $('btnCalib'), calibHint: $('calibHint'),
  btnRevertMic: $('btnRevertMic'),
  vdevHelp: $('vdevHelp'), micHelp: $('micHelp'), btnMicHelp: $('btnMicHelp'),
  grid: $('grid'), metrics: $('metrics'), toast: $('toast'),
  btnStopAll: $('btnStopAll'), setup: $('setup'), btnSetup: $('btnSetup'),
  btnAbout: $('btnAbout'), btnAboutClose: $('btnAboutClose'),
  aboutOverlay: $('aboutOverlay'), aboutTitle: $('aboutTitle'), aboutVersion: $('aboutVersion'),
  appShell: $('appShell'), toolsMenu: $('toolsMenu'), healthDetails: $('healthDetails'),
  selTheme: $('selTheme'), themeHelp: $('themeHelp'),
  btnSetupClose: $('btnSetupClose'), btnSetupSave: $('btnSetupSave'),
  btnLibrarySetup: $('btnLibrarySetup'), librarySettings: $('librarySettings'),
  selMonitor: $('selMonitor'), micGainSlider: $('micGainSlider'), sfxGainSlider: $('sfxGainSlider'),
  inputRoute: $('inputRoute'), outputRoute: $('outputRoute'), routeHelp: $('routeHelp'),
  btnDriverHelp: $('btnDriverHelp'), monitorHelp: $('monitorHelp'),
  player: $('player'), playerName: $('playerName'), playerMode: $('playerMode'),
  playerProgress: $('playerProgress'), playerTime: $('playerTime'), signalLabel: $('signalLabel'),
  healthSummary: $('healthSummary'), searchShortcut: $('searchShortcut'), notice: $('notice'),
  soundOverlay: $('soundOverlay'), soundTitle: $('soundTitle'), soundFile: $('soundFile'),
  soundGain: $('soundGain'), soundGainValue: $('soundGainValue'),
  soundName: $('soundName'), soundTags: $('soundTags'), soundHotkey: $('soundHotkey'),
  btnSoundClose: $('btnSoundClose'), btnSoundSave: $('btnSoundSave'),
  btnSoundLeft: $('btnSoundLeft'), btnSoundRight: $('btnSoundRight'), btnSoundRemove: $('btnSoundRemove'),
  utilityOverlay: $('utilityOverlay'), utilityTitle: $('utilityTitle'), btnUtilityClose: $('btnUtilityClose'),
  calibPanel: $('calibPanel'),
  voiceState: $('voiceState'), voiceEnabled: $('voiceEnabled'),
  voicePreset: $('voicePreset'), bypassBadge: $('bypassBadge'), voicePreview: $('voicePreview'),
  voiceEdit: $('voiceEdit'), previewHint: $('previewHint'), voiceStatus: $('voiceStatus'),
  voiceOverlay: $('voiceOverlay'), voiceClose: $('voiceClose'), voiceDone: $('voiceDone'),
  voiceCurrentPreset: $('voiceCurrentPreset'), dirtyBadge: $('dirtyBadge'),
  modalBypassBadge: $('modalBypassBadge'), pitch: $('pitch'), pitchOut: $('pitchOut'),
  formant: $('formant'), formantOut: $('formantOut'),
  gateOn: $('gateOn'), gate: $('gate'), gateOut: $('gateOut'),
  eqLowOn: $('eqLowOn'), eqLow: $('eqLow'), eqLowOut: $('eqLowOut'),
  eqMidOn: $('eqMidOn'), eqMid: $('eqMid'), eqMidOut: $('eqMidOut'),
  eqHighOn: $('eqHighOn'), eqHigh: $('eqHigh'), eqHighOut: $('eqHighOut'),
  driveOn: $('driveOn'), drive: $('drive'), driveOut: $('driveOut'),
  robotOn: $('robotOn'), robotHz: $('robotHz'), robotHzOut: $('robotHzOut'),
  robotMix: $('robotMix'), robotMixOut: $('robotMixOut'),
  voiceSaveAs: $('voiceSaveAs'), voiceSaveChanges: $('voiceSaveChanges'),
  voiceRename: $('voiceRename'), voiceDelete: $('voiceDelete'),
  voiceRestoreDraft: $('voiceRestoreDraft'), voicePresetNote: $('voicePresetNote'),
  voiceNameEditor: $('voiceNameEditor'), voicePresetName: $('voicePresetName'),
  voiceNameConfirm: $('voiceNameConfirm'), voiceNameCancel: $('voiceNameCancel'),
  voiceSaveState: $('voiceSaveState'),
};

el.selTheme.value = window.MixerTheme.getPreference();
el.selTheme.addEventListener('change', () => {
  const saved = window.MixerTheme.setPreference(el.selTheme.value);
  el.themeHelp.textContent = saved ? '立即生效，下次打开会记住。' : '本次已生效，但未能保存；下次打开后请重新选择。';
  if (!saved) toast(el.themeHelp.textContent, true);
});

// 模态面板只管理界面焦点，不领取音频票据或改变会话。
let aboutOpen = false;
let activeModal = null;
let modalReturnFocus = null;
let modalGeneration = 0;
let modalReturnSoundId = null;
function modalControls() {
  if (!activeModal) return [];
  return [...activeModal.querySelectorAll('button, input, select, a, [tabindex]')].filter(node => {
    if (node.disabled || node.getAttribute('tabindex') === '-1') return false;
    for (let parent = node; parent && parent !== activeModal; parent = parent.parentElement) {
      if (parent.classList.contains('hidden')) return false;
    }
    return true;
  });
}
function showModal(overlay, focus) {
  if (!activeModal) {
    modalReturnFocus = el.toolsMenu.contains(document.activeElement)
      ? el.toolsMenu.querySelector('summary') : document.activeElement;
    modalReturnSoundId = document.activeElement?.closest?.('.card')?.dataset.soundId;
  }
  else if (activeModal !== overlay) activeModal.classList.add('hidden');
  ++modalGeneration;
  activeModal = overlay;
  aboutOpen = overlay === el.aboutOverlay;
  setupOpen = overlay === el.setup;
  el.toolsMenu.open = false;
  el.healthDetails.open = false;
  el.appShell.inert = true;
  overlay.classList.remove('hidden');
  (focus || modalControls()[0])?.focus();
}
function closeModal() {
  if (!activeModal) return;
  activeModal.classList.add('hidden');
  ++modalGeneration;
  activeModal = null;
  aboutOpen = false;
  setupOpen = false;
  el.appShell.inert = false;
  const focus = modalReturnFocus;
  modalReturnFocus = null;
  const replacement = [...el.grid.children].find(card => card.dataset.soundId === modalReturnSoundId)?.querySelector('.sound-settings');
  modalReturnSoundId = null;
  if (focus && document.contains(focus)) focus.focus();
  else (replacement || el.btnSetup).focus();
}
async function openAbout() {
  if (aboutOpen) return;
  showModal(el.aboutOverlay, el.btnAboutClose);
  try {
    const info = await invoke('app_info');
    if (typeof info?.name === 'string' && info.name) el.aboutTitle.textContent = info.name;
    if (typeof info?.version === 'string' && info.version) {
      el.aboutVersion.textContent = `版本 ${info.version} · 支持 macOS 和 Windows`;
      el.aboutVersion.classList.remove('hidden');
    }
  } catch (_) { /* 简介与关闭功能不依赖版本信息。 */ }
}
function closeAbout() { if (aboutOpen) closeModal(); }
el.btnAbout.addEventListener('click', openAbout);
el.btnAboutClose.addEventListener('click', closeAbout);
for (const overlay of [el.aboutOverlay, el.setup, el.soundOverlay, el.voiceOverlay, el.utilityOverlay]) {
  overlay.addEventListener('click', event => {
    if (event.target === overlay) closeModal();
  });
}
document.addEventListener('focusin', event => {
  if (activeModal && !activeModal.contains(event.target)) modalControls()[0]?.focus();
});
function openUtility(panel, title) {
  for (const child of [el.qrPanel, el.testPanel, el.vdevHelp, el.micHelp, el.calibPanel]) {
    child.classList.toggle('hidden', child !== panel);
  }
  el.utilityTitle.textContent = title;
  showModal(el.utilityOverlay, el.btnUtilityClose);
}
el.btnUtilityClose.addEventListener('click', closeModal);

let state = { running: false, slots: [], items: [], lastStatus: null,
               monitorDevice: null, remoteUrl: null, activeTag: null, devices: [],
               lastDeviceLost: null, lastPanicSeen: null, lastStalled: null,
               lastMonitorError: null,
               silentSecs: 0, micHelpShown: false, micHelpDismissed: false };

// ── 变声状态与音效播放状态分开缓存 ───────────────────────────
// 变声命令不领取播放票据；保存类命令共用一条队列，回复按用户操作顺序生效。
// 本地编辑先写入 voiceState，轮询快照只在没有对应本地修改时覆盖。
// 块时长与宿主会话默认值一致；启动参数只影响当前会话，不写入配置。
const VOICE_BLOCK_MS = 10;
function neutralVoiceParams() {
  return {
    pitch_semitones: 0, formant_semitones: 0,
    gate_enabled: false, gate_db: -45,
    eq_low_enabled: false, eq_low_db: 0,
    eq_mid_enabled: false, eq_mid_db: 0,
    eq_high_enabled: false, eq_high_db: 0,
    drive_enabled: false, drive: 0,
    robot_enabled: false, robot_hz: 50, robot_mix: 0.8,
  };
}
const voiceState = {
  loaded: false, enabled: false, bypassed: false,
  presetId: '', presetName: '', modified: false,
  draftAvailable: false, saveError: null,
  params: neutralVoiceParams(),
};
// 参数值直接对应 VoiceParams，开关不用哨兵值表达关闭。
const VOICE_PARAM_CONTROLS = [
  { input: 'pitch', out: 'pitchOut', key: 'pitch_semitones', format: formatSemitone },
  { input: 'formant', out: 'formantOut', key: 'formant_semitones', format: formatSemitone },
  { input: 'gate', out: 'gateOut', key: 'gate_db', format: formatDb },
  { input: 'eqLow', out: 'eqLowOut', key: 'eq_low_db', format: formatDb },
  { input: 'eqMid', out: 'eqMidOut', key: 'eq_mid_db', format: formatDb },
  { input: 'eqHigh', out: 'eqHighOut', key: 'eq_high_db', format: formatDb },
  { input: 'drive', out: 'driveOut', key: 'drive', format: formatDecimal },
  { input: 'robotHz', out: 'robotHzOut', key: 'robot_hz', format: formatHz },
  { input: 'robotMix', out: 'robotMixOut', key: 'robot_mix', format: formatDecimal },
];
const VOICE_SWITCH_CONTROLS = [
  { input: 'gateOn', key: 'gate_enabled' },
  { input: 'eqLowOn', key: 'eq_low_enabled' },
  { input: 'eqMidOn', key: 'eq_mid_enabled' },
  { input: 'eqHighOn', key: 'eq_high_enabled' },
  { input: 'driveOn', key: 'drive_enabled' },
  { input: 'robotOn', key: 'robot_enabled' },
];
let voicePresets = [];
let voicePresetLoadVersion = 0;
let voiceAppliedGeneration = -1;
let voiceEditVersion = 0;
const voicePending = { enabled: 0, preset: 0, params: 0, draft: 0 };
let voiceLocalParams = new Set();
let voiceParamTimer = null;
let voiceNameMode = '';
let voiceCommandTail = Promise.resolve();

// ── 音效库条目：{ id, file, name, gain, tags: string[] }
//    标签始终保存为数组，单个标签中的标点不作为分隔符。
const basename = MixerLibrary.basename;
const makeItem = (file, name, gain, tags, id) => ({
  id: id || (globalThis.crypto?.randomUUID?.() ?? `sound-${Date.now()}-${Math.random().toString(16).slice(2)}`),
  file,
  name: (name || '').trim() || basename(file),
  gain: Number.isFinite(+gain) ? Math.max(VOL_MIN, Math.min(VOL_MAX, +gain)) : 0,
  tags: MixerLibrary.tags(tags),
});
const itemTags = (it) => MixerLibrary.tags(it?.tags);
const allTags = () => {
  const set = new Set();
  for (const it of state.items) for (const t of itemTags(it)) set.add(t);
  return [...set].sort();
};

// 单个音效的音量（dB）。范围刻意留了增益空间（最高 +12dB）。
const VOL_MIN = -40, VOL_MAX = 12;
let toastTimer = null;

function toast(msg, isError = false) {
  el.toast.textContent = msg;
  el.toast.classList.toggle('error', isError);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { el.toast.textContent = ''; }, 2600);
}

function hint(msg, kind = '') {
  el.hint.textContent = msg;
  el.hint.className = 'hint' + (kind ? ' ' + kind : '');
  el.notice.textContent = kind === 'error' ? msg : '';
  el.notice.classList.toggle('hidden', kind !== 'error');
}

// ─────────────────────────────── 设备

async function loadSavedConfig() {
  try {
    const c = await invoke('load_config');
    if (!c) return null;
    if (c.dir) el.inDir.value = c.dir;
    if (Array.isArray(c.items) && c.items.length) {
      // 不能写成 .map(makeItem) —— map 会传 (元素, 索引, 数组)，
      //    而 makeItem 的签名是 (file, name, gain, tags)，整个对象会被当成路径。
      state.items = c.items.map(o => makeItem(o.file, o.name, o.gain, o.tags, o.id));
    }
    if (c.buffer_ms) el.inBuf.value = c.buffer_ms;
    if (Number.isFinite(c.mic_gain_db)) el.inMicGain.value = boundedGain(c.mic_gain_db);
    if (Number.isFinite(c.sfx_gain_db)) el.inSfxGain.value = boundedGain(c.sfx_gain_db);
    if (typeof c.hotkeys === 'string') el.inKeys.value = c.hotkeys;
    const voiceKeys = c.voice_hotkeys;
    if (voiceKeys && typeof voiceKeys === 'object') {
      if (typeof voiceKeys.bypass === 'string') el.voiceKeyBypass.value = voiceKeys.bypass;
      if (typeof voiceKeys.next === 'string') el.voiceKeyNext.value = voiceKeys.next;
      if (typeof voiceKeys.prev === 'string') el.voiceKeyPrev.value = voiceKeys.prev;
    }
    if (typeof c.monitor === 'boolean') el.inMonitor.checked = c.monitor;
    if (typeof c.looping === 'boolean') el.inLoop.checked = c.looping;
    syncLoopHelp();
    state.monitorDevice = c.monitor_device || null;
    syncGainControls();

    // 设备下拉里没有这一项时，临时插进去，避免设置丢失
    const pick = (sel, name, direction) => {
      if (!name) return;
      name = MixerLibrary.resolveSelector(state.devices, name, direction);
      if (![...sel.options].some(o => o.value === name)) {
        const o = document.createElement('option');
        o.value = name; o.textContent = name + '（上次使用）';
        sel.appendChild(o);
      }
      sel.value = name;
    };
    pick(el.selIn, c.input_device, '输入');
    pick(el.selOut, c.output_device, '输出');
    pick(el.selMonitor, state.monitorDevice, '输出');
    syncConnection();

    if (c.dir) hint('已载入上次的设置', 'ok');
    return c;
  } catch (e) { return null; }
}

let configSaveTail = Promise.resolve();
async function saveCurrentConfig(before) {
  try {
    const gains = currentGains();
    const snapshot = {
      items: state.items.map(it => ({
        id: it.id, file: it.file, name: it.name, gain: it.gain, tags: it.tags,
      })),
      monitor: el.inMonitor.checked,
      looping: !!el.inLoop.checked,
      monitorDevice: state.monitorDevice || null,
      dir: el.inDir.value.trim(),
      inputDevice: el.selIn.value || null,
      outputDevice: el.selOut.value || null,
      bufferMs: parseInt(el.inBuf.value || '20', 10),
      micGainDb: gains.micDb,
      sfxGainDb: gains.sfxDb,
      hotkeys: el.inKeys.value.trim(),
    };
    // Preserve completion order when consecutive slider gestures save snapshots.
    configSaveTail = configSaveTail.catch(() => {}).then(async () => {
      if (before) await before.catch(() => {});
      return invoke('save_config', snapshot);
    });
    await configSaveTail;
    return true;
  } catch (error) {
    toast('设置保存失败：' + error + '。当前调整仍可使用，请重试保存。', true);
    return false;
  }
}

async function loadDevices() {
  try {
    const previousInput = el.selIn.value;
    const previousOutput = el.selOut.value;
    const devs = (await invoke('list_devices')) || [];
    state.devices = devs;
    const inputs = devs.filter(d => d.direction === '输入');
    const outputs = devs.filter(d => d.direction === '输出');

    fill(el.selIn, inputs, '（系统默认麦克风）');
    fill(el.selOut, outputs, '（自动探测虚拟声卡）');
    fill(el.selMonitor, outputs, '系统默认输出');

    if (previousInput) el.selIn.value = MixerLibrary.resolveSelector(devs, previousInput, '输入');
    if (previousOutput) el.selOut.value = MixerLibrary.resolveSelector(devs, previousOutput, '输出');
    if (state.monitorDevice) state.monitorDevice = MixerLibrary.resolveSelector(devs, state.monitorDevice, '输出');
    const guess = previousOutput ? null : await invoke('guess_output');
    if (guess) {
      for (const o of el.selOut.options) {
        if (o.value === guess) { el.selOut.value = guess; break; }
      }
    }
    if (state.monitorDevice) el.selMonitor.value = state.monitorDevice;
    syncConnection();
    checkVirtualDevice();
  } catch (e) {
    hint('读取设备失败：' + e, 'error');
  }
}

function fill(sel, list, placeholder) {
  sel.innerHTML = '';
  const opt0 = document.createElement('option');
  opt0.value = '';
  opt0.textContent = placeholder;
  sel.appendChild(opt0);
  for (const d of list) {
    const o = document.createElement('option');
    o.value = d.id || d.name;
    o.textContent = d.name +
      (d.is_default ? ' · 默认' : '') +
      `  ${d.sample_rate / 1000}kHz/${d.channels}ch`;
    sel.appendChild(o);
  }
}

// ─────────────────────────────── 音效

async function scanDir(silent = false) {
  if (state.running) { if (!silent) toast('先点「关闭音效」再改音效库', true); return; }
  const dir = el.inDir.value.trim();
  if (!dir) { if (!silent) hint('先填一个音效文件夹', 'error'); return; }
  try {
    const files = (await invoke('scan_dir', { path: dir })) || [];
    if (state.running) { if (!silent) toast('先点「关闭音效」再改音效库', true); return; }
    // 路径相同的条目复用身份、名字、标签、音量和热键，包括未绑定的空热键。
    applyLibraryEdit(MixerLibrary.scanFiles(state.items, hotkeyList(), files, makeItem));
    if (!files.length) {
      hint('这个文件夹里没有音频文件（支持 mp3 / wav / ogg / m4a / flac 等）', 'error');
    } else {
      hint(`找到 ${files.length} 个音效`, 'ok');
    }
    render();
  } catch (e) {
    hint('读取文件夹失败：' + (e && e.message ? e.message : e) +
         ' —— 检查路径是否存在、是否有权限', 'error');
    render();
  }
}

function hotkeyList() {
  return MixerLibrary.hotkeys(el.inKeys.value);
}

// ─────────────────────────────── 启停

async function start() {
  const choice = newAudioChoice();
  let opened = false;
  if (!state.items.length) {
    await scanDir(true);
    if (!state.items.length) {
      hint('还没有音效。添加音频，或选择文件夹后点「载入」。', 'error');
      setSetupOpen(true);
      return;
    }
  }
  el.btnToggle.disabled = true;
  hint('正在启动音频…');
  await saveCurrentConfig();   // 记住这次的选择
  if (choice !== audioChoice) return;
  try {
    // Finish changes sent to the previous session before opening a new one.
    await loopUpdateTail.catch(() => {});
    await flushGainUpdates();
    if (choice !== audioChoice) return;
    const startingGains = currentGains();
    const cfg = {
        input_device: el.selIn.value || null,
        output_device: el.selOut.value || null,
        files: state.items.map(it => it.file),
        ids: state.items.map(it => it.id),
        tags: state.items.map(itemTags),
        gains: state.items.map(it => it.gain),
        names: state.items.map(it => it.name),
        hotkeys: hotkeyList(),
        buffer_ms: parseInt(el.inBuf.value || '20', 10),
        mic_gain_db: startingGains.micDb,
        sfx_gain_db: startingGains.sfxDb,
        looping: el.inLoop.checked,
        monitor: el.inMonitor.checked,
        // StartConfig 走 serde 反序列化，字段名必须**蛇形**。
        //    写成驼峰会被静默忽略，然后被 null 覆盖 —— 症状是
        //    「用 config set 指定了监听设备，一点启动就被抹掉」。
        monitor_device: state.monitorDevice || null,
        remote_port: el.inRemote.checked ? 8765 : null,
      };
    // 变声初值只影响本次会话；配置持久化由 voice_* 命令负责。
    // 未成功读取状态时省略该字段，宿主回退读取自己的配置。
    if (voiceState.loaded) {
      cfg.voice = {
        enabled: !!voiceState.enabled,
        params: { ...voiceState.params },
        block_ms: VOICE_BLOCK_MS,
      };
    }
    if (!await confirmedAudio(await invokeAudio('start', { cfg }, choice), choice)) return;
    opened = true;
    state.running = true;
    clearPlaybackFeedback();
    // Decode/open can take time. A loop edit made during startup was saved as
    // an idle preference; reconcile it with the newly opened session now.
    if (cfg.looping !== !!el.inLoop.checked) {
      await queueLoopUpdate(!!el.inLoop.checked);
      if (choice !== audioChoice) return;
    }
    const gains = currentGains();
    if (cfg.mic_gain_db !== gains.micDb || cfg.sfx_gain_db !== gains.sfxDb) {
      await flushGainUpdates(true);
      if (choice !== audioChoice) return;
    }
    await poll(choice);
    if (choice !== audioChoice) return;
    // The idle grid uses source positions. The opened session may skip bad
    // files and assign different loaded slots, so rebuild names and bindings.
    render();
    if (!state.running) {
      setSetupOpen(true);
      toast('音频状态已被其他操作更新');
      return;
    }
    setSetupOpen(false);   // 启动后收起设置，把空间让给音效网格
    closeTestPanel();   // 自检是"启动前"的检查，开起来之后留着只占地方
    toast('已启动 · 关闭窗口不会退出');
  hint(`正在把「${el.selIn.selectedOptions[0]?.textContent || '系统默认麦克风'}」的声音和音效一起送进 ` +
       `「${el.selOut.selectedOptions[0]?.textContent || '虚拟声卡'}」。在目标应用中选择对应的虚拟麦克风，对方就能同时听到你说话和音效。`, 'ok');
  } catch (e) {
    if (choice !== audioChoice) return;
    if (!opened) state.running = false;
    await poll(choice);
    if (choice !== audioChoice) return;
    render();
    hint((opened ? '音量或循环设置同步失败，请重新调整对应设置：' : '启动失败：') + e, 'error');
  } finally {
    if (choice === audioChoice) {
      el.btnToggle.disabled = false;
      syncToggle();
    }
  }
}

async function stop() {
  const choice = newAudioChoice();
  el.btnToggle.disabled = true;
  try {
    if (!await confirmedAudio(await invokeAudio('stop', {}, choice), choice)) return;
  } catch (e) {
    if (choice === audioChoice) toast('关闭音效失败：' + e, true);
    return;
  } finally {
    if (choice === audioChoice) el.btnToggle.disabled = false;
  }
  if (choice !== audioChoice) return;
  state.running = false;
  state.standby = true;
  state.slots = [];
  clearPlaybackFeedback();
  setSetupOpen(false);
  el.btnToggle.disabled = false;
  syncToggle();
  render();
  await poll(choice);
  if (choice !== audioChoice) return;
  if (state.running || !state.standby) {
    toast('音频状态已被其他操作更新');
    return;
  }
  toast('音效已关闭');
  hint('已停止音效。麦克风仍在转发 —— 对方照常听得到你说话，' +
       '想放音效时点「开启音效」即可，目标应用的麦克风设置不用动。', 'ok');
}

let setupOpen = false;
function setSetupOpen(open) {
  if (open) showModal(el.setup, el.btnSetupClose);
  else if (activeModal === el.setup) closeModal();
}
function syncConnection() {
  const input = el.selIn.selectedOptions[0]?.textContent || '系统默认麦克风';
  const output = el.selOut.selectedOptions[0]?.textContent || '自动探测虚拟声卡';
  el.inputRoute.textContent = input;
  el.outputRoute.textContent = output;
  el.inputRoute.title = input;
  el.outputRoute.title = output;
  // 声卡和遥控是开流配置，运行期间不能伪装成即时开关。
  const connected = state.running || state.standby;
  for (const control of [el.selIn, el.selOut, el.selMonitor, el.inBuf, el.inRemote, el.inMonitor]) {
    control.disabled = !!connected;
  }
  el.monitorHelp.textContent = connected
    ? (el.inMonitor.checked ? '监听已开启 · 释放设备后可更改' : '监听已关闭 · 释放设备后可更改')
    : '自己同步听到正式音效';
}
function syncGainControls() {
  for (const [number, slider] of [[el.inMicGain, el.micGainSlider], [el.inSfxGain, el.sfxGainSlider]]) {
    const db = parseFloat(number.value);
    if (Number.isFinite(db)) {
      slider.value = boundedGain(db);
      slider.setAttribute('aria-valuetext', formatGain(boundedGain(db)));
    }
  }
}

let gainTimer = null;
let gainUpdateTail = Promise.resolve();
function currentGains() {
  return {
    micDb: boundedGain(parseFloat(el.inMicGain.value || '0')),
    sfxDb: boundedGain(parseFloat(el.inSfxGain.value || '-6')),
  };
}
function boundedGain(db) { return Math.max(-60, Math.min(36, db)); }
function queueGainUpdate(gains = currentGains()) {
  gainUpdateTail = gainUpdateTail.catch(() => {}).then(() => invoke('set_gains', gains));
  return gainUpdateTail;
}
function flushGainUpdates(force = false) {
  const pending = gainTimer !== null;
  if (pending || force) {
    clearTimeout(gainTimer);
    gainTimer = null;
    const update = queueGainUpdate();
    if (pending) saveCurrentConfig(update);
  }
  return gainUpdateTail;
}
function pushGains() {
  // Number inputs allow typed values beyond min/max. Keep the display, IPC and
  // saved setting equal without rewriting an unfinished empty edit.
  for (const input of [el.inMicGain, el.inSfxGain]) {
    const value = parseFloat(input.value);
    if (Number.isFinite(value)) input.value = boundedGain(value);
  }
  syncGainControls();
  clearTimeout(gainTimer);
  gainTimer = setTimeout(() => {
    gainTimer = null;
    const update = queueGainUpdate();
    saveCurrentConfig(update);
    update.then(() => toast('音量已更新')).catch(e => toast('音量更新失败：' + e, true));
  }, 120);
}

function syncToggle() {
  // 待机时按钮说「开启音效」而不是「启动」—— 因为引擎其实一直在跑，
  // 变的只是音效开没开。
  // 别叫「停止」—— 底部还有一个「停止播放」，两者完全不是一回事：
  //    这个是**开关音效功能**（关闭后麦克风照样转发），那个是**掐掉正在响的音效**。
  el.btnToggle.textContent = state.running ? '关闭音效'
    : state.standby ? '开启音效' : '启动';
  el.btnToggle.title = state.running
    ? '关掉音效播放。麦克风会继续转发，对方照样听得到你说话'
    : state.standby
      ? '重新打开音效（设备不重开，瞬间生效）'
      : '启动音频引擎（麦克风开始转发，热键生效）';
  el.btnToggle.classList.toggle('running', state.running);
  syncConnection();
}

// ─────────────────────────────── 人声效果

function voiceParamsFrom(source) {
  const params = neutralVoiceParams();
  if (source && typeof source === 'object') {
    for (const key of Object.keys(params)) {
      if (key.endsWith('_enabled')) params[key] = source[key] === true;
      else if (Number.isFinite(+source[key])) params[key] = +source[key];
    }
  }
  return params;
}

function voicePresetName(id) {
  return voicePresets.find(preset => preset && preset.id === id)?.name;
}

function markVoicePending(kind) {
  const token = ++voiceEditVersion;
  voicePending[kind] = token;
  return token;
}

function settleVoicePending(kind, token) {
  if (voicePending[kind] !== token) return false;
  voicePending[kind] = 0;
  return true;
}

function queueVoiceCommand(run) {
  const task = voiceCommandTail.catch(() => {}).then(run);
  voiceCommandTail = task.catch(() => {});
  return task;
}

/// 变声命令只有在仍属于最新一次本地操作时才回写状态；
/// 失败时重新读取宿主快照，避免用乐观值掩盖未生效的设置。
function runVoiceCommand(kind, token, command, args, failureText, after) {
  return queueVoiceCommand(async () => {
    try {
      const snapshot = await invoke(command, args);
      const settled = settleVoicePending(kind, token);
      if (settled && kind === 'params') voiceLocalParams.clear();
      applyVoiceSnapshot(snapshot);
      if (after) await after();
    } catch (error) {
      if (settleVoicePending(kind, token)) {
        if (kind === 'params') voiceLocalParams.clear();
        toast(failureText + '：' + error, true);
        await loadVoiceStatus();
      }
    }
  });
}

function formatSigned(value, unit) {
  const number = Number.isFinite(value) ? Math.round(value * 10) / 10 : 0;
  return `${number > 0 ? '+' : number < 0 ? '−' : ''}${Math.abs(number)} ${unit}`;
}

function formatSemitone(value) {
  return formatSigned(value, '半音');
}

function formatDb(value) {
  return formatSigned(value, 'dB');
}

function formatDecimal(value) {
  return Number.isFinite(value) ? value.toFixed(2) : '0.00';
}

function formatHz(value) {
  return `${Number.isFinite(value) ? Math.round(value) : 0} Hz`;
}

function renderVoiceRange(input, output, value, format) {
  input.value = String(value);
  output.textContent = format(value);
  input.setAttribute('aria-valuetext', output.textContent);
}

/// 拖动时只同步该控件自己的只读数值，避免整面板重写。
function syncVoiceParamControl(control) {
  const value = voiceState.params[control.key];
  const input = el[control.input];
  if (input.value !== String(value)) input.value = String(value);
  const output = el[control.out];
  output.textContent = control.format(value);
  input.setAttribute('aria-valuetext', output.textContent);
}

function isUserVoicePreset() {
  return typeof voiceState.presetId === 'string' && voiceState.presetId.startsWith('user:');
}

function renderVoiceEditState() {
  el.dirtyBadge.classList.toggle('hidden', !voiceState.modified);
  el.voiceSaveChanges.disabled = !isUserVoicePreset() || !voiceState.modified;
}

function syncVoicePreview() {
  const sessionAlive = state.running || state.standby;
  const reason = !sessionAlive ? '音频未启动'
    : state.lastStatus?.preview_playing ? '本机试听占用中' : '';
  el.voicePreview.disabled = !!reason;
  el.voicePreview.title = reason || '录制 3 秒后本机回放，不送给对方';
  el.previewHint.textContent = reason || '试听只在本机回放，不送给对方。';
}

function voicePresetOption(id) {
  for (const option of el.voicePreset.options) if (option.value === id) return option;
  return null;
}

function ensureVoicePresetOption(id, name) {
  if (!id || voicePresetOption(id)) return;
  const option = document.createElement('option');
  option.value = id;
  option.textContent = name || id;
  el.voicePreset.appendChild(option);
}

function renderVoicePresets() {
  const pendingId = voicePending.preset ? el.voicePreset.value : '';
  const selected = pendingId || voiceState.presetId;
  el.voicePreset.innerHTML = '';
  for (const preset of voicePresets) {
    if (!preset || typeof preset.id !== 'string') continue;
    const option = document.createElement('option');
    option.value = preset.id;
    option.textContent = String(preset.name ?? preset.id);
    el.voicePreset.appendChild(option);
  }
  ensureVoicePresetOption(selected, voiceState.presetName);
  if (selected) el.voicePreset.value = selected;
}

function renderVoicePanel() {
  const params = voiceState.params;
  el.modalBypassBadge.classList.toggle('hidden', !voiceState.bypassed);
  el.voiceCurrentPreset.textContent = voiceState.presetName || '未读取';
  for (const control of VOICE_SWITCH_CONTROLS) {
    el[control.input].checked = !!params[control.key];
  }
  for (const control of VOICE_PARAM_CONTROLS) {
    renderVoiceRange(el[control.input], el[control.out], params[control.key], control.format);
  }

  const userPreset = isUserVoicePreset();
  for (const button of [el.voiceSaveChanges, el.voiceRename, el.voiceDelete]) {
    button.classList.toggle('hidden', !userPreset);
  }
  renderVoiceEditState();
  el.voiceRestoreDraft.classList.toggle('hidden', !voiceState.draftAvailable);
  el.voicePresetNote.textContent = userPreset
    ? '用户预设可直接保存修改；未保存的修改只在本次运行有效。'
    : '内置预设只读。修改后可另存为用户预设；当前修改只在本次运行有效。';
  el.voiceSaveState.classList.toggle('hidden', !voiceState.saveError);
  el.voiceSaveState.textContent = voiceState.saveError ? `未保存：${voiceState.saveError}` : '';
}

function renderVoice() {
  const name = voiceState.presetName || '未读取';
  const stateText = voiceState.bypassed ? '临时旁路'
    : voiceState.enabled ? '已启用' : '已关闭';
  el.voiceState.textContent = voiceState.loaded ? `${name} · ${stateText}` : '未读取';
  el.voiceStatus.textContent = voiceState.loaded ? `人声效果：${name} · ${stateText}` : '人声效果';
  el.voiceStatus.title = el.voiceStatus.textContent;
  el.voiceEnabled.checked = !!voiceState.enabled;
  if (voiceState.presetId) {
    ensureVoicePresetOption(voiceState.presetId, voiceState.presetName);
    el.voicePreset.value = voiceState.presetId;
  }
  el.bypassBadge.classList.toggle('hidden', !voiceState.bypassed);
  if (activeModal === el.voiceOverlay) renderVoicePanel();
  syncVoicePreview();
}

function voiceSaveErrorFrom(snapshot) {
  return typeof snapshot.save_error === 'string' && snapshot.save_error
    ? snapshot.save_error : null;
}

/// 应用一份宿主快照。轮询请求发出后若用户又编辑过，整份快照作废；
/// 单项仍在等待回复时只跳过对应字段，正在拖动的参数保持本地值。
function applyVoiceSnapshot(snapshot, { fromPoll = false, requestVersion = null } = {}) {
  if (!snapshot || typeof snapshot !== 'object') return;
  if (fromPoll && requestVersion !== voiceEditVersion) return;
  const generation = Number(snapshot.generation);
  if (Number.isFinite(generation) && generation < voiceAppliedGeneration) return;
  const presetPending = voicePending.preset || voicePending.draft;
  const paramsPending = voicePending.params || voicePending.draft;
  const saveError = voiceSaveErrorFrom(snapshot);
  const draftAvailable = snapshot.draft_available === true;
  // 同一代次下只有保存结果与草稿标记可能变化；其余字段相同则跳过重绘。
  if (fromPoll && !voicePending.enabled && !presetPending && !paramsPending
      && Number.isFinite(generation) && generation === voiceAppliedGeneration
      && saveError === voiceState.saveError && draftAvailable === voiceState.draftAvailable) {
    return;
  }
  voiceState.loaded = true;
  if (!voicePending.enabled) voiceState.enabled = snapshot.enabled === true;
  if (!presetPending) {
    if (typeof snapshot.preset_id === 'string') voiceState.presetId = snapshot.preset_id;
    if (typeof snapshot.preset_name === 'string') voiceState.presetName = snapshot.preset_name;
  }
  if (!paramsPending) {
    voiceState.params = voiceParamsFrom(snapshot.params);
    voiceState.modified = snapshot.modified === true;
    voiceLocalParams.clear();
  } else if (voicePending.params) {
    const merged = voiceParamsFrom(snapshot.params);
    for (const key of voiceLocalParams) merged[key] = voiceState.params[key];
    voiceState.params = merged;
    voiceState.modified = true;
  }
  voiceState.draftAvailable = draftAvailable;
  voiceState.saveError = saveError;
  voiceState.bypassed = snapshot.bypassed === true;
  if (Number.isFinite(generation)) voiceAppliedGeneration = Math.max(voiceAppliedGeneration, generation);
  renderVoice();
}

async function loadVoicePresets() {
  const version = ++voicePresetLoadVersion;
  try {
    const presets = await invoke('voice_presets');
    // 较旧的列表回复不能覆盖预设操作后发起的刷新。
    if (version !== voicePresetLoadVersion) return;
    voicePresets = Array.isArray(presets)
      ? presets.filter(preset => preset && typeof preset.id === 'string')
      : [];
    renderVoicePresets();
  } catch (_) {
    // 保留已有预设；状态文案仍由 voice_status 提供。
  }
}

async function loadVoiceStatus(guard) {
  const requestVersion = voiceEditVersion;
  try {
    const snapshot = await invoke('voice_status');
    if (guard && !guard()) return;
    applyVoiceSnapshot(snapshot, { fromPoll: !!guard, requestVersion });
  } catch (_) {
    // 变声不可用时保留上次快照，不阻塞其他界面。
  }
}

async function openVoicePanel() {
  closeVoiceNameEditor();
  showModal(el.voiceOverlay, el.voiceClose);
  renderVoice();
  const generation = modalGeneration;
  await Promise.all([
    loadVoicePresets(),
    loadVoiceStatus(() => generation === modalGeneration),
  ]);
}

el.voiceEdit.addEventListener('click', openVoicePanel);
el.voiceClose.addEventListener('click', closeModal);
el.voiceDone.addEventListener('click', closeModal);

el.voiceEnabled.addEventListener('change', () => {
  const enabled = !!el.voiceEnabled.checked;
  const token = markVoicePending('enabled');
  voiceState.enabled = enabled;
  renderVoice();
  runVoiceCommand('enabled', token, 'voice_set_enabled', { enabled }, '变声开关更新失败');
});

el.voicePreset.addEventListener('change', () => {
  const id = el.voicePreset.value;
  if (!id) return;
  // 未合并的本地参数在切换后仍由本窗口持有，随下一次 50 ms 合并发送。
  const token = markVoicePending('preset');
  voiceState.presetId = id;
  voiceState.presetName = voicePresetName(id) || voiceState.presetName;
  renderVoice();
  runVoiceCommand('preset', token, 'voice_select_preset', { id }, '切换变声预设失败');
});

function sendVoiceParams() {
  const token = voicePending.params;
  if (!token) return;
  const params = { ...voiceState.params };
  runVoiceCommand('params', token, 'voice_set_params', { params }, '人声参数更新失败');
}

function flushVoiceParams() {
  if (!voiceParamTimer) return;
  clearTimeout(voiceParamTimer);
  voiceParamTimer = null;
  sendVoiceParams();
}

/// 删除或恢复会替换整套参数，未发送的本地编辑不能在其后覆盖新状态。
function cancelVoiceParams() {
  clearTimeout(voiceParamTimer);
  voiceParamTimer = null;
  voicePending.params = 0;
  voiceLocalParams.clear();
}

function queueVoiceParams(changedKeys) {
  markVoicePending('params');
  for (const key of changedKeys) {
    voiceLocalParams.add(key);
    const control = VOICE_PARAM_CONTROLS.find(item => item.key === key);
    if (control) {
      voiceState.params[key] = boundedVoiceValue(el[control.input]);
      syncVoiceParamControl(control);
      continue;
    }
    const toggle = VOICE_SWITCH_CONTROLS.find(item => item.key === key);
    if (toggle) voiceState.params[key] = !!el[toggle.input].checked;
  }
  voiceState.modified = true;
  renderVoiceEditState();
  clearTimeout(voiceParamTimer);
  voiceParamTimer = setTimeout(() => {
    voiceParamTimer = null;
    sendVoiceParams();
  }, 50);
}

function boundedVoiceValue(input) {
  const value = Number(input.value);
  if (!Number.isFinite(value)) return 0;
  const min = Number(input.getAttribute('min'));
  const max = Number(input.getAttribute('max'));
  return Math.max(Number.isFinite(min) ? min : value,
    Math.min(Number.isFinite(max) ? max : value, value));
}

for (const control of VOICE_PARAM_CONTROLS) {
  el[control.input].addEventListener('input', () => queueVoiceParams([control.key]));
  el[control.input].addEventListener('blur', flushVoiceParams);
}
for (const control of VOICE_SWITCH_CONTROLS) {
  el[control.input].addEventListener('change', () => queueVoiceParams([control.key]));
}

function openVoiceNameEditor(mode) {
  voiceNameMode = mode;
  el.voiceNameEditor.classList.remove('hidden');
  el.voicePresetName.value = mode === 'rename'
    ? (voiceState.presetName || '')
    : `${voiceState.presetName || '人声'} 副本`;
  el.voicePresetName.focus();
  el.voicePresetName.select();
}

function closeVoiceNameEditor() {
  voiceNameMode = '';
  el.voiceNameEditor.classList.add('hidden');
}

el.voiceSaveAs.addEventListener('click', () => openVoiceNameEditor('saveAs'));
el.voiceRename.addEventListener('click', () => {
  if (!isUserVoicePreset()) return;
  openVoiceNameEditor('rename');
});
el.voiceNameCancel.addEventListener('click', closeVoiceNameEditor);
el.voiceNameConfirm.addEventListener('click', () => {
  const name = el.voicePresetName.value.trim();
  if (!name) {
    toast('预设名称不能为空', true);
    return;
  }
  const mode = voiceNameMode;
  if (mode !== 'saveAs' && mode !== 'rename') return;
  if (mode === 'rename' && !isUserVoicePreset()) return;
  // 另存必须包含尚未发送的最新参数，队列顺序保证先写参数再保存预设。
  flushVoiceParams();
  const token = markVoicePending('preset');
  closeVoiceNameEditor();
  if (mode === 'rename') {
    runVoiceCommand('preset', token, 'voice_rename_preset',
      { id: voiceState.presetId, name }, '重命名预设失败', loadVoicePresets);
  } else {
    runVoiceCommand('preset', token, 'voice_save_as_user_preset',
      { name }, '另存为用户预设失败', loadVoicePresets);
  }
});

el.voiceSaveChanges.addEventListener('click', () => {
  if (!isUserVoicePreset()) return;
  flushVoiceParams();
  const token = markVoicePending('preset');
  runVoiceCommand('preset', token, 'voice_save_preset', undefined,
    '保存修改失败', loadVoicePresets);
});

el.voiceDelete.addEventListener('click', () => {
  if (!isUserVoicePreset()) return;
  cancelVoiceParams();
  const token = markVoicePending('preset');
  runVoiceCommand('preset', token, 'voice_delete_preset',
    { id: voiceState.presetId }, '删除预设失败', async () => {
      await loadVoicePresets();
      if (!voiceState.enabled && voiceState.presetId === 'builtin:female') {
        toast('已删除用户预设，已回退女声并关闭变声');
      }
    });
});

el.voiceRestoreDraft.addEventListener('click', () => {
  if (!voiceState.draftAvailable) return;
  cancelVoiceParams();
  const token = markVoicePending('draft');
  runVoiceCommand('draft', token, 'voice_restore_draft', undefined,
    '恢复未保存修改失败');
});

// ─────────────────────────────── 播放

// Commands can finish out of order (especially preview decode). Completion
// feedback and refreshes belong only to the most recent audio operation.
let audioChoice = 0;
let operationReservationTail = Promise.resolve();
function newAudioChoice() {
  // A newer operation owns the busy UI too; an abandoned startup may still be
  // waiting for a disk save or decode and must not leave the toggle locked.
  el.btnToggle.disabled = false;
  return ++audioChoice;
}

async function invokeAudio(command, args, choice) {
  // Reserve in user order, but do not put decoding/opening or their responses
  // in this queue. Stop must be able to cancel a slow preview immediately.
  const reserved = operationReservationTail.catch(() => {}).then(async () => {
    if (choice !== audioChoice) return null;
    return invoke('begin_playback_operation');
  });
  operationReservationTail = reserved;
  const operation = await reserved;
  if (operation == null || choice !== audioChoice) return;
  return invoke(command, { ...args, operation });
}

async function confirmedAudio(result, choice) {
  if (choice !== audioChoice) return false;
  if (result !== false) return true;
  // A newer hotkey/phone/menu action can cancel the host ticket without
  // changing this WebView's local choice. Follow native state in that case.
  await poll(choice);
  if (choice === audioChoice) {
    render();
    toast('播放状态已被其他操作更新');
  }
  return false;
}

function clearPlaybackFeedback() {
  if (state.lastStatus) state.lastStatus = {
    ...state.lastStatus, playing: false, active_slot: -1, playhead_secs: 0,
    preview_playing: false, preview_sound_id: null, preview_playhead_secs: 0,
  };
  // A confirmed stop also clears feedback when the following status request
  // fails. Later successful status always takes priority over this snapshot.
  const slots = new Map(baseSlots().map(slot => [slot.index, slot]));
  for (const card of el.grid.children) {
    const slot = slots.get(+card.dataset.slot);
    if (slot) updateCardPlayback(card, slot);
  }
  updatePlayer();
}

async function play(slot) {
  if (!MixerLibrary.canPlay(state)) { toast("先开启音效再播放", true); return; }
  const choice = newAudioChoice();
  try {
    await flushGainUpdates();
    if (choice !== audioChoice) return;
    const replacing = !!MixerLibrary.currentPlayback(state);
    const ok = await invokeAudio('play', { slot }, choice);
    if (choice !== audioChoice) return;
    if (ok === false) {
      await confirmedAudio(false, choice);
      if (choice === audioChoice) toast('未接纳这次播放，播放状态已刷新', true);
      return;
    }
    await poll(choice);
    if (choice !== audioChoice) return;
    toast(replacing ? '已切换播放 · 输出至虚拟设备' : '正在播放 · 输出至虚拟设备');
  } catch (e) {
    if (choice === audioChoice) toast(e, true);
  }
}

async function stopAll() {
  const choice = newAudioChoice();
  try {
    if (!await confirmedAudio(await invokeAudio('stop_all', {}, choice), choice)) return;
    clearPlaybackFeedback();
    await poll(choice);
    if (choice !== audioChoice) return;
    toast('已停止音效播放和试听（麦克风转发不受影响）');
  }
  catch (e) { if (choice === audioChoice) toast(e, true); }
}

// ─────────────────────────────── 渲染

function baseSlots() {
  return MixerLibrary.baseSlots(state, hotkeyList());
}

/// 按搜索词与标签过滤。**保留原始 index**（data-slot），
/// 否则筛选后点击会播错音效。
function visibleSlots() {
  const q = el.inSearch.value.trim().toLowerCase();
  const tag = state.activeTag;
  return baseSlots().filter(s => {
    const it = MixerLibrary.itemForSlot(state, s);
    if (tag && !itemTags(it).includes(tag)) return false;
    if (q) {
      const hay = (s.name + ' ' + itemTags(it).join(' ') + ' ' + (s.hotkey || '')).toLowerCase();
      if (!hay.includes(q)) return false;
    }
    return true;
  });
}

function renderTags() {
  const counts = new Map();
  for (const it of state.items) for (const tag of itemTags(it)) counts.set(tag, (counts.get(tag) || 0) + 1);
  el.tagbar.innerHTML = '';
  const entries = [[null, state.items.length], ...[...counts.entries()].sort()];
  for (const [tag, n] of entries) {
    const chip = document.createElement('button');
    chip.className = 'tag' + (state.activeTag === tag ? ' on' : '');
    chip.innerHTML = `${esc(tag ?? '全部音效')}<span class="n">${n}</span>`;
    chip.setAttribute('aria-pressed', String(state.activeTag === tag));
    chip.addEventListener('click', () => {
      state.activeTag = state.activeTag === tag ? null : tag;
      render();
      [...el.tagbar.children].find(child => child.getAttribute('aria-pressed') === 'true')?.focus();
    });
    el.tagbar.appendChild(chip);
  }
}
function render() {
  const focusedCard = document.activeElement?.closest?.('.card');
  const focusedId = focusedCard?.dataset.soundId;
  const focusedClass = document.activeElement?.classList.contains('sound-settings') ? '.sound-settings'
    : document.activeElement?.classList.contains('prev') ? '.prev' : '.play-trigger';
  renderTags();
  const slots = visibleSlots();
  el.grid.innerHTML = '';
  if (!slots.length) {
    const empty = document.createElement('div');
    empty.className = 'empty';
    empty.innerHTML = baseSlots().length
      ? '<h3>没有匹配的音效</h3><p>试试其他搜索词，或清除标签筛选。</p><button id="clearFilter">清除筛选</button>'
      : '<h3>添加你的第一条音效</h3><p>支持本地音频文件和任意可读取的文件夹。<br>连接虚拟麦克风后，即可在互动中快速触发。</p><button id="emptyAdd" class="primary">添加音频</button><button id="emptyFolder" class="quiet">从文件夹载入</button>';
    el.grid.appendChild(empty);
    empty.querySelector('#clearFilter')?.addEventListener('click', () => {
      el.inSearch.value = ''; state.activeTag = null; render(); el.inSearch.focus();
    });
    empty.querySelector('#emptyAdd')?.addEventListener('click', () => el.btnAdd.click());
    empty.querySelector('#emptyFolder')?.addEventListener('click', () => setSetupOpen(true));
  }
  for (const slot of slots) {
    const item = MixerLibrary.itemForSlot(state, slot);
    const tags = itemTags(item);
    const card = document.createElement('article');
    card.className = 'card';
    card.dataset.slot = slot.index;
    card.dataset.soundId = item?.id ?? slot.id;
    card.innerHTML = `
      <button class="play-trigger" aria-label="播放给对方：${esc(slot.name)}" title="${esc(slot.name)} · 点击从头播放">
        <span class="card-top"><span class="idx">${slot.index + 1}</span>${slot.hotkey ? `<kbd class="key" title="${esc(slot.hotkey)}">${esc(slot.hotkey)}</kbd>` : '<span class="idx">未绑热键</span>'}</span>
        <span class="name">${esc(slot.name)}</span>
        <span class="card-info"><span class="dur">${slot.duration ? clockTime(slot.duration) : '启动后读取时长'}</span>${tags.length ? `<span class="tags" title="${esc(tags.join('、'))}">· ${esc(tags.join('、'))}</span>` : ''}</span>
      </button>
      <div class="meta"><span class="play-label">播放给对方</span><div class="card-actions"><button class="prev" aria-label="仅自己试听：${esc(slot.name)}" title="试听仅自己可听，会替换当前音效">试听</button><button class="sound-settings" aria-label="${esc(slot.name)} 的音量与详情" aria-haspopup="dialog" title="音量、名称、标签和热键">…</button></div></div>
      <div class="playback-state"></div><div class="bar" aria-hidden="true"></div>`;
    // 容器接收主按钮的冒泡；试听和详情各自阻止冒泡。
    card.addEventListener('click', () => play(slot.index));
    card.querySelector('.prev').addEventListener('click', event => {
      event.stopPropagation();
      return preview(slot);
    });
    card.querySelector('.sound-settings').addEventListener('click', event => {
      event.stopPropagation();
      openSoundDetails(item?.id ?? slot.id);
    });
    updateCardPlayback(card, slot);
    el.grid.appendChild(card);
  }
  updateCount();
  updatePlayer();
  syncSoundFields();
  if (focusedId && !activeModal) {
    [...el.grid.children].find(card => card.dataset.soundId === focusedId)?.querySelector(focusedClass)?.focus();
  }
}
async function preview(slot) {
  const item = MixerLibrary.itemForSlot(state, slot);
  const file = slot.file || item?.file;
  if (!file) { toast('找不到这个音效的文件路径', true); return; }
  const choice = newAudioChoice();
  try {
    const replacing = !!MixerLibrary.currentPlayback(state);
    toast('加载试听…');
    await flushGainUpdates();
    if (choice !== audioChoice) return;
    if (!await confirmedAudio(await invokeAudio('preview', {
      file, gainDb: item?.gain ?? 0, soundId: item?.id ?? slot.id,
    }, choice), choice)) return;
    await poll(choice);
    if (choice !== audioChoice) return;
    toast(replacing ? '已切换试听（仅自己听，对方听不到）' : '试听中（对方听不到）');
  } catch (error) { if (choice === audioChoice) toast('试听失败：' + error, true); }
}

let soundEditingId = null;
let initialSoundTags = [];
let initialSoundTagText = '';
function soundEditingItem() { return state.items.find(item => item.id === soundEditingId); }
function syncSoundFields() {
  const item = soundEditingItem();
  const readOnly = state.running || !item;
  for (const field of [el.soundName, el.soundTags, el.soundHotkey, el.btnSoundLeft, el.btnSoundRight, el.btnSoundRemove]) field.disabled = readOnly;
  if (item && !readOnly) {
    const index = state.items.indexOf(item);
    el.btnSoundLeft.disabled = index === 0;
    el.btnSoundRight.disabled = index === state.items.length - 1;
  }
}
function openSoundDetails(soundId) {
  soundEditingId = soundId;
  const item = soundEditingItem();
  if (!item) { toast('这个音效已不在库中', true); return; }
  el.soundTitle.textContent = item.name;
  el.soundFile.textContent = item.file;
  el.soundGain.value = item.gain;
  el.soundGainValue.textContent = formatGain(item.gain);
  el.soundGain.setAttribute('aria-label', `${item.name} 音效音量`);
  el.soundGain.setAttribute('aria-valuetext', formatGain(item.gain));
  el.soundGain.title = `音效音量 ${formatGain(item.gain)} · 试听与播放均实时生效`;
  el.soundName.value = item.name;
  initialSoundTags = itemTags(item);
  initialSoundTagText = initialSoundTags.join(',');
  el.soundTags.value = initialSoundTagText;
  el.soundHotkey.value = hotkeyList()[state.items.indexOf(item)] || '';
  syncSoundFields();
  showModal(el.soundOverlay, el.btnSoundClose);
}
el.btnSoundClose.addEventListener('click', closeModal);
el.soundGain.addEventListener('input', () => {
  const db = +el.soundGain.value;
  setSoundGain(soundEditingId, db);
  el.soundGainValue.textContent = formatGain(db);
  el.soundGain.setAttribute('aria-valuetext', formatGain(db));
  el.soundGain.title = `音效音量 ${formatGain(db)} · 试听与播放均实时生效`;
});
el.soundGain.addEventListener('change', () => saveCurrentConfig());
el.btnSoundSave.addEventListener('click', async () => {
  const generation = modalGeneration;
  const item = soundEditingItem();
  if (!item) { closeModal(); return; }
  if (!state.running) {
    item.name = el.soundName.value.trim() || item.name;
    item.tags = MixerLibrary.tagsAfterEdit(initialSoundTags, initialSoundTagText, el.soundTags.value);
    const keys = hotkeyList();
    while (keys.length < state.items.length) keys.push('');
    keys[state.items.indexOf(item)] = el.soundHotkey.value.trim();
    el.inKeys.value = keys.join(',');
  }
  if (!await saveCurrentConfig()) return;
  if (generation === modalGeneration) closeModal();
  render();
});
for (const [button, delta] of [[el.btnSoundLeft, -1], [el.btnSoundRight, 1]]) {
  button.addEventListener('click', async () => {
    const item = soundEditingItem();
    if (!item || state.running) return;
    moveItem(state.items.indexOf(item), delta);
    el.soundHotkey.value = hotkeyList()[state.items.indexOf(item)] || '';
    await saveCurrentConfig();
  });
}
el.btnSoundRemove.addEventListener('click', async () => {
  const item = soundEditingItem();
  if (!item || state.running) return;
  removeItem(state.items.indexOf(item));
  closeModal();
  await saveCurrentConfig();
});

// ─────────────────────────────── 音效库操作

function applyLibraryEdit(edit) {
  state.items = edit.items;
  el.inKeys.value = edit.keys.join(',');
}

function removeItem(idx) {
  const it = state.items[idx];
  if (!it) return;
  if (state.running) { toast('先点「关闭音效」再改音效库', true); return; }
  applyLibraryEdit(MixerLibrary.remove(state.items, hotkeyList(), idx));
  render();
  toast(`已移除「${it.name}」（文件没删）`);
}

function moveItem(idx, delta) {
  const to = idx + delta;
  if (state.running) { toast('先点「关闭音效」再改音效库', true); return; }
  if (to < 0 || to >= state.items.length) return;
  applyLibraryEdit(MixerLibrary.move(state.items, hotkeyList(), idx, to));
  render();
}

function formatGain(db) {
  const value = Math.round(db * 10) / 10;
  return `${value > 0 ? '+' : ''}${value} dB`;
}

// 即刻更新正在响的音效或试听。每个 ID 独立串行，只合并其等待中的最新值。
const sendSoundGain = MixerLibrary.gainSender((soundId, db) =>
  invoke('set_sound_gain', { soundId, db })
    .catch(error => toast('音效音量更新失败：' + error, true)));
function setSoundGain(soundId, db) {
  if (!soundId || !Number.isFinite(db)) return;
  const item = state.items.find(item => item.id === soundId);
  if (item) item.gain = db;
  return sendSoundGain(soundId, db);
}

function updateCount() {
  const n = state.items.length;
  el.count.textContent = `${n} 条`;
}

function updateCardPlayback(card, slot) {
  const playback = MixerLibrary.playbackForSlot(state, slot);
  const preview = playback.kind === 'preview';
  card.classList.toggle('playing', playback.active);
  card.classList.toggle('previewing', preview);
  const button = card.querySelector('.prev');
  if (button) {
    button.classList.toggle('playing', preview);
    button.textContent = preview ? '试听中' : '试听';
  }
  const visibleLabel = card.querySelector('.play-label');
  if (visibleLabel) visibleLabel.textContent = playback.active
    ? preview ? '试听中' : '播放中' : '播放给对方';
  card.querySelector('.play-trigger')?.setAttribute('aria-pressed', String(playback.active && !preview));
  const label = card.querySelector('.playback-state');
  if (label) label.textContent = playback.active
    ? preview ? '试听中 · 仅自己听' : '播放中 · 输出至虚拟设备' : '';
  const bar = card.querySelector('.bar');
  if (bar) {
    bar.style.width = playback.progress + '%';
    bar.title = preview ? '试听进度' : '播放进度';
  }
}


function clockTime(seconds) {
  if (!Number.isFinite(seconds) || seconds < 0) return '—';
  const whole = Math.floor(seconds);
  return `${Math.floor(whole / 60)}:${String(whole % 60).padStart(2, '0')}`;
}
function updatePlayer() {
  const current = MixerLibrary.currentPlayback(state);
  const item = current && state.items.find(item => item.id === current.soundId);
  const slot = current && state.slots.find(slot => slot.index === current.slot);
  el.player.dataset.kind = current?.kind || 'idle';
  el.playerName.textContent = current ? item?.name || slot?.name || '当前音效' : '准备好下一条音效';
  el.playerName.title = el.playerName.textContent;
  el.playerMode.textContent = current
    ? current.kind === 'preview' ? '试听中 · 仅自己可听 · 只播一次'
      : `播放中 · 发送至虚拟麦克风${el.inLoop.checked ? ' · 循环' : ''}`
    : state.standby ? '音效已关闭 · 麦克风仍在转发' : '点卡片播放，或先试听确认';
  const progress = current?.duration > 0 && Number.isFinite(current.playhead)
    ? Math.max(0, Math.min(100, current.playhead / current.duration * 100)) : 0;
  el.playerProgress.style.width = progress + '%';
  el.playerTime.textContent = current ? `${clockTime(current.playhead)} / ${clockTime(current.duration)}` : '— / —';
  const connected = state.running || state.standby;
  el.signalLabel.textContent = connected ? '混合信号输出中' : '音频设备未启动';
}

function esc(s) {
  return String(s).replace(/[&<>"]/g, c =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}

// ─────────────────────────────── 状态轮询

let requestedStatus = 0;
let appliedStatus = 0;
async function poll(choice = audioChoice) {
  if (choice !== audioChoice) return;
  const request = ++requestedStatus;
  const voiceVersionAtRequest = voiceEditVersion;
  try {
    const st = await invoke('status');
    // 命令会立即刷新状态；较早的轮询迟到时不能把当前音效切回旧卡片。
    if (!st || choice !== audioChoice || request < appliedStatus) return;
    appliedStatus = request;
    const wasRunning = state.running;
    state.running = st.running;
    state.lastStatus = st;
    state.slots = st.slots || [];
    if (st.voice) {
      applyVoiceSnapshot(st.voice, { fromPoll: true, requestVersion: voiceVersionAtRequest });
    }

    // 顶栏：三种状态必须一眼分得清 ——
    //   待机时**麦克风仍在转发**，这点写出来，否则用户会以为"停了对方就听不到我"
    const wasStandby = state.standby;
    state.standby = !st.running && !!st.passthrough;
    // 待机状态变了要刷新按钮文案（轮询是唯一能发现"从别处停了"的途径）
    if (wasStandby !== state.standby) syncToggle();
    el.dot.className = 'dot ' + (!st.running ? (st.passthrough ? 'standby' : 'off')
      : st.overruns > 0 || st.underruns > 0 ? 'warn' : 'ok');
    el.headline.textContent = !st.running
      ? (state.standby
          ? '待机 · 人声转发中'
          : '未启动')
      : '音效已开启';
    el.headline.title = st.running
      ? `${st.device} · 已运行 ${fmtTime(st.uptime_secs)} · ${st.hotkeys_active ? '热键已绑定' : '热键未绑定'}`
      : state.standby ? '麦克风仍在转发，音效已关闭' : '设备未启动';

    // ── 麦克风一直没声音？ ──
    //
    // 这是最典型的"无声失败"：引擎在跑、界面一切正常，但对方就是听不到。
    // 原因通常是权限或"默认通信设备"选错了 —— 用户自己很难想到。
    //
    // 判定条件保守一点，避免误报：
    //   · 引擎在跑、当前没在放音效
    //   · 输出电平**正好**是数字静音的地板（真实麦克风总有底噪，不会正好 -100）
    //   · 连续 10 秒都这样
    if (st.running && !st.playing && !state.micHelpDismissed) {
      const silent = (st.out_peak_db ?? -100) <= -99.9;
      state.silentSecs = silent ? (state.silentSecs || 0) + 1 : 0;
      if (state.silentSecs >= 10 && !state.micHelpShown) {
        state.micHelpShown = true;
        showMicHelp(true);
      }
    } else if (!st.running) {
      state.silentSecs = 0;
    }

    // 程序内部 panic：必须让用户知道 —— 打包后没有控制台，不说就等于没发生
    if (st.last_panic && st.last_panic !== state.lastPanicSeen) {
      state.lastPanicSeen = st.last_panic;
      hint('程序内部出错了：' + st.last_panic +
           ' —— 请查看诊断日志；音频正在尝试自动恢复。', 'error');
      toast('程序内部出错（已兜住，正在自动恢复）', true);
    }

    // 音频卡死 / 已自动重启
    if (st.audio_stalled && st.audio_stalled !== state.lastStalled) {
      state.lastStalled = st.audio_stalled;
      hint(st.audio_stalled, 'error');
      toast('音频卡了一下，已自动处理', true);
    } else if (!st.audio_stalled && state.lastStalled) {
      state.lastStalled = null;
    } else if (!st.last_panic) {
      state.lastPanicSeen = null;
    }

    // 设备掉线：这是**正在游戏里也必须要看到**的报警
    if (st.device_lost && st.device_lost !== state.lastDeviceLost) {
      state.lastDeviceLost = st.device_lost;
      hint(st.device_lost, 'error');
      toast('设备掉线！' + st.device_lost, true);
    } else if (!st.device_lost && state.lastDeviceLost) {
      state.lastDeviceLost = null;
    }

    // 监听有独立的健康状态；同一错误持续存在时只提醒一次。
    if (st.monitor_error && st.monitor_error !== state.lastMonitorError) {
      state.lastMonitorError = st.monitor_error;
      hint('本机监听输出异常：' + st.monitor_error +
           ' —— 请检查监听设备并重新开启监听，麦克风和音效的主输出继续运行。', 'error');
      toast('本机监听出错，请检查监听设备；主输出继续运行', true);
    } else if (!st.monitor_error) {
      state.lastMonitorError = null;
    }

    // 底栏指标
    // 被跳过的坏文件：必须告诉用户，否则他会以为"明明放进去了怎么没有"
    if (st.running && (st.skipped || []).length) {
      hint(
        `${st.skipped.length} 个文件无法解码，已跳过（其余正常）：` +
        st.skipped.slice(0, 3).join('、') +
        (st.skipped.length > 3 ? ` 等 ${st.skipped.length} 个` : ''),
        'error'
      );
    }

    if (st.running) {
      const bad = (st.underruns > 0 || st.overruns > 0);
      el.metrics.innerHTML =
        `水位 <b>${st.ring_ms.toFixed(1)}ms</b> · ` +
        `欠载 <b class="${st.underruns ? 'bad' : ''}">${st.underruns}</b> · ` +
        `溢出 <b class="${st.overruns ? 'bad' : ''}">${st.overruns}</b> · ` +
        `限幅 <b>${st.limiter_db.toFixed(1)}dB</b>` +
        (bad ? ' · <span class="bad">建议调大缓冲</span>' : '');
    } else {
      el.metrics.textContent = st.last_error
      ? '上次错误：' + st.last_error
      : '关闭窗口不会退出 · 从 Dock（macOS）或托盘图标重新打开';
      if (st.last_error) hint('启动失败：' + st.last_error, 'error');
    }

    el.healthSummary.textContent = st.running
      ? (st.underruns > 0 || st.overruns > 0 ? '有缓冲告警' : '音频状态正常')
      : state.standby ? '人声转发中' : '音频状态';
    el.healthDetails.classList.toggle('warn', !!st.underruns || !!st.overruns || !!st.last_error);

    // 显示本程序的虚拟输出强度，目标应用的接收情况需要另行确认。
    updateMeter(st);

    // 手机遥控地址：优先显示局域网地址（手机要用的就是它）
    const urls = (st.remote_urls || []).filter(u => !u.includes('127.0.0.1'));
    const best = urls[0] || (st.remote_urls || [])[0];
    el.remote.textContent = best ? best.replace('http://', '') : '';
    el.remote.title = best ? `手机浏览器打开 ${best}\n（需与电脑在同一局域网）` : '';
    el.remote.dataset.url = best || '';
  

    if (wasRunning !== state.running) { syncToggle(); render(); }

    // 主播放和试听共用唯一当前音效；试听也可在未启动/待机时运行。
    const slots = new Map(baseSlots().map(slot => [slot.index, slot]));
    for (const card of el.grid.children) {
      const slot = slots.get(+card.dataset.slot);
      if (slot) updateCardPlayback(card, slot);
    }
    syncConnection();
    syncSoundFields();
    syncVoicePreview();
    updatePlayer();
  } catch (e) { /* 窗口关闭时忽略 */ }
}

// dBFS → 0~100% 宽度（-60dB 起算）
const METER_FLOOR = -60;
const pct = db => Math.max(0, Math.min(100, ((db - METER_FLOOR) / -METER_FLOOR) * 100));

function updateMeter(st) {
  if (!state.running && !state.standby) { el.meter.classList.add('off'); return; }
  el.meter.classList.remove('off');
  const rms = st.out_rms_db ?? -100;
  const peak = st.out_peak_db ?? -100;
  const bar = el.meter.querySelector('.bar');
  const pk = el.meter.querySelector('.peak');
  bar.style.width = pct(rms) + '%';
  pk.style.left = pct(peak) + '%';
  // -30dB 以下算太弱（语音软件可能当噪声滤掉）；-6dB 以上接近上限
  bar.className = 'bar' + (rms < -30 ? ' quiet' : rms > -6 ? ' hot' : '');
  el.meter.title =
    `送往虚拟麦克风：峰值 ${peak.toFixed(1)} dBFS · RMS ${rms.toFixed(1)} dBFS\n` +
    `绿色区间是 −18 ~ −12 dBFS —— 说话时落在这里最稳`;
}

function fmtTime(sec) {
  const s = Math.floor(sec);
  const m = Math.floor(s / 60);
  return m ? `${m}分${s % 60}秒` : `${s}秒`;
}

// ─────────────────────────────── 事件绑定

function syncLoopHelp() {
  el.loopHelp.textContent = el.inLoop.checked
    ? '循环已开启：正式播放与监听重复播放；取消后当前一遍结束即停止。试听始终只播放一次。'
    : '循环已关闭：正式播放与监听在当前一遍结束后停止。试听始终只播放一次。';
  updatePlayer();
}

let loopUpdateTail = Promise.resolve();
let loopChoice = 0;
function queueLoopUpdate(looping) {
  loopUpdateTail = loopUpdateTail.catch(() => {}).then(() => invoke('set_looping', { looping }));
  return loopUpdateTail;
}
el.inLoop.addEventListener('change', async () => {
  const choice = ++loopChoice;
  const audioWhenEdited = audioChoice;
  const looping = !!el.inLoop.checked;
  const hasSession = state.running || state.standby;
  syncLoopHelp();
  // 保存的是这次选择的快照；先等早前的音频更新，避免释放后其迟到覆盖新配置。
  const updated = hasSession
    ? queueLoopUpdate(looping)
    : loopUpdateTail.catch(() => {});
  const saved = saveCurrentConfig(updated);
  try {
    await updated;
    await saved;
    if (choice === loopChoice && audioWhenEdited === audioChoice) toast(looping
      ? '已开启循环播放（试听仍只播放一次）'
      : '已取消循环，当前一遍结束即停止');
  } catch (error) {
    if (choice === loopChoice && audioWhenEdited === audioChoice) toast('循环设置更新失败：' + error, true);
  }
});

el.btnToggle.addEventListener('click', () => state.running ? stop() : start());
el.btnStopAll.addEventListener('click', stopAll);
el.btnBrowse.addEventListener('click', async () => {
  const p = await invoke('pick_dir');
  if (p) { el.inDir.value = p; await scanDir(); }
});
el.btnScan.addEventListener('click', () => scanDir());
el.btnAdd.addEventListener('click', async () => {
  if (state.running) { toast('先点「关闭音效」再改音效库', true); return; }
  const picked = (await invoke('add_files')) || [];
  if (!picked.length) return;
  if (state.running) { toast('先点「关闭音效」再改音效库', true); return; }
  const previousCount = state.items.length;
  applyLibraryEdit(MixerLibrary.appendFiles(state.items, hotkeyList(), picked, makeItem));
  const added = state.items.length - previousCount;
  render();
  await saveCurrentConfig();
  hint(added ? `添加了 ${added} 个音效` : '这些文件已经在列表里了', added ? 'ok' : '');
});
el.btnClear.addEventListener('click', () => {
  if (state.running) { toast('先点「关闭音效」再改音效库', true); return; }
  state.items = []; el.inKeys.value = '';
  render(); hint('已清空音效列表');
  saveCurrentConfig();
});
el.inDir.addEventListener('keydown', e => { if (e.key === 'Enter') scanDir(); });
el.btnSelfTest.addEventListener('click', runSelfTest);

// 手动打开麦克风排查（自动面板被关掉之后还能再打开）
el.btnMicHelp.addEventListener('click', () => {
  state.micHelpDismissed = false;
  showMicHelp();
});
el.btnCalib.addEventListener('click', calibrateMic);
el.btnRevertMic.addEventListener('click', async () => {
  try {
    const r = await invoke('revert_to_real_mic');
    if (r && r.ok) {
      toast(`系统默认麦克风已切回「${r.device}」`);
      hint(`${r.note}`, 'ok');
    } else {
      toast(`切到「${r?.device || '?'}」没生效，当前仍是「${r?.current || '?'}」`, true);
    }
    await loadDevices();
  } catch (e) {
    toast('切回失败：' + e, true);
  }
});

// ── 麦克风电平校准 ─────────────────────────────────────────────
//
// 思路：把麦克风增益归零，静置 5 秒采样"送往虚拟麦克风的峰值"，
// 再算出要补多少才能落在目标带（-18~-12 dBFS）中间。
//
// 为什么不用新开一条测量通路：**没有音效在放时，输出电平就是麦克风电平** ——
// 现成的电平表已经够了，不必再加一套采集。
const CALIB_TARGET_PEAK = -15.0;   // 目标带 -18~-12 的正中
const CALIB_SECS = 5;

let calibrating = false;

async function calibrateMic() {
  if (calibrating) return;
  if (!state.running) {
    toast('先点「启动」，再校准', true);
    return;
  }
  calibrating = true;
  openUtility(el.calibPanel, '校准麦克风');
  const btn = el.btnCalib;
  const oldText = btn.textContent;
  btn.disabled = true;
  const setHint2 = (t, cls) => {
    el.calibHint.textContent = t;
    el.calibHint.className = 'calibhint' + (cls ? ' ' + cls : '');
  };
  try {
    const savedMic = parseFloat(el.inMicGain.value || '0');
    // 1. 停掉音效 + 增益归零 —— 这样测到的就是纯粹的麦克风电平
    await invoke('stop_all');
    await invoke('set_gains', {
      micDb: 0,
      sfxDb: parseFloat(el.inSfxGain.value || '-6'),
    });
    await new Promise(r => setTimeout(r, 400));

    // 2. 采样
    const peaks = [];
    const t0 = Date.now();
    while (Date.now() - t0 < CALIB_SECS * 1000) {
      const st = state.lastStatus;
      if (st && st.running) peaks.push(st.out_peak_db);
      const left = Math.ceil(CALIB_SECS - (Date.now() - t0) / 1000);
      btn.textContent = `请正常说话… ${left}s`;
      setHint2(`剩余 ${left} 秒，请对着麦克风正常说话。`);
      await new Promise(r => setTimeout(r, 200));
    }

    if (!peaks.length) {
      setHint2('测量失败：没读到电平', 'bad');
      await invoke('set_gains', { micDb: savedMic, sfxDb: parseFloat(el.inSfxGain.value || '-6') });
      return;
    }
    const measured = Math.max(...peaks);

    // 3. 判断 + 算补偿
    if (measured < -55) {
      setHint2(`几乎没测到声音（峰值 ${measured.toFixed(1)} dBFS）—— 请对着麦克风正常说话后重试`, 'bad');
      await invoke('set_gains', { micDb: savedMic, sfxDb: parseFloat(el.inSfxGain.value || '-6') });
      return;
    }
    const limiterHit = (state.lastStatus?.limiter_db || 0) > 0.5;
    let gain = Math.round(CALIB_TARGET_PEAK - measured);
    gain = Math.max(-24, Math.min(36, gain));   // 夹到合理范围，别搞出 +80dB 这种

    el.inMicGain.value = gain;
    syncGainControls();
    await invoke('set_gains', { micDb: gain, sfxDb: parseFloat(el.inSfxGain.value || '-6') });

    const band = measured + gain;
    const saved = await saveCurrentConfig();
    setHint2(
      `你的麦克风峰值 ${measured.toFixed(1)} dBFS → 补 ${gain > 0 ? '+' : ''}${gain} dB ` +
      `→ 预计 ${band.toFixed(1)} dBFS` +
      (limiterHit ? '（刚才有削波，建议说话离麦克风远一点）' : '') +
      (saved ? '。设置已保存，可重新校准。' : '。本次校准已生效，但未保存，请从连接设置重试保存。'),
      saved ? 'ok' : 'bad'
    );
    if (saved) toast(`校准完成：麦克风增益 ${gain > 0 ? '+' : ''}${gain} dB`);
  } catch (e) {
    setHint2('校准失败：' + e, 'bad');
  } finally {
    btn.disabled = false;
    btn.textContent = oldText;
    calibrating = false;
  }
}
el.btnRelease.addEventListener('click', async () => {
  const choice = newAudioChoice();
  try {
    if (!await confirmedAudio(await invokeAudio('release', {}, choice), choice)) return;
  } catch (error) {
    if (choice === audioChoice) toast('释放音频设备失败：' + error, true);
    return;
  }
  if (choice !== audioChoice) return;
  state.running = false;
  state.standby = false;
  state.slots = [];
  clearPlaybackFeedback();
  syncToggle();
  render();
  await poll(choice);
  if (choice !== audioChoice) return;
  if (state.running || state.standby) {
    toast('音频状态已被其他操作更新');
    return;
  }
  toast('已彻底释放音频设备');
  hint('设备已释放。如果目标应用的麦克风仍选着虚拟声卡，对方现在听不到你 —— 要么重新「启动」，' +
       '要么把目标应用的麦克风改回真实麦克风。', 'error');
});

/// 这台机器上有没有虚拟声卡？
///
/// 没有的话整个软件都用不了（游戏只能从"麦克风"读声音，而虚拟声卡就是那个麦克风），
/// 所以这种情况必须**主动告诉用户怎么装**，而不是让他对着空的下拉框发愣。
async function checkVirtualDevice() {
  const has = state.devices.some(d => d.is_virtual && d.direction.includes('输出'));
  el.btnDriverHelp.classList.toggle('hidden', has);
  if (!has) {
    hint('未找到虚拟音频设备，点击右侧「安装虚拟音频设备」查看平台指引。', 'error');
    await renderVdevHelp();
  }
  return has;
}

/// 按**当前平台**渲染"怎么装虚拟声卡"的引导。
///
/// 前端两端共用，但装驱动这件事在 macOS 和 Windows 上完全不一样
/// （BlackHole vs VB-CABLE），所以由后端告诉它该显示哪一套。
async function renderVdevHelp() {
  if (el.vdevHelp.dataset.rendered === '1') return;
  let info;
  try {
    info = await invoke('platform_info');
  } catch {
    return;
  }
  const esc2 = (t) => String(t).replace(/[&<>"]/g, c =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

  let html = `<div class="vdh-title">${esc2(info.title)}</div>` +
    `<p>语音、会议、直播和游戏应用通过「麦克风」读取声音。虚拟声卡的作用就是：` +
    `<b>我们把人声和音频写进去，目标应用从它读出来</b> —— 声音于是进了你的麦克风。</p>` +
    `<p class="vdh-pick">推荐装 <b>${esc2(info.driver)}</b>：</p>` +
    `<ol class="vdh-steps">` +
    info.steps.map(t => `<li>${mdBold(t)}</li>`).join('') +
    `</ol>`;

  if (info.cmd) {
    html += `<div class="vdh-cmd"><code id="vdhCmd">${esc2(info.cmd)}</code>` +
            `<button id="btnCopyCmd" class="ghost">复制</button></div>`;
  } else {
    html += `<p class="vdh-alt">下载：<a href="${esc2(info.url)}" id="vdhLink">${esc2(info.url)}</a></p>`;
  }
  if (info.naming) html += `<p class="vdh-naming">${esc2(info.naming)}</p>`;
  if (info.alts) html += `<p class="vdh-alt">${esc2(info.alts)}</p>`;
  html += `<div class="vdh-actions"><button id="btnRecheck" class="ghost">重新检测</button></div>`;

  el.vdevHelp.innerHTML = html;
  el.vdevHelp.dataset.rendered = '1';

  // 重新挂事件（内容刚被换掉，原来的监听器没了）
  const copy = el.vdevHelp.querySelector('#btnCopyCmd');
  if (copy) copy.addEventListener('click', copyDriverCmd);
  const recheck = el.vdevHelp.querySelector('#btnRecheck');
  if (recheck) recheck.addEventListener('click', async () => {
    el.vdevHelp.dataset.rendered = '';
    await loadDevices();
    if (await checkVirtualDevice()) toast('检测到虚拟声卡了');
    else toast('还是没有 —— 装完驱动记得重启一次', true);
  });
}

/// 麦克风收不到声音时，按平台给出排查步骤。
///
/// 内容是后端给的（`platform_info.mic_privacy`）—— macOS 和 Windows 的设置路径
/// 完全不一样，不该在前端里堆 if。
/// 把 `**粗体**` 转成 <b>。引导文案和步骤都要用，所以抽出来。
function mdBold(t) {
  return esc(t).replace(/\*\*(.+?)\*\*/g, '<b>$1</b>');
}

async function showMicHelp(automatic = false) {
  const generation = modalGeneration;
  if (el.micHelp.dataset.rendered !== '1') {
    let info;
    try {
      info = await invoke('platform_info');
    } catch {
      return;
    }
    const mp = info.mic_privacy;
    if (!mp) return;
    const esc2 = (t) => String(t).replace(/[&<>"]/g, c =>
      ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

    el.micHelp.innerHTML =
      `<div class="vdh-title">${esc2(mp.title)}</div>` +
      `<p>${mdBold('引擎在跑，但送进麦克风的信号一直是**完全没有声音**（真实麦克风总有一点底噪）。' +
                    '对方现在听不到你。按下面几步查一下：')}</p>` +
      `<ol class="vdh-steps">` +
      mp.steps.map(t => `<li>${mdBold(t)}</li>`).join('') +
      `</ol>` +
      `<div class="vdh-actions">` +
      (mp.settings_page ? `<button id="btnOpenMicSet" class="ghost">${esc2(mp.url_label || '打开设置')}</button>` : '') +
      `<button id="btnMicHelpLater" class="vdh-dismiss">我知道了，先不管</button>` +
      `</div>`;
    el.micHelp.dataset.rendered = '1';

    const open = el.micHelp.querySelector('#btnOpenMicSet');
    if (open) open.addEventListener('click', async () => {
      try { await invoke('open_system_settings', { page: mp.settings_page }); }
      catch (e) { toast('打不开设置页（' + e + '），手动进系统设置找一下', true); }
    });
    const later = el.micHelp.querySelector('#btnMicHelpLater');
    if (later) later.addEventListener('click', () => {
      state.micHelpDismissed = true;
      el.micHelp.classList.add('hidden');
      if (activeModal === el.utilityOverlay) closeModal();
      toast('已关闭提示，可从更多功能中的「麦克风没声音？」重新打开');
    });
  }
  if (generation !== modalGeneration) return;
  if (!automatic || !activeModal) openUtility(el.micHelp, '麦克风排查');
  else toast('麦克风信号持续静音，可从更多功能中查看排查步骤', true);
}

async function copyDriverCmd() {
  const code = el.vdevHelp.querySelector('#vdhCmd');
  if (!code) return;
  try {
    await navigator.clipboard.writeText(code.textContent);
    toast('命令已复制，粘到终端里执行');
  } catch {
    const r = document.createRange(); r.selectNodeContents(code);
    const sel = getSelection(); sel.removeAllRanges(); sel.addRange(r);
    document.execCommand?.('copy');
    toast('命令已选中，按 ⌘C 复制');
  }
}
el.inSearch.addEventListener('input', render);
el.inSearch.addEventListener('keydown', e => {
  e.stopPropagation();
  if (e.key === 'Escape') { el.inSearch.value = ''; state.activeTag = null; render(); }
});
el.btnSort.addEventListener('click', () => {
  if (state.running) { toast('先点「关闭音效」再排序', true); return; }
  applyLibraryEdit(MixerLibrary.sort(state.items, hotkeyList(),
    (a, b) => a.name.localeCompare(b.name, 'zh')));
  render();
  saveCurrentConfig();
  toast('已按名称排序');
});
el.btnQr.addEventListener('click', toggleQr);

function closeQr() {
  el.qrPanel.classList.add('hidden');
  el.qrPanel.innerHTML = '';
  if (activeModal === el.utilityOverlay && el.utilityTitle.textContent === '手机遥控') closeModal();
}

function closeTestPanel() {
  el.testPanel.classList.add('hidden');
  el.testPanel.innerHTML = '';
  if (activeModal === el.utilityOverlay && el.utilityTitle.textContent === '音频自检') closeModal();
}

async function toggleQr() {
  if (activeModal === el.utilityOverlay && !el.qrPanel.classList.contains('hidden')) {
    closeQr();
    return;
  }
  if (!state.running && !el.remote.dataset.url) {
    // 早期这里会静默失败 —— 用户点了没反应，只能一脸问号
    el.qrPanel.innerHTML = '';
    const tip = document.createElement('div');
    tip.className = 'qrhint';
    tip.innerHTML = '<b>遥控还没启动</b><br>' +
      '点右上角「启动」之后，这里会出现二维码 ——<br>' +
      '手机扫一下就能遥控播放音频。';
    el.qrPanel.appendChild(tip);

    openUtility(el.qrPanel, '手机遥控');
    return;
  }
  el.qrPanel.textContent = '正在读取遥控地址…';
  openUtility(el.qrPanel, '手机遥控');
  const generation = modalGeneration;
  try {
    const q = await invoke('remote_qr');
    if (generation !== modalGeneration) return;
    // 用 canvas 画矩阵：不依赖任何图片/第三方库
    const px = 6, quiet = 4, n = q.width + quiet * 2;
    const cv = document.createElement('canvas');
    cv.width = cv.height = n * px;
    const ctx = cv.getContext('2d');
    ctx.fillStyle = '#fff';
    ctx.fillRect(0, 0, cv.width, cv.height);
    ctx.fillStyle = '#000';
    for (let y = 0; y < q.width; y++) {
      for (let x = 0; x < q.width; x++) {
        if (q.cells[y * q.width + x]) {
          ctx.fillRect((x + quiet) * px, (y + quiet) * px, px, px);
        }
      }
    }
    el.qrPanel.innerHTML = '';
    el.qrPanel.appendChild(cv);
    const info = document.createElement('div');
    info.className = 'qrhint';
    info.innerHTML = '<b>手机扫这个</b><br>和电脑连同一个 WiFi<br>' +
                     `<span class="qrurl">${esc(q.url)}</span>`;
    el.qrPanel.appendChild(info);

  } catch (e) {
    if (generation === modalGeneration) toast(String(e), true);
  }
}
el.btnSetup.addEventListener('click', () => setSetupOpen(!setupOpen));
el.btnSetupClose.addEventListener('click', closeModal);
el.btnSetupSave.addEventListener('click', async () => {
  const generation = modalGeneration;
  state.monitorDevice = el.selMonitor.value || null;
  if (!await saveCurrentConfig()) return;
  syncConnection();
  if (generation !== modalGeneration) return;
  closeModal();
  toast('设置已保存，设备配置在下次启动时生效');
});
el.btnLibrarySetup.addEventListener('click', () => {
  setSetupOpen(true);
  el.librarySettings.focus();
});
el.btnDriverHelp.addEventListener('click', async () => {
  const generation = modalGeneration;
  await renderVdevHelp();
  if (generation === modalGeneration) openUtility(el.vdevHelp, '虚拟音频设备');
});
for (const control of [el.selIn, el.selOut]) control.addEventListener('change', syncConnection);
el.selMonitor.addEventListener('change', () => { state.monitorDevice = el.selMonitor.value || null; });
for (const control of [el.inMonitor, el.inRemote]) control.addEventListener('change', () => saveCurrentConfig());

// 人声效果热键独立保存并立即重绑，不经过音效全量配置，也不重启音频。
async function saveVoiceHotkeys() {
  try {
    await invoke('voice_set_hotkeys', {
      bypass: el.voiceKeyBypass.value.trim(),
      next: el.voiceKeyNext.value.trim(),
      prev: el.voiceKeyPrev.value.trim(),
    });
  } catch (error) {
    toast('人声效果热键保存失败：' + error, true);
  }
}
for (const control of [el.voiceKeyBypass, el.voiceKeyNext, el.voiceKeyPrev]) {
  control.addEventListener('change', saveVoiceHotkeys);
}


async function runSelfTest() {
  el.btnSelfTest.disabled = true;
  el.btnSelfTest.textContent = '自检中…';
  openUtility(el.testPanel, '音频自检');
  const generation = modalGeneration;
  el.testPanel.classList.remove('hidden');
  el.testPanel.innerHTML = '<div class="title">正在送测试音并从环回录回来（约 4 秒）…</div>';
  try {
    const r = await invoke('self_test', { outputDevice: el.selOut.value || null, captureDevice: null });
    if (generation !== modalGeneration) return;
    const cls = r.passed ? 'ok' : 'bad';
    let html = `<div class="title ${cls}">${r.passed ? '通过：' : '未通过：'}${esc(r.summary)}</div>`;
    if (r.attempts > 1 && r.passed) {
      html += `<div class="advice">⚠ 第 ${r.attempts} 次尝试才成功 —— ` +
              `前 ${r.attempts - 1} 次是虚拟声卡开流时的偶发异常，已自动重开流恢复。</div>`;
    } else if (r.attempts > 1) {
      html += `<div class="advice">（已自动重试 ${r.attempts} 次，均未通过）</div>`;
    }
    html += `<div class="row"><span>设备</span><b>${esc(r.device)}</b></div>`;
    for (const c of r.checks) {
      html += `<div class="row"><span>${c.ok ? '通过' : '未通过'} · ${esc(c.name)}</span>` +
              `<b>${esc(c.got)}</b><span>期望 ${esc(c.want)}</span></div>`;
    }
    if (r.advice && r.advice.length) {
      html += `<div class="advice">${r.advice.map(a => '⚠ ' + esc(a)).join('<br>')}</div>`;
    }
    if (r.passed) {
      html += '<div class="advice" style="color:var(--accent2)">' +
              '整条链路正常，可以点「启动」，在目标应用中选择对应虚拟麦克风了。</div>';
    }
    el.testPanel.innerHTML = html;

  } catch (e) {
    if (generation !== modalGeneration) return;
    el.testPanel.classList.remove('hidden');
    el.testPanel.innerHTML = `<div class="title bad"> 自检失败：${esc(e)}</div>`;
  } finally {
    el.btnSelfTest.disabled = false;
    el.btnSelfTest.textContent = '音频自检';
  }
}
el.remote.addEventListener('click', async () => {
  const u = el.remote.dataset.url;
  if (u) { try { await invoke('open_url', { url: u }); } catch (e) { toast(u); } }
});
el.inMicGain.addEventListener('input', pushGains);
el.inSfxGain.addEventListener('input', pushGains);
for (const [slider, number] of [[el.micGainSlider, el.inMicGain], [el.sfxGainSlider, el.inSfxGain]]) {
  slider.addEventListener('input', () => { number.value = slider.value; pushGains(); });
}


// 模态和编辑控件拥有键盘；播放快捷键只作用于主界面的非编辑区域。
document.addEventListener('keydown', event => {
  if (event.isComposing) return;
  if (activeModal) {
    if (event.key === 'Escape') { closeModal(); event.preventDefault(); }
    if (event.key === 'Tab') {
      const controls = modalControls();
      const first = controls[0], last = controls.at(-1);
      if (!controls.includes(document.activeElement) || event.shiftKey && document.activeElement === first) {
        last?.focus(); event.preventDefault();
      } else if (!event.shiftKey && document.activeElement === last) {
        first?.focus(); event.preventDefault();
      }
    }
    return;
  }
  if (event.key === 'Escape') {
    el.toolsMenu.open = false; el.healthDetails.open = false;
    return;
  }
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') {
    el.inSearch.focus(); event.preventDefault(); return;
  }
  if (event.metaKey || event.ctrlKey || event.altKey) return;
  // 卡片点击后保留焦点，空格仍是全局停止，Enter 保留按钮播放语义。
  if (event.key === ' ' && (event.target.classList.contains('play-trigger') || event.target.classList.contains('prev'))) {
    event.preventDefault();
    if (!event.repeat) stopAll();
    return;
  }
  if (event.repeat) return;
  if (event.target.isContentEditable || ['INPUT', 'SELECT', 'TEXTAREA', 'BUTTON', 'SUMMARY', 'A'].includes(event.target.tagName)) return;
  if (event.key >= '1' && event.key <= '9') {
    const index = +event.key - 1;
    if (index < (state.slots.length || state.items.length)) { play(index); event.preventDefault(); }
  } else if (event.key === ' ') { stopAll(); event.preventDefault(); }
});
document.addEventListener('click', event => {
  if (!el.toolsMenu.contains(event.target)) el.toolsMenu.open = false;
  if (!el.healthDetails.contains(event.target)) el.healthDetails.open = false;
});

// ─────────────────────────────── 启动

(async function init() {
 try {
  if (!window.__TAURI__) {
    hint('未检测到 Tauri 运行时 —— 请用 `cargo run -p mixer-gui` 启动', 'error');
    return;
  }
  const saved = await loadSavedConfig();
  await loadDevices();          // 会重建下拉，所以要在恢复配置之后
  if (saved) await loadSavedConfig();
  try {
    const platform = await invoke('platform_info');
    el.searchShortcut.textContent = platform?.os === 'windows' ? 'Ctrl K' : '⌘ K';
    el.routeHelp.textContent = platform?.os === 'windows'
      ? 'VB-CABLE：本软件输出选 CABLE Input；目标应用麦克风选 CABLE Output。'
      : '在目标应用中选择对应的虚拟麦克风；BlackHole 两边选择同一设备。';
  } catch (_) { /* 设备指导仍可从连接设置和关于中查看。 */ }
  await Promise.all([loadVoicePresets(), loadVoiceStatus()]);
  // 即使状态读取失败，也要初始化侧栏状态与试听可用性。
  renderVoice();
  syncGainControls();
  await queueGainUpdate().catch(e => toast('音量设置同步失败：' + e, true));
  setSetupOpen(false);
  syncToggle();
  render();
  // 只有**没有显式音效列表**时才去扫目录。
  // 否则会把用户手工挑的列表（「添加文件…」选的几个）整个替换成目录全量 ——
  // 实测：配置里 1 个文件，开一次 App 就变成整个文件夹的 3 个。
  if (saved && saved.dir && !state.items.length) await scanDir(true);
  setInterval(poll, 250);
  poll();

  // 上报渲染结果 —— 证明 WebView 真的跑起来了
  try {
    const n = document.querySelectorAll('.card').length;
    await invoke('frontend_ready', {
      info: `卡片 ${n} 个 · 视口 ${window.innerWidth}x${window.innerHeight} · ` +
            `设备下拉 ${el.selIn.options.length}/${el.selOut.options.length} 项`,
    });
  } catch (e) { /* 上报失败不影响使用 */ }

  // 调试用自动动作（走与点按钮完全相同的路径）。
  // 放在上报**之后** —— 否则自检结果会被上面的上报覆盖掉。
  try {
    const mode = await invoke('autostart');
    if (mode === 'selftest') {
      await runSelfTest();
      await invoke('frontend_ready', {
        info: '自检：' + el.testPanel.textContent.replace(/\s+/g, ' ').trim().slice(0, 500),
      });
    } else if (mode) {
      toast('MIXER_AUTOSTART：自动启动');
      await start();
    }
  } catch (e) { /* 忽略 */ }
 } catch (e) {
   // 直接把 init 里的异常送出去 —— 实测这个 WebView 里 unhandledrejection 不一定触发
   window.__TAURI__?.core?.invoke?.('js_error', {
     msg: 'init() 抛异常: ' + (e && e.message ? e.message : e),
     stack: (e && e.stack) ? e.stack : '',
   });
 }
})();
