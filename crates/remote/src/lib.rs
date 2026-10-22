//! 手机遥控 —— 局域网 HTTP 触发。
//!
//! ## 为什么要它
//!
//! 全局热键是本产品最大的未知风险：**部分全屏独占游戏会吞掉系统级热键**。
//! 如果目标游戏恰好是这种，热键方案就废了，而这是产品唯一的交互入口。
//!
//! 这个模块提供一条**完全不依赖系统键盘事件**的通路：手机浏览器打开
//! `http://<电脑IP>:8765`，点按钮就能触发音效。键盘、游戏、反作弊全都绕开了。
//!
//! ## 实现
//!
//! 只用 `std::net`，零依赖。手写最小 HTTP 解析（只认 GET 路径）。
//!
//! ⚠️ **安全提示**：默认监听 `0.0.0.0`（否则手机连不上），
//! 因此**同一局域网内的任何人都能触发音效**。这是刻意的取舍 ——
//! 它只在本地网络可达，且最坏后果是"对方听到一声笑声"。
//! 需要更严的隔离时用 `--remote-bind 127.0.0.1`。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};

use audio_core::engine::PlaybackControl;

/// 遥控服务看到的**可变状态**。
///
/// 为什么不在启动时直接传 `PlaybackControl`：GUI 里用户可以随时停止/重启音频，
/// 每次重启都会产生新的引擎实例。遥控服务应当**跨重启存活** ——
/// 否则手机上刷新一下页面就再也连不上了。所以控制句柄放这里，可随时替换。
/// 播放发生后的回调。
///
/// 为什么需要它：**触发音效的路径有三条**（界面点击 / 全局热键 / 手机遥控），
/// 而"监听"必须对三条都生效 —— 尤其热键，那才是游戏内的主要方式。
/// 早期监听只挂在界面点击那条路上，导致热键触发时**自己听不到任何声音**。
pub type PlayHook = Arc<dyn Fn(usize) + Send + Sync>;
pub type PlayHandler = Arc<dyn Fn(usize) -> bool + Send + Sync>;
pub type StopHandler = Arc<dyn Fn() + Send + Sync>;

/// 手机页面展示的变声快照。
///
/// 预设以 `(id, name)` 保存：页面只需要展示与选择，完整参数留在宿主。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceRemoteState {
    pub enabled: bool,
    pub bypassed: bool,
    pub preset_id: String,
    pub preset_name: String,
    pub presets: Vec<(String, String)>,
}

/// 宿主提供给遥控服务的变声入口。
///
/// 与播放路由分开保存：待机时播放路由被清除，变声仍需可用；释放会话时
/// 两者一起清除。
pub struct VoiceRoute {
    pub state: Arc<dyn Fn() -> VoiceRemoteState + Send + Sync>,
    pub set_enabled: Arc<dyn Fn(bool) -> bool + Send + Sync>,
    pub select_preset: Arc<dyn Fn(&str) -> bool + Send + Sync>,
}

#[derive(Default)]
pub struct Shared {
    pub control: Mutex<Option<Arc<PlaybackControl>>>,
    pub slots: Mutex<Vec<String>>,
    /// 每次成功触发播放后调用（参数是 0 基槽位号）
    pub on_play: Mutex<Option<PlayHook>>,
    play_handler: Mutex<Option<PlayHandler>>,
    stop_handler: Mutex<Option<StopHandler>>,
    voice_route: Mutex<Option<Arc<VoiceRoute>>>,
}

