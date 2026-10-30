// Keep requests from this control page in click order. A slow play request
// must finish before a later stop is sent; aborting fetch cannot cancel an
// operation which the computer may already have accepted.
let commandTail = Promise.resolve();
let commandChoice = 0;
let commandDone = 0;
let pollSerial = 0;

function pct(db) {
  return Math.max(0, Math.min(100, (db + 60) / 60 * 100));
}

function applyVoice(state) {
  const toggle = document.getElementById('venable');
  const preset = document.getElementById('vpreset');
  const bypass = document.getElementById('vbypass');
  if (toggle) toggle.checked = !!state.enabled;
  if (preset) preset.textContent = state.preset_name || '';
  if (bypass) bypass.hidden = !state.bypassed;
  document.querySelectorAll('.vp').forEach(button => {
    const selected = button.dataset.id === state.preset_id;
    if (selected) button.classList.add('on');
    else button.classList.remove('on');
  });
}

async function submit(path, message, element) {
  const choice = ++commandChoice;
  if (element) {
    element.classList.add('on');
    setTimeout(() => element.classList.remove('on'), 260);
  }
  const command = commandTail.catch(() => {}).then(async () => {
    try {
      const response = await fetch(path);
      const result = await response.json();
      if (choice === commandChoice) {
        if (typeof result.preset_id === 'string') applyVoice(result);
        document.getElementById('m').textContent = result.ok
          ? message
          : '失败：' + (result.error || '');
      }
    } catch (error) {
      if (choice === commandChoice) {
        document.getElementById('m').textContent = '连不上电脑';
      }
    }
  });
  commandTail = command;
  try {
    await command;
  } finally {
    // 命令完成（无论成功或失败）都会让之前启动的轮询快照过期。
    commandDone++;
  }
}

function hit(path, element) {
  return submit(path, path === '/stop' ? '已停止' : '已触发', element);
}

function voiceEnable(enabled) {
  return submit(
    enabled ? '/voice/enable/1' : '/voice/enable/0',
    enabled ? '已启用变声' : '已关闭变声',
  );
}

function voicePick(element) {
  return submit(
    '/voice/preset/' + encodeURIComponent(element.dataset.id),
    '已切换预设',
    element,
  );
}

// 状态轮询只更新显示，不占用命令队列。只有轮询启动于最后一次命令完成之后，
// 且期间没有新命令开始或完成时，结果才可应用；否则旧快照会覆盖刚生效的状态。
async function pollVoice() {
  const serial = ++pollSerial;
  const choice = commandChoice;
  const done = commandDone;
  try {
    const response = await fetch('/voice/status');
    const result = await response.json();
    if (
      serial === pollSerial
      && choice === commandChoice
      && done === commandDone
      && result.ok
    ) {
      applyVoice(result);
    }
  } catch (error) {
    // 保持上一次显示；命令本身会报告连接失败。
  }
}

if (typeof setInterval === 'function') setInterval(pollVoice, 1000);

// 待机页没有音效按钮。桌面恢复音效后页面要整体刷新才会出现按钮，所以待机页轮询
// 播放是否可用；变声状态由上面的轮询单独维护，不受这次刷新影响。
async function pollPlayback() {
  try {
    const response = await fetch('/playback');
    const result = await response.json();
    if (result.available) location.reload();
  } catch (error) {
    // 保持待机页，下一轮再试。
  }
}

const standbyMarker = document.getElementById('s');
if (typeof setInterval === 'function' && standbyMarker && standbyMarker.dataset
    && standbyMarker.dataset.standby) {
  setInterval(pollPlayback, 2000);
}

document.addEventListener('keydown', event => {
  if (event.key >= '1' && event.key <= '9') {
    const button = document.querySelectorAll('.b')[+event.key - 1];
    if (button) button.click();
  }
  if (event.key === ' ') {
    hit('/stop', document.body);
    event.preventDefault();
  }
});
