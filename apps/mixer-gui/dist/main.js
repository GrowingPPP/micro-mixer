// 麦克风混音助手前端。纯静态、无打包器 —— 通过 withGlobalTauri 暴露的 __TAURI__ 调用 Rust。

const invoke = (cmd, args) => window.__TAURI__.core.invoke(cmd, args);

const $ = (id) => document.getElementById(id);

// ── 前端错误上报 ─────────────────────────────────────────
// 打包后的 App 没有控制台，前端抛异常就完全静默（只会表现为"窗口空白"）。
// 这里把错误送回后端，`open --stdout <文件>` 就能看到。
function reportErr(msg, stack) {
  // ⚠️ 必须**作为方法**调用（window.__TAURI__.core.invoke(...)），
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
};

// 关于窗口只展示功能和版本；打开、关闭均不改变音频会话。
let aboutOpen = false;
let aboutReturnFocus = null;
async function openAbout() {
  if (aboutOpen) return;
  aboutReturnFocus = document.activeElement || el.btnAbout;
  aboutOpen = true;
  el.aboutOverlay.classList.remove('hidden');
  el.btnAboutClose.focus();
  try {
    const info = await invoke('app_info');
    if (typeof info?.name === 'string' && info.name) el.aboutTitle.textContent = info.name;
    if (typeof info?.version === 'string' && info.version) {
      el.aboutVersion.textContent = `版本 ${info.version} · 支持 macOS 和 Windows`;
      el.aboutVersion.classList.remove('hidden');
    }
  } catch (_) { /* 简介与关闭功能不依赖版本信息。 */ }
}
function closeAbout() {
  if (!aboutOpen) return;
  aboutOpen = false;
  el.aboutOverlay.classList.add('hidden');
  aboutReturnFocus?.focus();
}
el.btnAbout.addEventListener('click', openAbout);
el.btnAboutClose.addEventListener('click', closeAbout);
el.aboutOverlay.addEventListener('click', e => {
  if (e.target === el.aboutOverlay) closeAbout();
});
document.addEventListener('focusin', e => {
  if (aboutOpen && e.target !== el.btnAboutClose) el.btnAboutClose.focus();
});

let state = { running: false, slots: [], items: [], lastStatus: null,
               monitorDevice: null, remoteUrl: null, activeTag: null, devices: [],
               lastDeviceLost: null, lastPanicSeen: null, lastStalled: null,
               lastMonitorError: null,
               silentSecs: 0, micHelpShown: false, micHelpDismissed: false };

