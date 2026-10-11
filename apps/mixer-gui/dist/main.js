// 音效板前端。纯静态、无打包器 —— 通过 withGlobalTauri 暴露的 __TAURI__ 调用 Rust。

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
  inLoop: $('inLoop'), hint: $('setupHint'),
  btnAdd: $('btnAdd'), btnClear: $('btnClear'), count: $('count'),
  inRemote: $('inRemote'), remote: $('remote'), meter: $('meter'),
  inMonitor: $('inMonitor'),
  btnSelfTest: $('btnSelfTest'), testPanel: $('testPanel'),
  btnQr: $('btnQr'), qrPanel: $('qrPanel'),
  inSearch: $('inSearch'), tagbar: $('tagbar'), btnSort: $('btnSort'),
  btnRelease: $('btnRelease'), btnCalib: $('btnCalib'), calibHint: $('calibHint'),
  btnRevertMic: $('btnRevertMic'),
  vdevHelp: $('vdevHelp'), btnRecheck: $('btnRecheck'), btnCopyCmd: $('btnCopyCmd'), vdhCmd: $('vdhCmd'),
  grid: $('grid'), metrics: $('metrics'), toast: $('toast'),
  btnStopAll: $('btnStopAll'), setup: $('setup'), btnSetup: $('btnSetup'),
};

let state = { running: false, slots: [], items: [], lastStatus: null,
               monitorDevice: null, remoteUrl: null, activeTag: null, devices: [],
               lastDeviceLost: null, lastPanicSeen: null, lastStalled: null };

// ── 音效库条目：{ file, name, gain, tags }
//    标签用逗号分隔的字符串存（与配置一致），用时再拆成数组。
const basename = (p) => String(p).split('/').pop().replace(/\.[^.]+$/, '');
const makeItem = (file, name, gain, tags) => ({
  file,
  name: (name || '').trim() || basename(file),
  gain: Number.isFinite(+gain) ? +gain : 0,
  tags: tags || '',
});
const itemTags = (it) => String(it?.tags || '')
  .split(/[,|;]/).map(t => t.trim()).filter(Boolean);
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
      state.items = c.items.map(o => makeItem(o.file, o.name, o.gain, o.tags));
    }
    if (c.buffer_ms) el.inBuf.value = c.buffer_ms;
    if (c.mic_gain_db !== null) el.inMicGain.value = c.mic_gain_db;
    if (c.sfx_gain_db !== null) el.inSfxGain.value = c.sfx_gain_db;
    if (c.hotkeys) el.inKeys.value = c.hotkeys;
    if (typeof c.monitor === 'boolean') el.inMonitor.checked = c.monitor;
    state.monitorDevice = c.monitor_device || null;

    // 设备下拉里没有这一项时，临时插进去，避免设置丢失
    const pick = (sel, name) => {
      if (!name) return;
      if (![...sel.options].some(o => o.value === name)) {
        const o = document.createElement('option');
        o.value = name; o.textContent = name + '（上次使用）';
        sel.appendChild(o);
      }
      sel.value = name;
    };
    pick(el.selIn, c.input_device);
    pick(el.selOut, c.output_device);

    if (c.dir) hint('已载入上次的设置', 'ok');
    return c;
  } catch (e) { return null; }
}

async function saveCurrentConfig() {
  try {
    await invoke('save_config', {
      items: state.items.map(it => ({
        file: it.file, name: it.name, gain: it.gain, tags: it.tags,
      })),
      monitor: el.inMonitor.checked,
      monitorDevice: state.monitorDevice || null,
      dir: el.inDir.value.trim(),
      inputDevice: el.selIn.value || null,
      outputDevice: el.selOut.value || null,
      bufferMs: parseInt(el.inBuf.value || '20', 10),
      micGainDb: parseFloat(el.inMicGain.value || '0'),
      sfxGainDb: parseFloat(el.inSfxGain.value || '-6'),
      hotkeys: el.inKeys.value.trim(),
    });
  } catch (e) { /* 保存失败不影响使用 */ }
}

async function loadDevices() {
  try {
    const devs = (await invoke('list_devices')) || [];
    state.devices = devs;
    const inputs = devs.filter(d => d.direction === '输入');
    const outputs = devs.filter(d => d.direction === '输出');

    fill(el.selIn, inputs, '（系统默认麦克风）');
    fill(el.selOut, outputs, '（自动探测虚拟声卡）');

    const guess = await invoke('guess_output');
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
    o.value = d.name;
    o.textContent = d.name +
      (d.is_default ? ' · 默认' : '') +
      `  ${d.sample_rate / 1000}kHz/${d.channels}ch`;
    sel.appendChild(o);
  }
}

