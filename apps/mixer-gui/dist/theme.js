// 在样式加载前恢复外观，避免手动主题与系统主题不同时闪过错误底色。
(() => {
  const storageKey = 'mixer.ui.theme';
  const normalize = value => ['system', 'light', 'dark'].includes(value) ? value : 'system';
  let preference = 'system';
  try { preference = normalize(window.localStorage.getItem(storageKey)); } catch (_) { /* 存储不可用时仍可显示界面。 */ }
  const systemTheme = window.matchMedia?.('(prefers-color-scheme: dark)');
  function apply() {
    document.documentElement.dataset.theme = preference === 'system'
      ? (systemTheme?.matches ? 'dark' : 'light') : preference;
  }
  apply();
  const onSystemChange = () => { if (preference === 'system') apply(); };
  if (systemTheme?.addEventListener) systemTheme.addEventListener('change', onSystemChange);
  else systemTheme?.addListener?.(onSystemChange);
  // 外观是此 WebView 的本机偏好，不进入 GUI / CLI 共用的音频配置与 IPC。
  window.MixerTheme = Object.freeze({
    getPreference: () => preference,
    setPreference(value) {
      preference = normalize(value);
      apply();
      try { window.localStorage.setItem(storageKey, preference); return true; }
      catch (_) { return false; }
    },
  });
})();