// ── 音效库条目：{ id, file, name, gain, tags: string[] }
//    标签始终保存为数组，单个标签中的标点不作为分隔符。
const basename = MixerLibrary.basename;
const makeItem = (file, name, gain, tags, id) => ({
  id: id || (globalThis.crypto?.randomUUID?.() ?? `sound-${Date.now()}-${Math.random().toString(16).slice(2)}`),
  file,
  name: (name || '').trim() || basename(file),
  gain: Number.isFinite(+gain) ? +gain : 0,
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
}

// ─────────────────────────────── 设备

async function loadSavedConfig() {
  try {
    const c = await invoke('load_config');
    if (!c) return null;
    if (c.dir) el.inDir.value = c.dir;
    if (Array.isArray(c.items) && c.items.length) {
      // ⚠️ 不能写成 .map(makeItem) —— map 会传 (元素, 索引, 数组)，
      //    而 makeItem 的签名是 (file, name, gain, tags)，整个对象会被当成路径。
      state.items = c.items.map(o => makeItem(o.file, o.name, o.gain, o.tags, o.id));
    }
    if (c.buffer_ms) el.inBuf.value = c.buffer_ms;
    if (c.mic_gain_db !== null) el.inMicGain.value = c.mic_gain_db;
    if (c.sfx_gain_db !== null) el.inSfxGain.value = c.sfx_gain_db;
    if (typeof c.hotkeys === 'string') el.inKeys.value = c.hotkeys;
    if (typeof c.monitor === 'boolean') el.inMonitor.checked = c.monitor;
    if (typeof c.looping === 'boolean') el.inLoop.checked = c.looping;
    syncLoopHelp();
    state.monitorDevice = c.monitor_device || null;

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

    if (c.dir) hint('已载入上次的设置', 'ok');
    return c;
  } catch (e) { return null; }
}

let configSaveTail = Promise.resolve();
async function saveCurrentConfig(before) {
  try {
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
      micGainDb: parseFloat(el.inMicGain.value || '0'),
      sfxGainDb: parseFloat(el.inSfxGain.value || '-6'),
      hotkeys: el.inKeys.value.trim(),
    };
    // Preserve completion order when consecutive slider gestures save snapshots.
    configSaveTail = configSaveTail.catch(() => {}).then(async () => {
      if (before) await before.catch(() => {});
      return invoke('save_config', snapshot);
    });
    await configSaveTail;
  } catch (e) { /* 保存失败不影响使用 */ }
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

    if (previousInput) el.selIn.value = MixerLibrary.resolveSelector(devs, previousInput, '输入');
    if (previousOutput) el.selOut.value = MixerLibrary.resolveSelector(devs, previousOutput, '输出');
    if (state.monitorDevice) state.monitorDevice = MixerLibrary.resolveSelector(devs, state.monitorDevice, '输出');
    const guess = previousOutput ? null : await invoke('guess_output');
    if (guess) {
      for (const o of el.selOut.options) {
        if (o.value === guess) { el.selOut.value = guess; break; }
      }
    }
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
  if (state.running) { if (!silent) toast('先点「停止」再改音效库', true); return; }
  const dir = el.inDir.value.trim();
  if (!dir) { if (!silent) hint('先填一个音效文件夹', 'error'); return; }
  try {
    const files = (await invoke('scan_dir', { path: dir })) || [];
    if (state.running) { if (!silent) toast('先点「停止」再改音效库', true); return; }
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
      hint('还没有音效。选一个音效文件夹，点「载入」。', 'error');
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
    if (choice !== audioChoice) return;
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
        mic_gain_db: parseFloat(el.inMicGain.value || '0'),
        sfx_gain_db: parseFloat(el.inSfxGain.value || '-6'),
        looping: el.inLoop.checked,
        monitor: el.inMonitor.checked,
        // ⚠️ StartConfig 走 serde 反序列化，字段名必须**蛇形**。
        //    写成驼峰会被静默忽略，然后被 null 覆盖 —— 症状是
        //    「用 config set 指定了监听设备，一点启动就被抹掉」。
        monitor_device: state.monitorDevice || null,
        remote_port: el.inRemote.checked ? 8765 : null,
      };
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
    hint((opened ? '循环设置更新失败，请重新切换循环开关：' : '启动失败：') + e, 'error');
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
  setSetupOpen(true);
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

let setupOpen = true;
function setSetupOpen(open) {
  setupOpen = open;
  el.setup.classList.toggle('hidden', !open);
  el.btnSetup.textContent = open ? '设置' : '设置 ▾';
  el.btnSetup.classList.toggle('primary', !open && state.running);
}

let gainTimer = null;
function pushGains() {
  if (!state.running) return;
  clearTimeout(gainTimer);
  gainTimer = setTimeout(() => {
    invoke('set_gains', {
      micDb: parseFloat(el.inMicGain.value || '0'),
      sfxDb: parseFloat(el.inSfxGain.value || '-6'),
    }).then(() => toast('增益已更新')).catch(() => {});
  }, 120);
}

function syncToggle() {
  // 待机时按钮说「开启音效」而不是「启动」—— 因为引擎其实一直在跑，
  // 变的只是音效开没开。
  // ⚠️ 别叫「停止」—— 底部还有一个「停止播放」，两者完全不是一回事：
  //    这个是**开关音效功能**（关闭后麦克风照样转发），那个是**掐掉正在响的音效**。
  el.btnToggle.textContent = state.running ? '关闭音效'
    : state.standby ? '开启音效' : '启动';
  el.btnToggle.title = state.running
    ? '关掉音效播放。麦克风会继续转发，对方照样听得到你说话'
    : state.standby
      ? '重新打开音效（设备不重开，瞬间生效）'
      : '启动音频引擎（麦克风开始转发，热键生效）';
  el.btnToggle.classList.toggle('running', state.running);
}

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
}