// ─────────────────────────────── 音效

async function scanDir(silent = false) {
  const dir = el.inDir.value.trim();
  if (!dir) { if (!silent) hint('先填一个音效文件夹', 'error'); return; }
  try {
    const files = (await invoke('scan_dir', { path: dir })) || [];
    // 已有的条目按路径复用（保住用户改过的名字、标签、音量）
    const known = new Map(state.items.map(it => [it.file, it]));
    state.items = files.map(f => known.get(f) || makeItem(f));
    if (!files.length) {
      hint('这个文件夹里没有音频文件（支持 mp3 / wav / ogg / m4a / flac 等）', 'error');
    } else {
      hint(`找到 ${files.length} 个音效`, 'ok');
      if (!el.inKeys.value.trim()) {
        el.inKeys.value = files.map((_, i) => 'F' + (i + 1)).join(',');
      }
    }
    render();
  } catch (e) {
    hint('读取文件夹失败：' + (e && e.message ? e.message : e) +
         ' —— 检查路径是否存在、是否有权限', 'error');
    state.items = []; render();
  }
}

function hotkeyList() {
  return el.inKeys.value.split(',').map(s => s.trim()).filter(Boolean);
}

// ─────────────────────────────── 启停

async function start() {
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
  try {
    await invoke('start', {
      cfg: {
        input_device: el.selIn.value || null,
        output_device: el.selOut.value || null,
        files: state.items.map(it => it.file),
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
      },
    });
    state.running = true;
    setSetupOpen(false);   // 启动后收起设置，把空间让给音效网格
    closeTestPanel();   // 自检是"启动前"的检查，开起来之后留着只占地方
    toast('已启动 · 关闭窗口不会退出');
  hint(`正在把「${el.selIn.value || '系统默认麦克风'}」的声音和音效一起送进 ` +
       `「${el.selOut.value || '虚拟声卡'}」。游戏麦克风选它就能同时听到你说话和音效。`, 'ok');
  } catch (e) {
    hint('启动失败：' + e, 'error');
    state.running = false;
  } finally {
    el.btnToggle.disabled = false;
    syncToggle();
  }
}

async function stop() {
  el.btnToggle.disabled = true;
  try { await invoke('stop'); } catch (e) { /* 忽略 */ }
  state.running = false;
  state.slots = [];
  setSetupOpen(true);
  el.btnToggle.disabled = false;
  syncToggle();
  render();
  toast('音效已关闭');
  hint('已停止音效。麦克风仍在转发 —— 队友照常听得到你说话，' +
       '想放音效时点「开启音效」即可，游戏里的麦克风不用动。', 'ok');
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
    ? '关掉音效播放。麦克风会继续转发，队友照样听得到你说话'
    : state.standby
      ? '重新打开音效（设备不重开，瞬间生效）'
      : '启动音频引擎（麦克风开始转发，热键生效）';
  el.btnToggle.classList.toggle('running', state.running);
}

// ─────────────────────────────── 播放

async function play(slot) {
  try {
    const ok = await invoke('play', { slot });
    if (!ok) { toast('没有这个音效', true); return; }
    // 和「试听」形成明确对照：这条是真的送进麦克风了
    toast('已送进麦克风 —— 队友听得到');
  } catch (e) {
    toast(e, true);
  }
}

async function stopAll() {
  try { await invoke('stop_all'); toast('已掐掉正在播放的音效（麦克风转发不受影响）'); }
  catch (e) { toast(e, true); }
}

// ─────────────────────────────── 渲染

function baseSlots() {
  return state.slots.length ? state.slots
    : state.items.map((it, i) => ({
        index: i,
        name: it.name,
        file: it.file,
        duration: 0,
        hotkey: hotkeyList()[i] || '',
        gain_db: it.gain,
      }));
}