impl Shared {
    pub fn set(&self, ctl: Arc<PlaybackControl>, slots: Vec<String>) {
        let mut control = self.control.lock().unwrap_or_else(|e| e.into_inner());
        let mut names = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        *control = Some(ctl);
        *names = slots;
    }
    /// Publish a complete host route before either playback operation can observe it.
    /// Legacy callers can still use `set` plus the optional hook/handler setters.
    pub fn set_routed(
        &self,
        ctl: Arc<PlaybackControl>,
        slots: Vec<String>,
        play_handler: PlayHandler,
        stop_handler: StopHandler,
    ) {
        // Every operation taking several state locks follows this order. Readers
        // retain their handler lock until the fallback snapshot is complete, so
        // they cannot combine a missing old handler with a newly published control.
        let mut play = self.play_handler.lock().unwrap_or_else(|e| e.into_inner());
        let mut stop = self.stop_handler.lock().unwrap_or_else(|e| e.into_inner());
        let mut control = self.control.lock().unwrap_or_else(|e| e.into_inner());
        let mut names = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        *play = Some(play_handler);
        *stop = Some(stop_handler);
        *control = Some(ctl);
        *names = slots;
    }
    pub fn clear(&self) {
        let mut play = self.play_handler.lock().unwrap_or_else(|e| e.into_inner());
        let mut stop = self.stop_handler.lock().unwrap_or_else(|e| e.into_inner());
        let mut control = self.control.lock().unwrap_or_else(|e| e.into_inner());
        let mut names = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let mut hook = self.on_play.lock().unwrap_or_else(|e| e.into_inner());
        let mut voice = self.voice_route.lock().unwrap_or_else(|e| e.into_inner());
        *play = None;
        *stop = None;
        *control = None;
        names.clear();
        *hook = None;
        *voice = None;
    }
    pub fn set_hook(&self, hook: PlayHook) {
        *self.on_play.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }
    pub fn set_play_handler(&self, handler: PlayHandler) {
        *self.play_handler.lock().unwrap_or_else(|e| e.into_inner()) = Some(handler);
    }
    pub fn set_stop_handler(&self, handler: StopHandler) {
        *self.stop_handler.lock().unwrap_or_else(|e| e.into_inner()) = Some(handler);
    }
    /// Publish the voice route independently from playback.
    pub fn set_voice_route(&self, route: VoiceRoute) {
        *self.voice_route.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(route));
    }
    pub fn clear_voice_route(&self) {
        *self.voice_route.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    fn voice_route(&self) -> Option<Arc<VoiceRoute>> {
        self.voice_route
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    /// 当前变声快照；没有发布路由（已释放）时为 `None`。
    ///
    /// 先克隆路由再调用状态闭包，避免持锁执行宿主回调。
    pub fn voice_state(&self) -> Option<VoiceRemoteState> {
        let route = self.voice_route()?;
        Some((route.state)())
    }
    /// 执行一次遥控启用命令；路由已被替换时旧结果按失败处理。
    pub fn voice_set_enabled(&self, enabled: bool) -> bool {
        let Some(route) = self.voice_route() else {
            return false;
        };
        let ok = (route.set_enabled)(enabled);
        ok && self.current_voice_route(&route)
    }
    /// 执行一次遥控预设选择；路由已被替换时旧结果按失败处理。
    pub fn voice_select_preset(&self, id: &str) -> bool {
        let Some(route) = self.voice_route() else {
            return false;
        };
        let ok = (route.select_preset)(id);
        ok && self.current_voice_route(&route)
    }
    fn current_voice_route(&self, route: &Arc<VoiceRoute>) -> bool {
        self.voice_route()
            .is_some_and(|current| Arc::ptr_eq(&current, route))
    }
    pub fn play(&self, slot: usize) -> bool {
        let (handler, control, hook) = {
            let handler = self.play_handler.lock().unwrap_or_else(|e| e.into_inner());
            if handler.is_some() {
                (handler.clone(), None, None)
            } else {
                let control = self
                    .control
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let hook = self
                    .on_play
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                (None, control, hook)
            }
        };
        // Host callbacks may clear or replace this Shared. Invoke after all state
        // locks are released; an already cloned handler owns its lifecycle gate.
        if let Some(handler) = handler {
            return handler(slot);
        }
        let ok = control.as_ref().is_some_and(|c| c.play(slot));
        if let Some(hook) = hook.filter(|_| ok) {
            hook(slot);
        }
        ok
    }
    pub fn stop_all(&self) {
        let (handler, control) = {
            let handler = self.stop_handler.lock().unwrap_or_else(|e| e.into_inner());
            if handler.is_some() {
                (handler.clone(), None)
            } else {
                let control = self
                    .control
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                (None, control)
            }
        };
        if let Some(handler) = handler {
            handler();
        } else if let Some(control) = control {
            control.stop_all();
        }
    }
}

pub struct RemoteConfig {
    pub port: u16,
    pub bind: String,
}

/// 启动遥控服务。返回本机可用的访问地址列表，供打印。
pub fn serve(shared: Arc<Shared>, cfg: RemoteConfig) -> Result<Vec<String>> {
    let mut server = Server::start(shared, cfg)?;
    // Legacy CLI API: the process lifetime owns this listener.
    server.shutdown_on_drop = false;
    Ok(server.urls.clone())
}

/// A cancellable listener: changing ports and dropping a host release the socket.
pub struct Server {
    pub urls: Vec<String>,
    pub port: u16,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    shutdown_on_drop: bool,
}
impl Server {
    pub fn start(shared: Arc<Shared>, cfg: RemoteConfig) -> Result<Self> {
        let addr = format!("{}:{}", cfg.bind, cfg.port);
        let listener = TcpListener::bind(&addr)
            .with_context(|| format!("监听 {addr} 失败（端口被占用？换 --remote <端口>）"))?;
        listener.set_nonblocking(true)?;

        let port = listener.local_addr()?.port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();

        let thread = std::thread::spawn(move || {
            while !worker_shutdown.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((s, _)) => {
                        let shared = shared.clone();
                        // 每个连接一个线程：请求量极小，够用且简单
                        std::thread::spawn(move || {
                            let _ = handle(s, &shared);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    Err(e) => {
                        log::warn!("遥控连接失败: {e}");
                        break;
                    }
                }
            }
        });

        // 收集本机 IPv4 地址，方便用户直接在手机上敲
        let bind_all = cfg.bind == "0.0.0.0";
        let mut urls = vec![format!("http://127.0.0.1:{port}")];
        if bind_all {
            for ip in local_ipv4_addrs() {
                urls.push(format!("http://{ip}:{port}"));
            }
        }
        Ok(Self {
            urls,
            port,
            shutdown,
            thread: Some(thread),
            shutdown_on_drop: true,
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if self.shutdown_on_drop {
            self.shutdown.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

fn handle(mut s: TcpStream, shared: &Shared) -> Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;

    let mut reader = BufReader::new(s.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");

    // 把请求头读完（否则某些客户端会重置连接）
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
    }

    if method != "GET" {
        return respond(&mut s, 405, "text/plain; charset=utf-8", "只支持 GET");
    }

    let path = path.split('?').next().unwrap_or("/");

    let (ctl, slots) = {
        let control = shared.control.lock().unwrap_or_else(|e| e.into_inner());
        let slots = shared.slots.lock().unwrap_or_else(|e| e.into_inner());
        (control.clone(), slots.clone())
    };

    // 变声路由独立于播放控制：待机时播放已清空，手机仍能启用与切换预设。
    match path {
        "/voice/status" => {
            let body = match shared.voice_state() {
                Some(state) => voice_json(true, None, &state),
                None => unavailable_voice_json().to_owned(),
            };
            return respond(&mut s, 200, "application/json", &body);
        }
        "/voice/enable/1" | "/voice/enable/0" => {
            let enabled = path == "/voice/enable/1";
            let ok = shared.voice_set_enabled(enabled);
            return respond_voice_command(&mut s, shared, ok, "变声不可用");
        }
        p if p.starts_with("/voice/preset/") => {
            let id = percent_decode(p.trim_start_matches("/voice/preset/"));
            if id.is_empty() {
                return respond(
                    &mut s,
                    404,
                    "application/json",
                    r#"{"ok":false,"error":"没有这个预设"}"#,
                );
            }
            let ok = shared.voice_select_preset(&id);
            return respond_voice_command(&mut s, shared, ok, "没有这个预设");
        }
        _ => {}
    }

    let Some(ctl) = ctl else {
        // 待机时播放路由已清除，但变声路由仍在：页面要能继续操作变声。
        if matches!(path, "/" | "/index.html") {
            if let Some(state) = shared.voice_state() {
                let html = control_page(&[], None, Some(&state));
                return respond(&mut s, 200, "text/html; charset=utf-8", &html);
            }
        }
        return respond(&mut s, 200, "text/html; charset=utf-8", NOT_STARTED_PAGE);
    };
    let voice = shared.voice_state();

    match path {
        "/" | "/index.html" => {
            let html = control_page(&slots, Some(&ctl), voice.as_ref());
            respond(&mut s, 200, "text/html; charset=utf-8", &html)
        }
        "/stop" => {
            shared.stop_all();
            respond(
                &mut s,
                200,
                "application/json",
                r#"{"ok":true,"action":"stop_all"}"#,
            )
        }
        "/status" => {
            let st = ctl.stats();
            // 带上电平：手机上也能看到"对方听到的声音有多大"
            let body = format!(
                r#"{{"playing":{},"slot":{},"playhead":{},"underruns":{},"overruns":{},"peak_db":{:.1},"rms_db":{:.1}}}"#,
                st.playing,
                st.active_slot,
                st.playhead,
                st.underruns,
                st.overruns,
                st.out_peak_db,
                st.out_rms_db
            );
            respond(&mut s, 200, "application/json", &body)
        }
        p if p.starts_with("/play/") => {
            let n: usize = p.trim_start_matches("/play/").parse().unwrap_or(0);
            if n == 0 || n > slots.len() {
                return respond(
                    &mut s,
                    404,
                    "application/json",
                    r#"{"ok":false,"error":"没有这个槽位"}"#,
                );
            }
            let ok = shared.play(n - 1);
            let body = format!(r#"{{"ok":{},"slot":{}}}"#, ok, n);
            respond(&mut s, 200, "application/json", &body)
        }
        _ => respond(&mut s, 404, "text/plain; charset=utf-8", "404"),
    }
}

const NOT_STARTED_PAGE: &str = "<meta charset=utf-8><body style='background:#111;color:#eee;font:16px -apple-system;padding:32px'>                 <h2>音频未启动</h2><p style='color:#888'>请在电脑上点「启动」。</p>                 <script>
function pct(db){return Math.max(0,Math.min(100,(db+60)/60*100));}setTimeout(()=>location.reload(),2000)</script>";

fn unavailable_voice_json() -> &'static str {
    r#"{"ok":false,"error":"音频未启动"}"#
}

fn respond_voice_command(s: &mut TcpStream, shared: &Shared, ok: bool, error: &str) -> Result<()> {
    let body = match shared.voice_state() {
        Some(state) => voice_json(ok, (!ok).then_some(error), &state),
        None => unavailable_voice_json().to_owned(),
    };
    respond(s, 200, "application/json", &body)
}

fn voice_json(ok: bool, error: Option<&str>, state: &VoiceRemoteState) -> String {
    let presets = state
        .presets
        .iter()
        .map(|(id, name)| {
            format!(
                r#"{{"id":"{}","name":"{}"}}"#,
                json_escape(id),
                json_escape(name)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let error = error
        .map(|error| format!(r#","error":"{}""#, json_escape(error)))
        .unwrap_or_default();
    format!(
        r#"{{"ok":{},"enabled":{},"bypassed":{},"preset_id":"{}","preset_name":"{}","presets":[{}]{}}}"#,
        ok,
        state.enabled,
        state.bypassed,
        json_escape(&state.preset_id),
        json_escape(&state.preset_name),
        presets,
        error
    )
}

fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out
}

/// 把 URL 路径段还原成预设 ID；只处理 `%XX`，路径中的 `+` 保持原样。
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_digit(bytes[index + 1]), hex_digit(bytes[index + 2]))
            {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn respond(s: &mut TcpStream, code: u16, ctype: &str, body: &str) -> Result<()> {
    let reason = match code {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(resp.as_bytes())?;
    s.flush()?;
    Ok(())
}

/// 生成手机端控制页。刻意做成大按钮、深色、单手可点。
fn control_page(
    slots: &[String],
    ctl: Option<&PlaybackControl>,
    voice: Option<&VoiceRemoteState>,
) -> String {
    let mut buttons = String::new();
    for (i, name) in slots.iter().enumerate() {
        buttons.push_str(&format!(
            r#"<button class="b" onclick="hit('/play/{}',this)"><span class="n">{}</span><span class="i">{}</span></button>"#,
            i + 1,
            html_escape(name),
            i + 1
        ));
    }
    let status = match ctl {
        Some(ctl) => format!(
            "点一下就播给对方听 · 当前 {}/{}",
            if ctl.stats().playing {
                "播放中"
            } else {
                "待机"
            },
            slots.len()
        ),
        None => "音频待机 · 变声仍可遥控".to_owned(),
    };
    let stop = if ctl.is_some() {
        r#"<button class="stop" onclick="hit('/stop',this)">全部停止</button>"#
    } else {
        ""
    };
    let voice = voice.map(voice_card).unwrap_or_default();
    format!(
        r#"<!doctype html><html lang="zh"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1,user-scalable=no">
<title>音效遥控</title>
<style>
:root{{color-scheme:dark}}
*{{box-sizing:border-box}}
body{{margin:0;padding:16px;background:#111;color:#eee;
  font:16px/1.4 -apple-system,system-ui,"PingFang SC",sans-serif}}
h1{{font-size:17px;font-weight:600;margin:0 0 4px}}
#s{{font-size:13px;color:#888;margin-bottom:16px}}
.grid{{display:grid;grid-template-columns:repeat(2,1fr);gap:12px}}
.b{{display:flex;flex-direction:column;align-items:center;justify-content:center;
  min-height:96px;padding:12px;border:0;border-radius:16px;background:#242424;color:#eee;
  font-size:17px;font-weight:600;cursor:pointer;-webkit-tap-highlight-color:transparent;
  transition:transform .06s,background .12s}}
.b:active{{transform:scale(.96);background:#3a3a3a}}
.b.on{{background:#1f6feb}}
.i{{font-size:12px;color:#777;font-weight:400;margin-top:6px}}
.stop{{width:100%;margin-top:16px;min-height:60px;background:#3a1d1d;color:#ffb4b4;
  border:0;border-radius:16px;font-size:16px;font-weight:600}}
.vcard{{margin-top:16px;padding:14px;border-radius:16px;background:#1a1c20;
  border:1px solid #272b34}}
.vtitle{{display:flex;align-items:center;justify-content:space-between;
  font-weight:600;margin-bottom:10px}}
.vbadge{{font-size:12px;color:#f5c451;border:1px solid #5c4a1e;background:#2c2512;
  border-radius:999px;padding:2px 8px}}
.vrow{{display:flex;align-items:center;justify-content:space-between;gap:12px;
  font-size:14px;color:#c7ccd4;margin:8px 0}}
.vrow strong{{color:#fff;font-weight:600}}
.vlist{{display:grid;grid-template-columns:repeat(2,1fr);gap:8px;margin-top:10px}}
.vp{{min-height:44px;padding:8px;border:1px solid #30343c;border-radius:10px;
  background:#242424;color:#ddd;font-size:14px}}
.vp.on{{background:#1f6feb;border-color:#1f6feb;color:#fff}}
input[role=switch]{{width:48px;height:28px;accent-color:#1f6feb}}
#m{{margin-top:12px;font-size:13px;color:#888;min-height:20px}}
</style></head><body>
<div id="meterwrap" style="position:sticky;top:0;background:#0e0f12;padding:10px 14px;border-bottom:1px solid #272b34">
  <div style="font:11px -apple-system;color:#8b93a1;margin-bottom:5px">送往麦克风的电平</div>
  <div style="position:relative;height:14px;background:#0a0c10;border:1px solid #272b34;border-radius:4px;overflow:hidden">
    <i style="position:absolute;top:0;bottom:0;left:70%;width:10%;background:rgba(34,197,94,.18);border-left:1px solid rgba(34,197,94,.35);border-right:1px solid rgba(34,197,94,.35)"></i>
    <i id="mbar" style="position:absolute;top:2px;bottom:2px;left:0;width:0%;background:#22c55e;border-radius:2px;transition:width .1s"></i>
  </div>
</div>
<h1>音效遥控</h1>
<div id="s">{status}</div>
<div class="grid">{buttons}</div>
{stop}
{voice}
<div id="m"></div>
<script>
{script}
</script></body></html>"#,
        status = status,
        buttons = buttons,
        stop = stop,
        voice = voice,
        script = include_str!("control-page.js")
    )
}

fn voice_card(state: &VoiceRemoteState) -> String {
    let mut presets = String::new();
    for (id, name) in &state.presets {
        let selected = if *id == state.preset_id { " on" } else { "" };
        presets.push_str(&format!(
            r#"<button class="vp{}" data-id="{}" onclick="voicePick(this)">{}</button>"#,
            selected,
            html_escape(id),
            html_escape(name)
        ));
    }
    format!(
        r#"<section class="vcard">
  <div class="vtitle"><span>变声</span><span id="vbypass" class="vbadge"{}>临时旁路</span></div>
  <label class="vrow"><span>启用变声</span><input id="venable" type="checkbox" role="switch"{} onchange="voiceEnable(this.checked)"></label>
  <div class="vrow"><span>当前预设</span><strong id="vpreset">{}</strong></div>
  <div class="vlist">{}</div>
</section>"#,
        if state.bypassed { "" } else { " hidden" },
        if state.enabled { " checked" } else { "" },
        html_escape(&state.preset_name),
        presets
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 列出本机非回环 IPv4 地址（枚举网卡，零依赖）。
/// 把一个 URL 编成二维码矩阵。
///
/// 为什么需要：手机遥控是**热键被游戏吞掉时的唯一退路**，而原本要手输
/// `10.220.179.90:8765` 这种地址 —— 手机上又慢又容易打错。扫一下就好。
///
/// 返回 `(边长, 行优先的布尔矩阵)`，`true` 表示黑块。
/// 返回矩阵而不是图片：前端用 canvas 画，不引入任何图片依赖；
/// 也方便脚本验证（见 `examples/qr_probe.rs`）。
pub fn qr_matrix(text: &str) -> Option<(usize, Vec<bool>)> {
    use qrcode::QrCode;
    let code = QrCode::new(text.as_bytes()).ok()?;
    let w = code.width();
    let colors = code.to_colors();
    Some((
        w,
        colors
            .into_iter()
            .map(|c| c == qrcode::Color::Dark)
            .collect(),
    ))
}

/// macOS 应用防火墙开着时，返回一句**可操作**的提示（手机可能连不上）。
///
/// 手机遥控是热键失效时的兜底通路，如果它悄悄连不上，用户会以为整个程序坏了。
/// 防火墙对**未签名的程序**默认会拦入站连接（或者弹一个容易被忽略的窗）。
#[cfg(target_os = "macos")]
pub fn firewall_hint(binary: &str) -> Option<String> {
    let out = std::process::Command::new("/usr/libexec/ApplicationFirewall/socketfilterfw")
        .arg("--getglobalstate")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // 用 "State = 1" 判断，避免依赖界面语言
    if !text.contains("State = 1") {
        return None;
    }
    Some(format!(
        "macOS 防火墙开着。手机若连不上，执行一次下面两条放行即可：\n\
         \x20 sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add \"{binary}\"\n\
         \x20 sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp \"{binary}\""
    ))
}

#[cfg(not(target_os = "macos"))]
pub fn firewall_hint(_binary: &str) -> Option<String> {
    None
}

/// 找出**手机可能连得上**的本机 IPv4 地址。
///
/// ⚠️ 不能用"UDP connect 8.8.8.8 探测默认路由"那种常见写法：
/// 开发机上默认路由经常走 VPN，探到的是 VPN 的 198.19.x.x，
/// **手机在局域网里根本连不上**（实测踩过）。
/// 所以这里枚举全部网卡，按**名字**剔除 VPN / 点对点 / AWDL 之类的虚拟接口。
#[cfg(unix)]
fn local_ipv4_addrs() -> Vec<String> {
    // 这些前缀的接口不是给局域网用的
    const SKIP: &[&str] = &[
        "utun", "tun", "tap", "ppp", "ipsec", "awdl", "llw", "bridge", "ap", "gif", "stf", "anpi",
    ];
    let mut out: Vec<(u8, String)> = Vec::new(); // (优先级, ip)，优先级小的排前面

    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return Vec::new();
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;

            if ifa.ifa_addr.is_null() {
                continue;
            }
            if (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let flags = ifa.ifa_flags as i32;
            if flags & libc::IFF_UP == 0 || flags & libc::IFF_LOOPBACK != 0 {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name)
                .to_string_lossy()
                .to_string();
            if SKIP.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            let sin = ifa.ifa_addr as *const libc::sockaddr_in;
            let raw = u32::from_be((*sin).sin_addr.s_addr);
            let ip = format!(
                "{}.{}.{}.{}",
                (raw >> 24) & 0xff,
                (raw >> 16) & 0xff,
                (raw >> 8) & 0xff,
                raw & 0xff
            );
            // 有线/无线网卡排最前，其余靠后
            let prio =
                if name.starts_with("en") || name.starts_with("eth") || name.starts_with("wl") {
                    0
                } else {
                    1
                };
            out.push((prio, ip));
        }
        libc::freeifaddrs(ifap);
    }

    out.sort();
    out.dedup_by(|a, b| a.1 == b.1);
    out.into_iter().map(|(_, ip)| ip).collect()
}

/// 非 Unix：退回 UDP 探测（Windows 上另有网络列表 API，暂不引入依赖）
#[cfg(not(unix))]
fn local_ipv4_addrs() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                let ip = addr.ip().to_string();
                if ip != "0.0.0.0" && !ip.starts_with("127.") {
                    out.push(ip);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