async function play(slot) {
  if (!MixerLibrary.canPlay(state)) { toast("先开启音效再播放", true); return; }
  const choice = newAudioChoice();
  try {
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
    // 和「试听」形成明确对照：这条是真的送进麦克风了
    toast(replacing ? '已切换播放 —— 对方听得到' : '已送进麦克风 —— 对方听得到');
  } catch (e) {
    if (choice === audioChoice) toast(e, true);
  }
}

async function stopAll() {
  const choice = newAudioChoice();
  try {
    if (!await confirmedAudio(await invokeAudio('stop_all', {}, choice), choice)) return;
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
  for (const it of state.items) for (const t of itemTags(it)) {
    counts.set(t, (counts.get(t) || 0) + 1);
  }
  el.tagbar.innerHTML = '';
  if (!counts.size) return;
  for (const [t, n] of [...counts.entries()].sort()) {
    const chip = document.createElement('span');
    chip.className = 'tag' + (state.activeTag === t ? ' on' : '');
    chip.innerHTML = `${esc(t)}<span class="n">${n}</span>`;
    chip.title = state.activeTag === t ? '点一下取消筛选' : `只看带「${t}」的音效`;
    chip.addEventListener('click', () => {
      state.activeTag = state.activeTag === t ? null : t;
      render();
    });
    el.tagbar.appendChild(chip);
  }
}

function render() {
  renderTags();
  const slots = visibleSlots();

  el.grid.innerHTML = '';
  if (!baseSlots().length) {
    const d = document.createElement('div');
    d.className = 'empty';
    d.innerHTML = state.running
      ? '没有音效'
      : '选一个音效文件夹，点「载入」<br><b>然后把目标应用的麦克风选为对应的虚拟麦克风</b>';
    el.grid.appendChild(d);
    updateCount();
    return;
  }
  if (!slots.length) {
    const d = document.createElement('div');
    d.className = 'empty';
    d.innerHTML = '没有匹配的音效<br><b>换个搜索词，或点一下标签取消筛选</b>' +
                   `<br><br><span class="prev" id="clearFilter" style="cursor:pointer">清除筛选</span>`;
    el.grid.appendChild(d);
    const cf = d.querySelector('#clearFilter');
    if (cf) cf.addEventListener('click', () => {
      el.inSearch.value = ''; state.activeTag = null; render();
    });
    updateCount();
    return;
  }

  for (const s of slots) {
    const card = document.createElement('div');
    card.className = 'card';
    card.dataset.slot = s.index;   // ⚠️ 始终是**原始**槽位号
    const it = MixerLibrary.itemForSlot(state, s);
    const g = it?.gain ?? s.gain_db ?? 0;
    const tags = itemTags(it);
    card.innerHTML = `
      <div class="tools">
        <button data-act="left"  title="前移（会改变热键对应关系）">←</button>
        <button data-act="right" title="后移（会改变热键对应关系）">→</button>
        <button data-act="del" class="del" title="从音效库移除（不会删除文件）">×</button>
      </div>
      <span class="idx">${s.index + 1}</span>
      <div class="name" title="双击可改名">${esc(s.name)}</div>
      <div class="meta">
        <span class="prev" title="试听：只放给你自己听，对方听不到。想让对方听到请点卡片空白处，或按热键">试听</span>
        <span class="dur">${s.duration ? s.duration.toFixed(1) + 's' : ''}</span>
        ${s.hotkey ? `<span class="key">${esc(s.hotkey)}</span>` : ''}
      </div>
      <div class="playback-state" aria-live="polite"></div>
      <div class="tags" title="双击可编辑标签">${
        tags.length ? tags.map(t => `<i>${esc(t)}</i>`).join('')
                    : '<i class="addtag">＋ 标签</i>'
      }</div>
      <div class="volume-control">
        <div class="volume-heading">
          <label class="volume-label" for="sound-volume-${s.index}">音效音量</label>
          <output class="volume-value" for="sound-volume-${s.index}">${formatGain(g)}</output>
        </div>
        <input id="sound-volume-${s.index}" class="vol ${volClass(g)}" type="range"
               min="${VOL_MIN}" max="${VOL_MAX}" step="1" value="${g}"
               aria-label="${esc(s.name)} 音效音量" aria-valuetext="${formatGain(g)}"
               title="音效音量 ${formatGain(g)} · 试听与播放均实时生效">
        <div class="volume-help">试听、播放均实时生效</div>
      </div>
      <div class="bar" title="播放进度" aria-hidden="true"></div>`;

    card.addEventListener('click', () => play(s.index));

    // 工具栏：前移 / 后移 / 移除
    for (const btn of card.querySelectorAll('.tools button')) {
      btn.addEventListener('click', e => {
        e.stopPropagation();
        const act = btn.dataset.act;
        if (act === 'del') removeItem(MixerLibrary.sourceIndex(s));
        else moveItem(MixerLibrary.sourceIndex(s), act === 'left' ? -1 : 1);
      });
      btn.addEventListener('pointerdown', e => e.stopPropagation());
    }

    // 双击改名
    const nameEl = card.querySelector('.name');
    nameEl.addEventListener('dblclick', e => {
      e.stopPropagation();
      startRename(nameEl, MixerLibrary.sourceIndex(s));
    });

    // 双击标签区改标签
    const tagsEl = card.querySelector('.tags');
    tagsEl.addEventListener('dblclick', e => {
      e.stopPropagation();
      startTagEdit(tagsEl, MixerLibrary.sourceIndex(s));
    });
    tagsEl.addEventListener('click', e => e.stopPropagation());

    // 试听：只给自己听
    const pv = card.querySelector('.prev');
    pv.addEventListener('click', async e => {
      e.stopPropagation();
      const file = s.file || it?.file;
      if (!file) { toast('找不到这个音效的文件路径', true); return; }
      const choice = newAudioChoice();
      try {
        const replacing = !!MixerLibrary.currentPlayback(state);
        toast('加载试听…');
        if (!await confirmedAudio(await invokeAudio('preview', {
          file, gainDb: it?.gain ?? 0, soundId: it?.id ?? s.id,
        }, choice), choice)) return;
        await poll(choice);
        if (choice !== audioChoice) return;
        toast(replacing ? '已切换试听（仅自己听，对方听不到）' : '试听中（对方听不到）');
      } catch (err) { if (choice === audioChoice) toast('试听失败：' + err, true); }
    });
    pv.addEventListener('pointerdown', e => e.stopPropagation());

    const vol = card.querySelector('.vol');
    const volume = card.querySelector('.volume-control');
    volume.addEventListener('click', e => e.stopPropagation());
    volume.addEventListener('pointerdown', e => e.stopPropagation());
    vol.addEventListener('click', e => e.stopPropagation());
    vol.addEventListener('pointerdown', e => e.stopPropagation());
    vol.addEventListener('input', e => {
      e.stopPropagation();
      const db = +e.target.value;
      setSoundGain(it?.id ?? s.id, db);
      vol.className = 'vol ' + volClass(db);
      vol.title = `音效音量 ${formatGain(db)} · 试听与播放均实时生效`;
      vol.setAttribute('aria-valuetext', formatGain(db));
      card.querySelector('.volume-value').textContent = formatGain(db);
    });
    vol.addEventListener('change', e => { e.stopPropagation(); return saveCurrentConfig(); });
    updateCardPlayback(card, s);
    el.grid.appendChild(card);
  }
  updateCount();
}

// ─────────────────────────────── 音效库操作

function applyLibraryEdit(edit) {
  state.items = edit.items;
  el.inKeys.value = edit.keys.join(',');
}

function removeItem(idx) {
  const it = state.items[idx];
  if (!it) return;
  if (state.running) { toast('先点「停止」再改音效库', true); return; }
  applyLibraryEdit(MixerLibrary.remove(state.items, hotkeyList(), idx));
  render();
  toast(`已移除「${it.name}」（文件没删）`);
}

function moveItem(idx, delta) {
  const to = idx + delta;
  if (state.running) { toast('先点「停止」再改音效库', true); return; }
  if (to < 0 || to >= state.items.length) return;
  applyLibraryEdit(MixerLibrary.move(state.items, hotkeyList(), idx, to));
  render();
}

/// 双击标签区进入编辑。
///
/// 分隔符同时认中英文逗号 / 顿号 / 分号 / 竖线 —— 用户打中文时很自然会用「，」，
/// 只认英文逗号会让人以为"输了没反应"。
function startTagEdit(tagsEl, idx) {
  if (state.running) { toast('先点「停止」再改标签', true); return; }
  const it = state.items[idx];
  if (!it) return;
  const input = document.createElement('input');
  input.type = 'text';
  input.className = 'taginput';
  const initialTags = itemTags(it);
  const initialText = initialTags.join(',');
  input.value = initialText;
  const others = allTags().filter(t => !initialTags.includes(t));
  input.placeholder = others.length ? `可加：${others.slice(0, 3).join('、')}` : '标签1,标签2';
  tagsEl.innerHTML = '';
  tagsEl.appendChild(input);
  input.focus();
  input.select();

  let done = false;
  const finish = (save) => {
    if (done) return;
    done = true;
    // 打开后直接离开编辑框，必须保留含字面逗号等标点的原始标签。
    if (save) it.tags = MixerLibrary.tagsAfterEdit(initialTags, initialText, input.value);
    render();
  };
  input.addEventListener('blur', () => finish(true), { once: true });
  input.addEventListener('keydown', e => {
    e.stopPropagation();               // 别让数字键触发播放
    if (e.key === 'Enter') { e.preventDefault(); input.blur(); }
    if (e.key === 'Escape') { e.preventDefault(); finish(false); }
  });
  input.addEventListener('click', e => e.stopPropagation());
  input.addEventListener('pointerdown', e => e.stopPropagation());
}

function startRename(nameEl, idx) {
  if (state.running) { toast('先点「停止」再改名', true); return; }
  const it = state.items[idx];
  if (!it) return;
  nameEl.contentEditable = 'true';
  nameEl.textContent = it.name;
  nameEl.focus();
  document.execCommand?.('selectAll', false, null);
  const finish = (save) => {
    nameEl.contentEditable = 'false';
    if (save) {
      const v = nameEl.textContent.trim();
      if (v) { it.name = v; toast(`改名为「${v}」`); }
    }
    render();
  };
  nameEl.addEventListener('blur', () => finish(true), { once: true });
  nameEl.addEventListener('keydown', ev => {
    ev.stopPropagation();
    if (ev.key === 'Enter') { ev.preventDefault(); nameEl.blur(); }
    if (ev.key === 'Escape') { ev.preventDefault(); finish(false); }
  });
}

function volClass(db) {
  if (db <= VOL_MIN) return 'quiet';
  if (db > 0) return 'boosted';
  return '';
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
  el.count.textContent = n ? `（共 ${n} 个）` : '';
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
  const label = card.querySelector('.playback-state');
  if (label) label.textContent = playback.active
    ? preview ? '试听中 · 仅自己听' : '播放中 · 对方听得到' : '';
  const bar = card.querySelector('.bar');
  if (bar) {
    bar.style.width = playback.progress + '%';
    bar.title = preview ? '试听进度' : '播放进度';
  }
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
  try {
    const st = await invoke('status');
    // 命令会立即刷新状态；较早的轮询迟到时不能把当前音效切回旧卡片。
    if (!st || choice !== audioChoice || request < appliedStatus) return;
    appliedStatus = request;
    const wasRunning = state.running;
    state.running = st.running;
    state.lastStatus = st;
    state.slots = st.slots || [];

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
          ? '待机 · 麦克风仍在转发（对方听得到你，音效已关）'
          : '未启动')
      : `${st.device} · 已运行 ${fmtTime(st.uptime_secs)}` +
        (st.hotkeys_active ? ' · 热键已绑定' : ' · 热键未绑定');

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
        showMicHelp();
      }
    } else if (!st.running) {
      state.silentSecs = 0;
    }

    // 程序内部 panic：必须让用户知道 —— 打包后没有控制台，不说就等于没发生
    if (st.last_panic && st.last_panic !== state.lastPanicSeen) {
      state.lastPanicSeen = st.last_panic;
      hint('💥 程序内部出错了：' + st.last_panic +
           ' —— 请查看诊断日志；音频正在尝试自动恢复。', 'error');
      toast('程序内部出错（已兜住，正在自动恢复）', true);
    }

    // 音频卡死 / 已自动重启
    if (st.audio_stalled && st.audio_stalled !== state.lastStalled) {
      state.lastStalled = st.audio_stalled;
      hint('🎧 ' + st.audio_stalled, 'error');
      toast('音频卡了一下，已自动处理', true);
    } else if (!st.audio_stalled && state.lastStalled) {
      state.lastStalled = null;
    } else if (!st.last_panic) {
      state.lastPanicSeen = null;
    }

    // 设备掉线：这是**正在游戏里也必须要看到**的报警
    if (st.device_lost && st.device_lost !== state.lastDeviceLost) {
      state.lastDeviceLost = st.device_lost;
      hint('🔌 ' + st.device_lost, 'error');
      toast('设备掉线！' + st.device_lost, true);
    } else if (!st.device_lost && state.lastDeviceLost) {
      state.lastDeviceLost = null;
    }

    // 监听有独立的健康状态；同一错误持续存在时只提醒一次。
    if (st.monitor_error && st.monitor_error !== state.lastMonitorError) {
      state.lastMonitorError = st.monitor_error;
      hint('🎧 本机监听输出异常：' + st.monitor_error +
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
      : '关闭窗口不会退出，引擎继续运行 · 菜单栏图标可重新打开';
      if (st.last_error) hint('启动失败：' + st.last_error, 'error');
    }

    // 电平表 —— 直接回答"对方能不能听到我"
    updateMeter(st);

    // 手机遥控地址：优先显示局域网地址（手机要用的就是它）
    const urls = (st.remote_urls || []).filter(u => !u.includes('127.0.0.1'));
    const best = urls[0] || (st.remote_urls || [])[0];
    el.remote.textContent = best ? '📱 ' + best.replace('http://', '') : '';
    el.remote.title = best ? `手机浏览器打开 ${best}\n（需与电脑在同一局域网）` : '';
    el.remote.dataset.url = best || '';
  

    if (wasRunning !== state.running) { syncToggle(); render(); }

    // 主播放和试听共用唯一当前音效；试听也可在未启动/待机时运行。
    const slots = new Map(baseSlots().map(slot => [slot.index, slot]));
    for (const card of el.grid.children) {
      const slot = slots.get(+card.dataset.slot);
      if (slot) updateCardPlayback(card, slot);
    }
  } catch (e) { /* 窗口关闭时忽略 */ }
}