/// 按搜索词与标签过滤。**保留原始 index**（data-slot），
/// 否则筛选后点击会播错音效。
function visibleSlots() {
  const q = el.inSearch.value.trim().toLowerCase();
  const tag = state.activeTag;
  return baseSlots().filter(s => {
    const it = state.items[s.index];
    if (tag && !itemTags(it).includes(tag)) return false;
    if (q) {
      const hay = (s.name + ' ' + (it?.tags || '') + ' ' + (s.hotkey || '')).toLowerCase();
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
      : '选一个音效文件夹，点「载入」<br><b>然后把游戏/语音软件的麦克风选成同一个虚拟声卡</b>';
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
    card.className = 'card' + (isActive(s.index) ? ' playing' : '');
    card.dataset.slot = s.index;   // ⚠️ 始终是**原始**槽位号
    const it = state.items[s.index];
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
        <span class="prev" title="试听：只放给你自己听，队友听不到。想让队友听到请点卡片空白处，或按热键">试听</span>
        <span class="dur">${s.duration ? s.duration.toFixed(1) + 's' : ''}</span>
        ${s.hotkey ? `<span class="key">${esc(s.hotkey)}</span>` : ''}
      </div>
      <div class="tags" title="双击可编辑标签">${
        tags.length ? tags.map(t => `<i>${esc(t)}</i>`).join('')
                    : '<i class="addtag">＋ 标签</i>'
      }</div>
      <input class="vol ${volClass(g)}" type="range"
             min="${VOL_MIN}" max="${VOL_MAX}" step="1" value="${g}"
             title="音量 ${g} dB">
      <div class="bar"></div>`;

    card.addEventListener('click', () => play(s.index));

    // 工具栏：前移 / 后移 / 移除
    for (const btn of card.querySelectorAll('.tools button')) {
      btn.addEventListener('click', e => {
        e.stopPropagation();
        const act = btn.dataset.act;
        if (act === 'del') removeItem(s.index);
        else moveItem(s.index, act === 'left' ? -1 : 1);
      });
      btn.addEventListener('pointerdown', e => e.stopPropagation());
    }

    // 双击改名
    const nameEl = card.querySelector('.name');
    nameEl.addEventListener('dblclick', e => {
      e.stopPropagation();
      startRename(nameEl, s.index);
    });

    // 双击标签区改标签
    const tagsEl = card.querySelector('.tags');
    tagsEl.addEventListener('dblclick', e => {
      e.stopPropagation();
      startTagEdit(tagsEl, s.index);
    });
    tagsEl.addEventListener('click', e => e.stopPropagation());

    // 试听：只给自己听
    const pv = card.querySelector('.prev');
    pv.addEventListener('click', async e => {
      e.stopPropagation();
      const file = (state.slots[s.index] || {}).file || it?.file;
      if (!file) { toast('找不到这个音效的文件路径', true); return; }
      pv.classList.add('playing');
      setTimeout(() => pv.classList.remove('playing'), 400);
      try {
        await invoke('preview', { file, gainDb: it?.gain ?? 0 });
        toast('试听中（队友听不到）');
      } catch (err) { toast('试听失败：' + err, true); }
    });
    pv.addEventListener('pointerdown', e => e.stopPropagation());

    const vol = card.querySelector('.vol');
    vol.addEventListener('click', e => e.stopPropagation());
    vol.addEventListener('pointerdown', e => e.stopPropagation());
    vol.addEventListener('input', e => {
      e.stopPropagation();
      const db = +e.target.value;
      setSlotGain(s.index, db);
      vol.className = 'vol ' + volClass(db);
      vol.title = `音量 ${db} dB`;
    });
    el.grid.appendChild(card);
  }
  updateCount();
}

// ─────────────────────────────── 音效库操作

function removeItem(idx) {
  const it = state.items[idx];
  if (!it) return;
  if (state.running) { toast('先点「停止」再改音效库', true); return; }
  state.items.splice(idx, 1);
  syncHotkeys();
  render();
  toast(`已移除「${it.name}」（文件没删）`);
}

function moveItem(idx, delta) {
  const to = idx + delta;
  if (state.running) { toast('先点「停止」再改音效库', true); return; }
  if (to < 0 || to >= state.items.length) return;
  const [it] = state.items.splice(idx, 1);
  state.items.splice(to, 0, it);
  // 热键是按位置对应的，音效换了位置，热键也要跟着换
  const keys = hotkeyList();
  if (keys.length === state.items.length) {
    const [k] = keys.splice(idx, 1);
    keys.splice(to, 0, k);
    el.inKeys.value = keys.join(',');
  }
  render();
}

/// 音效数量变了之后，把热键列表补齐到相同长度
function syncHotkeys() {
  const keys = hotkeyList();
  if (keys.length !== state.items.length) {
    el.inKeys.value = state.items.map((_, i) => keys[i] || ('F' + (i + 1))).join(',');
  }
}

/// 双击标签区进入编辑。
///
/// 分隔符同时认中英文逗号 / 顿号 / 分号 / 竖线 —— 用户打中文时很自然会用「，」，
/// 只认英文逗号会让人以为"输了没反应"。
const TAG_SEP = /[,|;，、；]+/;

function parseTags(text) {
  return [...new Set(
    String(text || '').split(TAG_SEP).map(t => t.trim()).filter(Boolean)
  )];
}

function startTagEdit(tagsEl, idx) {
  if (state.running) { toast('先点「停止」再改标签', true); return; }
  const it = state.items[idx];
  if (!it) return;
  const input = document.createElement('input');
  input.type = 'text';
  input.className = 'taginput';
  input.value = itemTags(it).join(',');
  const others = allTags().filter(t => !itemTags(it).includes(t));
  input.placeholder = others.length ? `可加：${others.slice(0, 3).join('、')}` : '标签1,标签2';
  tagsEl.innerHTML = '';
  tagsEl.appendChild(input);
  input.focus();
  input.select();

  let done = false;
  const finish = (save) => {
    if (done) return;
    done = true;
    if (save) it.tags = parseTags(input.value).join(',');
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
  if (db <= VOL_MIN) return 'muted';
  if (db > 0) return 'boosted';
  return '';
}

// 单音效音量：先改本地状态（未启动时也不丢），启动后立即下发
let slotGainTimer = null;
function setSlotGain(slot, db) {
  if (state.items[slot]) state.items[slot].gain = db;
  if (!state.running) return;
  clearTimeout(slotGainTimer);
  slotGainTimer = setTimeout(() => {
    invoke('set_slot_gain', { slot, db })
      .then(ok => { if (ok) toast(`${slot + 1} 号音量 ${db} dB`); })
      .catch(() => {});
  }, 90);
}

function updateCount() {
  const n = state.items.length;
  el.count.textContent = n ? `（共 ${n} 个）` : '';
}

function isActive(i) {
  const st = state.lastStatus;
  return !!(st && st.playing && st.active_slot === i);
}


function esc(s) {
  return String(s).replace(/[&<>"]/g, c =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}

// ─────────────────────────────── 状态轮询

async function poll() {
  try {
    const st = await invoke('status');
    if (!st) return;
    const wasRunning = state.running;
    state.running = st.running;
    state.lastStatus = st;
    state.slots = st.slots || [];

    // 顶栏：三种状态必须一眼分得清 ——
    //   待机时**麦克风仍在转发**，这点写出来，否则用户会以为"停了队友就听不到我"
    const wasStandby = state.standby;
    state.standby = !st.running && !!st.passthrough;
    // 待机状态变了要刷新按钮文案（轮询是唯一能发现"从别处停了"的途径）
    if (wasStandby !== state.standby) syncToggle();
    el.dot.className = 'dot ' + (!st.running ? (st.passthrough ? 'standby' : 'off')
      : st.overruns > 0 || st.underruns > 0 ? 'warn' : 'ok');
    el.headline.textContent = !st.running
      ? (state.standby
          ? '待机 · 麦克风仍在转发（队友听得到你，音效已关）'
          : '未启动')
      : `${st.device} · 已运行 ${fmtTime(st.uptime_secs)}` +
        (st.hotkeys_active ? ' · 热键已绑定' : ' · 热键未绑定');

    // 程序内部 panic：必须让用户知道 —— 打包后没有控制台，不说就等于没发生
    if (st.last_panic && st.last_panic !== state.lastPanicSeen) {
      state.lastPanicSeen = st.last_panic;
      hint('💥 程序内部出错了：' + st.last_panic +
           ' —— 已记录到 ~/Library/Logs/音效板/panic.log，音频会在几秒内自动恢复。', 'error');
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

    // 电平表 —— 直接回答"队友能不能听到我"
    updateMeter(st);

    // 手机遥控地址：优先显示局域网地址（手机要用的就是它）
    const urls = (st.remote_urls || []).filter(u => !u.includes('127.0.0.1'));
    const best = urls[0] || (st.remote_urls || [])[0];
    el.remote.textContent = best ? '📱 ' + best.replace('http://', '') : '';
    el.remote.title = best ? `手机浏览器打开 ${best}\n（需与电脑在同一局域网）` : '';
    el.remote.dataset.url = best || '';
  

    if (wasRunning !== state.running) { syncToggle(); render(); }

    // 播放进度
    if (st.running) {
      for (const card of el.grid.children) {
        const i = +card.dataset.slot;
        if (Number.isNaN(i)) continue;
        card.classList.toggle('playing', isActive(i));
        const bar = card.querySelector('.bar');
        if (bar) {
          const dur = (state.slots[i] || {}).duration || 0;
          const pct = (isActive(i) && dur > 0)
            ? Math.min(100, (st.playhead_secs / dur) * 100) : 0;
          bar.style.width = pct + '%';
        }
      }
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

el.btnToggle.addEventListener('click', () => state.running ? stop() : start());
el.btnStopAll.addEventListener('click', stopAll);
el.btnBrowse.addEventListener('click', async () => {
  const p = await invoke('pick_dir');
  if (p) { el.inDir.value = p; await scanDir(); }
});
el.btnScan.addEventListener('click', () => scanDir());
el.btnAdd.addEventListener('click', async () => {
  const picked = (await invoke('add_files')) || [];
  if (!picked.length) return;
  const have = new Set(state.items.map(it => it.file));
  let added = 0;
  for (const f of picked) {
    if (!have.has(f)) { state.items.push(makeItem(f)); added++; }
  }
  if (!el.inKeys.value.trim() || hotkeyList().length < state.items.length) {
    el.inKeys.value = state.items.map((_, i) => 'F' + (i + 1)).join(',');
  }
  render();
  hint(added ? `添加了 ${added} 个音效` : '这些文件已经在列表里了', added ? 'ok' : '');
});
el.btnClear.addEventListener('click', () => {
  state.items = []; el.inKeys.value = '';
  render(); hint('已清空音效列表');
});
el.inDir.addEventListener('keydown', e => { if (e.key === 'Enter') scanDir(); });
el.btnSelfTest.addEventListener('click', runSelfTest);
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
  await invoke('release').catch(() => {});
  state.running = false;
  state.standby = false;
  state.slots = [];
  syncToggle();
  render();
  toast('已彻底释放音频设备');
  hint('设备已释放。⚠️ 如果游戏的麦克风还指着虚拟声卡，队友现在听不到你 —— 要么重新「启动」，' +
       '要么把游戏里的麦克风改回真实麦克风。', 'error');
});

/// 这台机器上有没有虚拟声卡？
///
/// 没有的话整个软件都用不了（游戏只能从"麦克风"读声音，而虚拟声卡就是那个麦克风），
/// 所以这种情况必须**主动告诉用户怎么装**，而不是让他对着空的下拉框发愣。
function checkVirtualDevice() {
  const has = state.devices.some(d => d.is_virtual && d.direction.includes('输出'));
  el.vdevHelp.classList.toggle('hidden', has);
  return has;
}
el.btnRecheck.addEventListener('click', async () => {
  await loadDevices();
  if (checkVirtualDevice()) toast('检测到虚拟声卡了 ✅');
  else toast('还是没有 —— 装完 BlackHole 需要重启一次', true);
});
el.btnCopyCmd.addEventListener('click', async () => {
  const cmd = el.vdhCmd.textContent;
  try {
    await navigator.clipboard.writeText(cmd);
    toast('命令已复制，粘到终端里执行');
  } catch {
    // 剪贴板可能没有权限：退回到"选中让用户自己复制"
    const r = document.createRange(); r.selectNodeContents(el.vdhCmd);
    const sel = getSelection(); sel.removeAllRanges(); sel.addRange(r);
    document.execCommand?.('copy');
    toast('命令已选中，按 ⌘C 复制');
  }
});
el.inSearch.addEventListener('input', render);
document.addEventListener('keydown', e => {
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
  // 按名称排序会改变位置 → 热键也会跟着走（热键是按位置对应的）
  state.items.sort((a, b) => a.name.localeCompare(b.name, 'zh'));
  el.inKeys.value = state.items.map((_, i) => 'F' + (i + 1)).join(',');
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
      '手机扫一下就能在游戏里点音效。';
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
  if (state.running) { toast('先点「停止」再自检（两边会抢同一个设备）', true); return; }
  el.btnSelfTest.disabled = true;
  el.btnSelfTest.textContent = '自检中…';
  el.testPanel.className = 'testpanel';
  el.testPanel.innerHTML = '<div class="title">正在送测试音并从环回录回来（约 4 秒）…</div>';
  try {
    const r = await invoke('self_test', { outputDevice: el.selOut.value || null });
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
              '整条链路正常，可以点「启动」进游戏了。</div>';
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
