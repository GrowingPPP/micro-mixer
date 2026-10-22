// Keep requests from this control page in click order. A slow play request
// must finish before a later stop is sent; aborting fetch cannot cancel an
// operation which the computer may already have accepted.
let commandTail = Promise.resolve();
let commandChoice = 0;
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
  await command;
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

// 状态轮询只更新显示，不占用命令队列。命令开始后，轮询期间发出的旧回复
// 按代次丢弃，避免旧会话状态覆盖刚生效的开关或预设。
async function pollVoice() {
  const serial = ++pollSerial;
  const choice = commandChoice;
  try {
    const response = await fetch('/voice/status');
    const result = await response.json();
    if (serial === pollSerial && choice === commandChoice && result.ok) {
      applyVoice(result);
    }
  } catch (error) {
    // 保持上一次显示；命令本身会报告连接失败。
  }
}

if (typeof setInterval === 'function') setInterval(pollVoice, 1000);

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