// dBFS → 0~100% 宽度（-60dB 起算）
const METER_FLOOR = -60;
const pct = db => Math.max(0, Math.min(100, ((db - METER_FLOOR) / -METER_FLOOR) * 100));

function updateMeter(st) {
  if (!state.running) { el.meter.classList.add('off'); return; }
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
  if (state.running) { toast('先点「停止」再改音效库', true); return; }
  const picked = (await invoke('add_files')) || [];
  if (!picked.length) return;
  if (state.running) { toast('先点「停止」再改音效库', true); return; }
  const previousCount = state.items.length;
  applyLibraryEdit(MixerLibrary.appendFiles(state.items, hotkeyList(), picked, makeItem));
  const added = state.items.length - previousCount;
  render();
  hint(added ? `添加了 ${added} 个音效` : '这些文件已经在列表里了', added ? 'ok' : '');
});
el.btnClear.addEventListener('click', () => {
  if (state.running) { toast('先点「停止」再改音效库', true); return; }
  state.items = []; el.inKeys.value = '';
  render(); hint('已清空音效列表');
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
    await invoke('set_gains', { micDb: gain, sfxDb: parseFloat(el.inSfxGain.value || '-6') });

    const band = measured + gain;
    setHint2(
      `你的麦克风峰值 ${measured.toFixed(1)} dBFS → 补 ${gain > 0 ? '+' : ''}${gain} dB ` +
      `→ 预计 ${band.toFixed(1)} dBFS` +
      (limiterHit ? '（刚才有削波，建议说话离麦克风远一点）' : '') +
      '。满意就点「启动」旁边的设置保存，不满意再点一次重测。',
      'ok'
    );
    toast(`校准完成：麦克风增益 ${gain > 0 ? '+' : ''}${gain} dB`);
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
  hint('设备已释放。⚠️ 如果目标应用的麦克风仍选着虚拟声卡，对方现在听不到你 —— 要么重新「启动」，' +
       '要么把目标应用的麦克风改回真实麦克风。', 'error');
});

