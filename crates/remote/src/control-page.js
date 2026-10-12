// Keep requests from this control page in click order. A slow play request
// must finish before a later stop is sent; aborting fetch cannot cancel an
// operation which the computer may already have accepted.
let commandTail = Promise.resolve();
let commandChoice = 0;

function pct(db) {
  return Math.max(0, Math.min(100, (db + 60) / 60 * 100));
}

async function hit(path, element) {
  const choice = ++commandChoice;
  element.classList.add('on');
  setTimeout(() => element.classList.remove('on'), 260);
  const command = commandTail.catch(() => {}).then(async () => {
    try {
      const response = await fetch(path);
      const result = await response.json();
      if (choice === commandChoice) {
        document.getElementById('m').textContent = result.ok
          ? path === '/stop' ? '已停止' : '已触发'
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
