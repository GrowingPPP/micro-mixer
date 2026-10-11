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
//! 它只在本地网络可达，且最坏后果是"队友听到一声笑声"。
//! 需要更严的隔离时用 `--remote-bind 127.0.0.1`。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
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

#[derive(Default)]
pub struct Shared {
    pub control: Mutex<Option<Arc<PlaybackControl>>>,
    pub slots: Mutex<Vec<String>>,
    /// 每次成功触发播放后调用（参数是 0 基槽位号）
    pub on_play: Mutex<Option<PlayHook>>,
}

impl Shared {
    pub fn set(&self, ctl: Arc<PlaybackControl>, slots: Vec<String>) {
        *self.control.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctl);
        *self.slots.lock().unwrap_or_else(|e| e.into_inner()) = slots;
    }
    pub fn clear(&self) {
        *self.control.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    pub fn set_hook(&self, hook: PlayHook) {
        *self.on_play.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }
    fn fire_hook(&self, slot: usize) {
        let h = self
            .on_play
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(h) = h {
            h(slot);
        }
    }
}

pub struct RemoteConfig {
    pub port: u16,
    pub bind: String,
}

/// 启动遥控服务。返回本机可用的访问地址列表，供打印。
pub fn serve(shared: Arc<Shared>, cfg: RemoteConfig) -> Result<Vec<String>> {
    let addr = format!("{}:{}", cfg.bind, cfg.port);
    let listener = TcpListener::bind(&addr)
        .with_context(|| format!("监听 {addr} 失败（端口被占用？换 --remote <端口>）"))?;
    listener.set_nonblocking(false).ok();

    let port = cfg.port;

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let shared = shared.clone();
                    // 每个连接一个线程：请求量极小，够用且简单
                    std::thread::spawn(move || {
                        let _ = handle(s, &shared);
                    });
                }
                Err(e) => log::warn!("遥控连接失败: {e}"),
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
    Ok(urls)
}

fn handle(mut s: TcpStream, shared: &Shared) -> Result<()> {
    let ctl = shared
        .control
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let slots: Vec<String> = shared
        .slots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let ctl = match ctl {
        Some(c) => c,
        None => {
            return respond(
                &mut s,
                200,
                "text/html; charset=utf-8",
                "<meta charset=utf-8><body style='background:#111;color:#eee;font:16px -apple-system;padding:32px'>                 <h2>音频未启动</h2><p style='color:#888'>请在电脑上点「启动」。</p>                 <script>
function pct(db){{return Math.max(0,Math.min(100,(db+60)/60*100));}}setTimeout(()=>location.reload(),2000)</script>",
            )
        }
    };
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

    match path {
        "/" | "/index.html" => {
            let html = control_page(&slots, &ctl);
            respond(&mut s, 200, "text/html; charset=utf-8", &html)
        }
        "/stop" => {
            ctl.stop_all();
            respond(
                &mut s,
                200,
                "application/json",
                r#"{"ok":true,"action":"stop_all"}"#,
            )
        }
        "/status" => {
            let st = ctl.stats();
            // 带上电平：手机上也能看到"队友听到的声音有多大"
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
            let ok = ctl.play(n - 1);
            if ok {
                shared.fire_hook(n - 1); // 监听等副作用由宿主决定
            }
            let body = format!(r#"{{"ok":{},"slot":{}}}"#, ok, n);
            respond(&mut s, 200, "application/json", &body)
        }
        _ => respond(&mut s, 404, "text/plain; charset=utf-8", "404"),
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
fn control_page(slots: &[String], ctl: &PlaybackControl) -> String {
    let st = ctl.stats();
    let mut buttons = String::new();
    for (i, name) in slots.iter().enumerate() {
        buttons.push_str(&format!(
            r#"<button class="b" onclick="hit('/play/{}',this)"><span class="n">{}</span><span class="i">{}</span></button>"#,
            i + 1,
            html_escape(name),
            i + 1
        ));
    }
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
<div id="s">点一下就播给队友听 · 当前 {}/{}</div>
<div class="grid">{buttons}</div>
<button class="stop" onclick="hit('/stop',this)">全部停止</button>
<div id="m"></div>
<script>
function pct(db){{return Math.max(0,Math.min(100,(db+60)/60*100));}}
async function hit(p,el){{
  el.classList.add('on');setTimeout(()=>el.classList.remove('on'),260);
  try{{const r=await fetch(p);const j=await r.json();
    document.getElementById('m').textContent=j.ok?'已触发':'失败：'+(j.error||'');}}
  catch(e){{document.getElementById('m').textContent='连不上电脑';}}
}}
document.addEventListener('keydown',e=>{{
  if(e.key>='1'&&e.key<='9'){{const b=document.querySelectorAll('.b')[+e.key-1];if(b)b.click();}}
  if(e.key===' '){{hit('/stop',document.body);e.preventDefault();}}
}});
</script></body></html>"#,
        if st.playing { "播放中" } else { "待机" },
        slots.len(),
        buttons = buttons
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