/// 这台机器上有没有虚拟声卡？
///
/// 没有的话整个软件都用不了（游戏只能从"麦克风"读声音，而虚拟声卡就是那个麦克风），
/// 所以这种情况必须**主动告诉用户怎么装**，而不是让他对着空的下拉框发愣。
async function checkVirtualDevice() {
  const has = state.devices.some(d => d.is_virtual && d.direction.includes('输出'));
  el.vdevHelp.classList.toggle('hidden', has);
  if (!has) await renderVdevHelp();
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

  let html = `<div class="vdh-title">⚠️ ${esc2(info.title)}</div>` +
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
    if (await checkVirtualDevice()) toast('检测到虚拟声卡了 ✅');
    else toast('还是没有 —— 装完驱动记得重启一次', true);
  });
}

/// 麦克风收不到声音时，按平台给出排查步骤。
///
/// 内容是后端给的（`platform_info.mic_privacy`）—— macOS 和 Windows 的设置路径
/// 完全不一样，不该在前端里堆 if。
/// 把 `**粗体**` 转成 <b>。引导文案和步骤都要用，所以抽出来。
function mdBold(t) {
  return String(t).replace(/\*\*(.+?)\*\*/g, '<b>$1</b>');
}

async function showMicHelp() {
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
      `<div class="vdh-title">🔇 ${esc2(mp.title)}</div>` +
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
      toast('好，不再提示。真没声音的话点上面的「麦克风」按钮可以再打开');
    });
  }
  el.micHelp.classList.remove('hidden');
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
document.addEventListener('keydown', e => {
  if (aboutOpen) return;
  if (e.key !== 'Escape') return;
  // 正在改名字/标签时 Esc 是"取消编辑"，别抢
  if (document.activeElement && document.activeElement.classList.contains('taginput')) return;
  if (document.activeElement && document.activeElement.isContentEditable) return;
  let closed = false;
  if (!el.qrPanel.classList.contains('hidden')) { closeQr(); closed = true; }
  if (!el.testPanel.classList.contains('hidden')) { closeTestPanel(); closed = true; }
  if (closed) e.preventDefault();
});
el.inSearch.addEventListener('keydown', e => {
  e.stopPropagation();
  if (e.key === 'Escape') { el.inSearch.value = ''; state.activeTag = null; render(); }
});
el.btnSort.addEventListener('click', () => {
  if (state.running) { toast('先点「停止」再排序', true); return; }
  applyLibraryEdit(MixerLibrary.sort(state.items, hotkeyList(),
    (a, b) => a.name.localeCompare(b.name, 'zh')));
  render();
  toast('已按名称排序');
});
el.btnQr.addEventListener('click', toggleQr);

/// 给面板挂一个右上角的「×」。
///
/// 自检结果和二维码块头都不小，不关掉的话音效卡片会被挤到最底下去 ——
/// 用户实测反馈过这一点。除了点 ×，按 Esc 也能关（见下面的全局键盘处理）。
function addPanelClose(panel, onClose) {
  const b = document.createElement('button');
  b.className = 'panelclose';
  b.textContent = '×';
  b.title = '关闭（Esc）';
  b.addEventListener('click', e => {
    e.stopPropagation();
    onClose();
  });
  panel.appendChild(b);
}

function closeQr() {
  el.qrPanel.classList.add('hidden');
  el.qrPanel.innerHTML = '';
}

function closeTestPanel() {
  el.testPanel.classList.add('hidden');
  el.testPanel.innerHTML = '';
}

async function toggleQr() {
  if (!el.qrPanel.classList.contains('hidden')) {
    closeQr();
    return;
  }
  if (!state.running && !state.remoteUrl) {
    // 早期这里会静默失败 —— 用户点了没反应，只能一脸问号
    el.qrPanel.innerHTML = '';
    const tip = document.createElement('div');
    tip.className = 'qrhint';
    tip.innerHTML = '<b>遥控还没启动</b><br>' +
      '点右上角「启动」之后，这里会出现二维码 ——<br>' +
      '手机扫一下就能遥控播放音频。';
    el.qrPanel.appendChild(tip);
    addPanelClose(el.qrPanel, closeQr);
    el.qrPanel.classList.remove('hidden');
    return;
  }
  try {
    const q = await invoke('remote_qr');
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
    addPanelClose(el.qrPanel, closeQr);
    el.qrPanel.classList.remove('hidden');
  } catch (e) {
    toast(String(e), true);
  }
}
el.btnSetup.addEventListener('click', () => setSetupOpen(!setupOpen));

async function runSelfTest() {
  el.btnSelfTest.disabled = true;
  el.btnSelfTest.textContent = '自检中…';
  el.testPanel.className = 'testpanel';
  el.testPanel.innerHTML = '<div class="title">正在送测试音并从环回录回来（约 4 秒）…</div>';
  try {
    const r = await invoke('self_test', { outputDevice: el.selOut.value || null, captureDevice: null });
    const cls = r.passed ? 'ok' : 'bad';
    let html = `<div class="title ${cls}">${r.passed ? '✅' : '❌'} ${esc(r.summary)}</div>`;
    if (r.attempts > 1 && r.passed) {
      html += `<div class="advice">⚠ 第 ${r.attempts} 次尝试才成功 —— ` +
              `前 ${r.attempts - 1} 次是虚拟声卡开流时的偶发异常，已自动重开流恢复。</div>`;
    } else if (r.attempts > 1) {
      html += `<div class="advice">（已自动重试 ${r.attempts} 次，均未通过）</div>`;
    }
    html += `<div class="row"><span>设备</span><b>${esc(r.device)}</b></div>`;
    for (const c of r.checks) {
      html += `<div class="row"><span>${c.ok ? '✅' : '❌'} ${esc(c.name)}</span>` +
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
    addPanelClose(el.testPanel, closeTestPanel);
  } catch (e) {
    el.testPanel.className = 'testpanel';
    el.testPanel.innerHTML = `<div class="title bad">❌ 自检失败：${esc(e)}</div>`;
  } finally {
    el.btnSelfTest.disabled = false;
    el.btnSelfTest.textContent = '自检';
  }
}
el.remote.addEventListener('click', async () => {
  const u = el.remote.dataset.url;
  if (u) { try { await invoke('open_url', { url: u }); } catch (e) { toast(u); } }
});
el.inMicGain.addEventListener('input', pushGains);
el.inSfxGain.addEventListener('input', pushGains);

// 快捷键：数字播放，空格停止（窗口内有效；游戏内靠全局热键）
document.addEventListener('keydown', e => {
  if (aboutOpen) {
    if (e.key === 'Escape') { closeAbout(); e.preventDefault(); }
    if (e.key === 'Tab') { el.btnAboutClose.focus(); e.preventDefault(); }
    return;
  }
  if (e.target.tagName === 'INPUT' || e.target.tagName === 'SELECT') return;
  if (e.key >= '1' && e.key <= '9') {
    const i = +e.key - 1;
    if (i < (state.slots.length || state.items.length)) { play(i); e.preventDefault(); }
  } else if (e.key === ' ') {
    stopAll(); e.preventDefault();
  }
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
  setSetupOpen(true);
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
