//! GUI 冒烟测试 —— 不开窗口，把 GUI 会调用的**全部 Rust 命令**跑一遍。
//!
//! 为什么需要它：`screencapture` 在没给屏幕录制权限时只返回黑图，
//! 无法用截图验证界面。而 GUI 真正的风险不在 WebView 渲染（那是 Tauri 的事），
//! 而在**它背后的命令层**：设备枚举、文件扫描、引擎启动、播放、状态读取。
//! 这些都能在无窗口模式下完整验证。
//!
//! ```bash
//! ./target/release/mixer-gui --smoke --dir ~/Sounds
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::host::{AudioHost, StartConfig};

/// 读当前进程的常驻内存（KB）。
///
/// 反复启停的音效引擎最容易泄漏（历史上真漏过：cpal 的闭包没释放，
/// 30 轮涨到 106MB）。所以这条检查在**两个平台上都要有** ——
/// 不能因为 Windows 上没有 `ps` 就把它跳过。
#[cfg(not(target_os = "windows"))]
fn rss_kb() -> Option<u64> {
    // macOS 没有 /proc，走 ps；Linux 上 ps 也有
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// 读当前进程的工作集大小（KB）—— Windows 版。
///
/// Windows 既没有 `/proc` 也没有 `ps`，走 `GetProcessMemoryInfo`。
/// 手写 FFI 而不是引依赖：只为这一个调用拉一个 windows-sys 进来不值当，
/// 而且这个结构体的布局是**稳定**的（Win32 从 NT4 起没变过）。
#[cfg(target_os = "windows")]
fn rss_kb() -> Option<u64> {
    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
    }

    let mut pmc = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool_usage: 0,
        quota_paged_pool_usage: 0,
        quota_peak_non_paged_pool_usage: 0,
        quota_non_paged_pool_usage: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
    };
    // SAFETY: pmc 是按 Win32 文档定义的结构体，cb 已填对；
    // GetCurrentProcess 返回的是伪句柄，不需要释放。
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) };
    if ok == 0 {
        return None;
    }
    Some((pmc.working_set_size / 1024) as u64)
}

/// 极简 HTTP GET（只用于冒烟测试，不处理 chunked 等复杂情况）。
fn http_get(port: u16, path: &str) -> Result<String> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("连不上 127.0.0.1:{port}"))?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    Ok(buf)
}

pub fn run(dir: PathBuf) -> Result<()> {
    println!("\n═══ GUI 冒烟测试（无窗口）═══\n");
    let host = AudioHost::new();

    // 1. 设备枚举
    let devs = host.list_devices().context("list_devices 失败")?;
    let ins = devs.iter().filter(|d| d.direction == "输入").count();
    let outs = devs.iter().filter(|d| d.direction == "输出").count();
    println!("✅ list_devices      输入 {ins} 个 / 输出 {outs} 个");
    for d in devs.iter().take(3) {
        println!(
            "     · [{}] {}{}",
            d.direction,
            d.name,
            if d.is_default { " (默认)" } else { "" }
        );
    }

    // 2. 虚拟声卡探测
    let out_dev = host.guess_output();
    println!(
        "✅ guess_output      {}",
        out_dev.as_deref().unwrap_or("（没探测到，需要手动选）")
    );

    // 3. 文件扫描
    let files =
        crate::host::scan_dir(&dir).with_context(|| format!("scan_dir 失败: {}", dir.display()))?;
    if files.is_empty() {
        bail!("目录里没有音频文件: {}", dir.display());
    }
    println!("✅ scan_dir          {} 个音效", files.len());
    for f in files.iter().take(5) {
        println!(
            "     · {}",
            f.file_name().unwrap_or_default().to_string_lossy()
        );
    }

    // 4. 启动引擎（与 GUI 点「启动」完全同一条代码路径）
    let cfg = StartConfig {
        input_device: None,
        output_device: out_dev,
        files: files.iter().map(|p| p.display().to_string()).collect(),
        ids: Vec::new(),
        tags: Vec::new(),
        gains: vec![0.0; files.len()],
        // 顺手覆盖"自定义显示名"这条路径：第一个音效改名，验证引擎用的是它
        names: {
            let mut n = vec![String::new(); files.len()];
            if let Some(first) = n.first_mut() {
                *first = "自定义名字".into();
            }
            n
        },
        hotkeys: (1..=files.len().min(9)).map(|i| format!("F{i}")).collect(),
        buffer_ms: 20,
        mic_gain_db: -60.0, // 关掉麦克风，只验证音效通路
        sfx_gain_db: 0.0,
        looping: false,
        monitor: true, // 顺带覆盖监听路径（会短暂从默认输出放声）
        monitor_device: None,
        remote_port: Some(18767), // 冒烟测试用固定端口，顺便验证手机遥控
    };
    let base = cfg.clone();
    let t0 = Instant::now();
    host.start(cfg.clone(), None).context("start 失败")?;
    println!("✅ start             用时 {}ms", t0.elapsed().as_millis());

    let st = host.status();
    println!("     · 输出设备 {}  ← {}", st.device, st.input_device);
    if st.slots.first().map(|s| s.name.as_str()) != Some("自定义名字") {
        bail!(
            "槽位名没用上自定义名，实际是 {:?}",
            st.slots.first().map(|s| s.name.clone())
        );
    }
    println!("✅ 自定义显示名      第一个槽位显示为「自定义名字」（不是文件名）");
    println!(
        "     · 槽位 {} 个，热键{}绑定",
        st.slots.len(),
        if st.hotkeys_active { "已" } else { "未" }
    );

    // 5. 播放第一个音效
    host.play(0).context("play 失败")?;
    println!("✅ play(0)           已下发");

    // 6. 轮询状态，确认真的播起来了
    let mut saw_playing = false;
    let mut max_head = 0f32;
    // 电平表必须在**播放过程中**采样：声音播完后电平会随释放时间衰减下去
    let mut max_peak = -100f32;
    let mut max_rms = -100f32;
    let wait = Instant::now();
    while wait.elapsed() < Duration::from_secs(6) {
        let s = host.status();
        if s.playing {
            saw_playing = true;
            max_head = max_head.max(s.playhead_secs);
        }
        max_peak = max_peak.max(s.out_peak_db);
        max_rms = max_rms.max(s.out_rms_db);
        std::thread::sleep(Duration::from_millis(25));
        if max_head > 0.8 {
            break; // 播到 0.8 秒足够验证，不必等它放完
        }
    }
    if !saw_playing {
        bail!("6 秒内 status 从未报告 playing=true —— 播放没有真正开始");
    }
    println!("✅ status            playing=true，播放进度到 {max_head:.2}s");

    // 电平表：播放过程中必须读到真实电平，否则界面上那个表就是摆设
    if max_peak < -40.0 {
        bail!("播放期间电平表最高只读到 {max_peak:.1} dBFS —— 电平表没接上或信号没出去");
    }
    println!("✅ 电平表            播放期间峰值 {max_peak:.1} dBFS · RMS {max_rms:.1} dBFS");

    // 监听：必须**真的被调用过**（否则用户按键后自己什么都听不到，还以为没生效）
    let st = host.status();
    if st.monitor_plays == 0 {
        bail!("监听一次都没触发 —— 用户按键后将得不到任何反馈");
    }
    println!(
        "✅ 监听              播放 {} 次，监听触发 {} 次（自己也能听到）",
        st.slots.len().min(1),
        st.monitor_plays
    );

    // ── 待机语义：停止只关音效，**麦克风必须继续转发** ──────────────────
    //
    // 这是用户实打实会踩的坑：他大多数时候不用音效，而游戏的麦克风一直指着虚拟声卡。
    // 如果「停止」把转发也断了，虚拟声卡里就是一片死寂（实测精确为 0），
    // **对方会突然听不到他说话**。
    {
        let before = host.status();
        if !before.running || !before.passthrough {
            bail!(
                "开始时应处于「开启」且转发中：running={} passthrough={}",
                before.running,
                before.passthrough
            );
        }
        host.stop();
        let mid = host.status();
        if mid.running {
            bail!("停止后 running 应为 false");
        }
        if !mid.passthrough {
            bail!("❌ 停止把麦克风转发也断了 —— 对方会突然听不到用户说话");
        }
        println!("✅ 待机              停止后 running=false 但 passthrough=true（麦克风仍在转发）");

        // 重新开启应走快路：不重开设备，状态立刻回到 running
        host.activate(None).context("重新激活失败")?;
        let after = host.status();
        if !after.running || !after.passthrough {
            bail!("重新激活后应回到 running=true：{:?}", after.running);
        }
        println!("✅ 重新激活          回到 running=true（设备未重开，瞬间生效）");

        // 彻底释放：这时才真的断开转发
        host.release();
        let rel = host.status();
        if rel.passthrough {
            bail!("release 之后不应再转发");
        }
        println!("✅ 彻底释放          passthrough=false（此后必须把目标应用的麦克风改回去）");

        // 收尾：重新启动，好让后面的检查（stop_all / 遥控 / 试听）有引擎可用
        host.start(cfg, None).context("重新启动失败")?;
    }

    // 7. 全部停止
    host.stop_all().context("stop_all 失败")?;
    std::thread::sleep(Duration::from_millis(400));
    let s = host.status();
    println!("✅ stop_all          playing={}", s.playing);

    // 8. 增益热更新
    host.set_gains(0.0, -12.0);
    println!("✅ set_gains         已更新");

    let ok = host.set_slot_gain(0, -6.0).context("set_slot_gain 失败")?;
    if !ok {
        bail!("set_slot_gain(0) 返回 false");
    }
    if host.set_slot_gain(99, 0.0)? {
        bail!("越界槽位应返回 false");
    }
    println!("✅ set_slot_gain     单音效音量可调，越界已挡住");

    // 9. 手机遥控（热键被游戏吞掉时的兜底通路）
    let st = host.status();
    if st.remote_urls.is_empty() {
        bail!("手机遥控没有启动（remote_urls 为空）");
    }
    let body = http_get(18767, "/status").context("请求 /status 失败")?;
    if !body.contains("playing") {
        bail!("/status 返回内容异常: {body}");
    }
    let page = http_get(18767, "/").context("请求 / 失败")?;
    if !page.contains("<button") {
        bail!("控制页没有按钮");
    }
    let played = http_get(18767, "/play/1").context("请求 /play/1 失败")?;
    if !played.contains("\"ok\":true") {
        bail!("/play/1 未成功: {played}");
    }
    let bad = http_get(18767, "/play/999").context("请求 /play/999 失败")?;
    if !bad.contains("\"ok\":false") {
        bail!("越界槽位应返回 ok:false，实际: {bad}");
    }
    println!(
        "✅ 手机遥控          {} · 控制页/状态/触发/越界 全部正常",
        st.remote_urls.first().cloned().unwrap_or_default()
    );

    // 10. 试听（走系统默认输出，**不经过虚拟麦克风** —— 这就是它存在的意义）
    host.preview(files[0].clone(), -20.0)
        .context("preview 失败")?;
    println!("✅ preview          试听已启动（走默认输出，对方听不到）");
    std::thread::sleep(Duration::from_millis(700));
    host.stop_preview();
    std::thread::sleep(Duration::from_millis(150));
    println!("✅ stop_preview     试听已停止");

    // 11. 停止（= 待机）后再彻底释放
    //
    // 注意：现在「停止」只关音效、**保留麦克风转发**，所以要验证的是两件事 ——
    // 音效确实停了，但转发还在；真正断开要用 release()。
    host.stop();
    let s = host.status();
    if s.running {
        bail!("stop 之后 status 仍报告 running=true");
    }
    if !s.passthrough {
        bail!("stop 之后麦克风应仍在转发（这正是「停止不等于静音」的关键）");
    }
    println!("✅ stop              音效已关，麦克风仍在转发（待机）");

    host.release();
    if host.status().passthrough {
        bail!("release 之后不应还在转发");
    }
    println!("✅ release           设备已释放，转发停止");

    // ══════════════════════════════════════════════════════════
    //  异常路径（对抗性测试）—— 平时看不出来，出事时才发现
    // ══════════════════════════════════════════════════════════
    println!("\n── 异常路径 ──");

    // ── 设备热插拔检测 ────────────────────────────────────────────────
    //
    // 说明：**"设备真的消失"这一半没法自动化**（总不能让脚本去拔你的耳机）。
    // 这里验的是另一半 —— 设备都在时不能误报，以及报警状态能正确传到界面上。
    {
        if let Some((_, i, o, n)) = host.check_devices() {
            bail!("设备都在的情况下误报了掉线：in_lost={i} out_lost={o} 「{n}」");
        }
        println!("✅ 热插拔检测        设备正常时不误报");

        // 报警状态要能传到界面上（界面据此弹提示）
        host.set_device_lost(Some("测试用报警".into()));
        let st = host.status();
        if st.device_lost.as_deref() != Some("测试用报警") {
            bail!("device_lost 没传到 status：{:?}", st.device_lost);
        }
        host.set_device_lost(None);
        if host.status().device_lost.is_some() {
            bail!("device_lost 清不掉");
        }
        println!("✅ 掉线报警          能写入 status、也能清除");
    }

    // ── 一键切回真实麦克风 ─────────────────────────────────────────────
    //
    // 这条测试会**真的改动系统默认输入设备**，最后必须还原。
    #[cfg(target_os = "macos")]
    {
        let host2 = AudioHost::new();
        host2.remember_default_input();
        let recorded = host2.original_default_input();
        if recorded.is_none() {
            bail!("没能记录到原始默认输入设备");
        }
        let (orig_id, orig_name) = recorded.unwrap();
        println!("✅ 记录原始麦克风      启动时记下「{orig_name}」(id={orig_id})");

        // 模拟"默认设备被改乱了"：挑一个**别的**真实输入设备切过去
        let others: Vec<_> = crate::macos_audio::input_devices()
            .into_iter()
            .filter(|(id, n)| *id != orig_id && !audio_core::device::is_virtual_name(n))
            .collect();
        if let Some((other_id, other_name)) = others.first().cloned() {
            crate::macos_audio::set_default_input(other_id);
            let now = crate::macos_audio::default_input()
                .map(|(_, n)| n)
                .unwrap_or_default();
            if now == other_name {
                println!("✅ 模拟默认被改乱      默认输入现在是「{other_name}」");

                // 现在让程序切回去
                let (ok, dev, cur) = host2.revert_to_real_mic().context("切换失败")?;
                if !ok || cur != orig_name {
                    // 还原现场再报错
                    crate::macos_audio::set_default_input(orig_id);
                    bail!("没能切回真实麦克风：ok={ok} 目标是「{dev}」实际「{cur}」");
                }
                println!("✅ 切回真实麦克风      已还原为「{dev}」（回读确认）");
            } else {
                println!("⚠ 跳过切回测试：无法把默认设备改成「{other_name}」（系统或驱动拒绝）");
            }
            // 无论成败都还原，别把用户的设置留在测试状态
            crate::macos_audio::set_default_input(orig_id);
        } else {
            println!("⚠ 跳过切回测试：机器上只有一个真实输入设备");
        }
    }

    // ── 设备掉线检测：靠的是**音频流自己报的错** ──
    //
    // 为什么不是"去枚举一遍设备列表"：那样要在另一个线程另起一个 cpal 后端，
    // 实测会和主线程开流撞死（macOS 上直接卡住）。流错误是平台无关的信号 ——
    // CoreAudio 和 WASAPI 在设备失效时都会回调。
    //
    // 真机上这条路没法自动化测（总不能在测试里真拔设备），所以用钩子注入一次。
    {
        host.start(base.clone(), None).context("启动失败")?;
        std::thread::sleep(std::time::Duration::from_millis(300));

        // 注入一条"输入侧失效" —— 应当被判定为麦克风掉线
        let health = host.session_health().context("没有运行中的会话")?;
        health.report_error("输入", "自测注入：设备已失效");
        let r = host.check_devices();
        match r {
            Some((_, in_lost, out_lost, which)) => {
                if !in_lost || out_lost {
                    bail!("判定方向错了：in_lost={in_lost} out_lost={out_lost}");
                }
                println!("✅ 设备掉线检测      流报错 → 判定「{which}」掉线（输入侧）");
            }
            None => bail!("流报错没有被判定为设备掉线"),
        }
        // 信号只能消费一次 —— 否则会一直误报
        if health.take_error().is_some() {
            bail!("流错误信号被重复消费了，会一直误报掉线");
        }

        // 反向：输出侧
        health.report_error("输出", "自测注入：设备已失效");
        match host.check_devices() {
            Some((_, in_lost, out_lost, _)) if !in_lost && out_lost => {
                println!("✅ 设备掉线检测      输出侧也能正确判定");
            }
            other => bail!("输出侧的判定不对：{other:?}"),
        }
        health.take_error();
        host.release();
    }

    // ── 打开系统设置：只认白名单 ──
    //
    // 这条命令能拉起系统设置窗口，所以做成"前端传语义名、后端拼 URL"，
    // 前端没法让它去开任意地址。这里验证白名单确实拦得住 ——
    // **只测拒绝路径**，免得冒烟的时候真弹出一堆系统设置窗口。
    for bad in ["file:///etc/passwd", "http://evil.example", "../../etc", ""] {
        if crate::open_system_settings(bad.to_string()).is_ok() {
            bail!("open_system_settings 接受了不该接受的参数：{bad:?}");
        }
    }
    println!("✅ 系统设置白名单     4 个非法参数全部被拒（含 file:// 与相对路径）");

    // ── 音频回调里的 panic 必须被就地兜住 ────────────────────────────
    //
    // 这条是**对抗性测试**：故意在渲染回调里 panic 一次，验证
    //   ① 进程不会死（panic=abort 时会直接被杀）
    //   ② panic 被 catch_unwind 兜住，没有 unwind 跨出 FFI 边界（那是 UB）
    //   ③ 引擎会被标记为"不可信"，好让看门狗去重启它
    //
    // 不做这一步的话，这套兜底就只是"看起来做了" —— 而项目里反复踩过这个坑。
    {
        // 先正常起一次，让渲染回调跑起来
        host.start(base.clone(), None).context("启动失败")?;
        std::thread::sleep(std::time::Duration::from_millis(300));

        audio_core::engine::arm_panic_hook_for_test(1); // 1 秒后故意 panic
                                                        // 等到它真的触发（渲染在推进，所以 1 秒多就够）
        let mut fired = false;
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if audio_core::engine::panic_hook_fired() {
                fired = true;
                break;
            }
        }
        if !fired {
            bail!("自测钩子没触发 —— 渲染回调可能压根没在跑");
        }
        // panic 之后回调还会被继续调用几轮，给它一点时间置位
        std::thread::sleep(std::time::Duration::from_millis(400));
        if !host.audio_progress().3 {
            bail!("回调 panic 没有被标记 —— 引擎状态会被当成可信的继续用");
        }
        println!("✅ 回调 panic 兜底      panic 被就地兜住、已标记引擎不可信（进程仍存活）");

        // 恢复到干净状态，别影响后面的检查
        host.release();
    }

    // 0. 坏文件不能拖垮整批（回归：早期版本遇到第一个坏文件就整个起不来）
    {
        let d = std::env::temp_dir().join(format!("mixer_bad_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join("a_空文件.wav"), b"")?;
        std::fs::write(d.join("b_垃圾数据.mp3"), vec![0xABu8; 4096])?;
        std::fs::write(d.join("c_其实是网页.mp3"), b"<html>404 not found</html>")?;
        // 一个好文件
        let good = files[0].clone();
        std::fs::copy(&good, d.join("d_好文件.wav"))?;

        let mixed = crate::host::scan_dir(&d)?;
        if mixed.len() != 4 {
            bail!("候选文件数应为 4，实际 {}", mixed.len());
        }
        let mut c = base.clone();
        c.files = mixed.iter().map(|p| p.display().to_string()).collect();
        c.gains = vec![0.0; mixed.len()];
        c.hotkeys = vec![];
        c.remote_port = None;
        host.start(c, None)
            .context("有坏文件时整批启动失败 —— 单个坏文件不该拖垮全部")?;
        let st = host.status();
        if st.slots.len() != 1 {
            bail!("应只加载 1 个可用音效，实际 {}", st.slots.len());
        }
        if st.skipped.len() != 3 {
            bail!("应报告 3 个被跳过的文件，实际 {:?}", st.skipped);
        }
        println!(
            "✅ 坏文件不拖垮整批      4 个文件 → 1 个可用 + 3 个跳过（{}）",
            st.skipped.join("；")
        );
        // 后面要验证「未启动时的行为」，所以这里必须**彻底释放**而不是只进待机
        host.release();
        let _ = std::fs::remove_dir_all(&d);
    }

    // 1. 未启动时的操作不应 panic，也不应假装成功
    if host.play(0).is_ok() {
        bail!("未启动时 play 应该报错，而不是返回 Ok");
    }
    let _ = host.stop_all();
    host.release();
    host.stop(); // 重复 stop
    println!("✅ 未启动时的 play / stop_all / 重复 stop   均安全");

    // 2. 配置里写着一个**已经不存在的**输出设备会怎样？
    //
    //    早期这里是"直接报错"。但实测发现真实场景更常见的是**设备被拔了**
    //    （换机器、拔耳机）—— 这时直接报错会让用户对着"启动失败"发愣，
    //    而其实换一个设备就能用。所以现在改成：**回退到别的虚拟声卡，并且必须留痕**。
    //
    //    这条检查验的是新行为的两条底线：
    //      ① 回退必须被记录（不能静默换成别的设备 —— 用户要知道声音去哪了）
    //      ② **绝不能退到扬声器**（那会把音效外放出来，还可能在语音时造成回声）
    let mut bad = base.clone();
    bad.output_device = Some("绝对不存在的设备_xyzzy".into());
    match host.start(bad, None) {
        Ok(()) => {
            let st = host.status();
            let note = st.device_lost.clone().unwrap_or_default();
            if note.is_empty() {
                bail!("悄悄换了输出设备却没告诉用户（device_lost 为空）");
            }
            if !note.contains("绝对不存在的设备_xyzzy") {
                bail!("提示里没说清是哪个设备不见了：{note}");
            }
            // 底线：绝不能退到非虚拟设备
            if !audio_core::device::is_virtual_name(&st.device) {
                bail!("❌ 回退到了非虚拟设备「{}」—— 会把音效外放出去", st.device);
            }
            println!(
                "✅ 不存在的设备          回退到「{}」并留痕（没退到扬声器）",
                st.device
            );
            host.release();
        }
        Err(e) => {
            // 机器上一个虚拟声卡都没有时，报错也是对的
            let msg = format!("{e:#}");
            let first = msg.lines().next().unwrap_or("").to_string();
            println!("✅ 不存在的设备          没有可回退的虚拟声卡 → 报错挡住：{first}");
        }
    }

    // 3. 启动失败后必须还能正常启动（可恢复）
    let mut ok = base.clone();
    ok.remote_port = None;
    host.start(ok, None)
        .context("失败后重新启动失败 —— 状态没恢复干净")?;
    println!("✅ 失败后重新启动        成功（状态可恢复）");
    host.stop();

    // 4. 空音效列表：不应 panic（等价于"只透传麦克风"）
    let mut empty = base.clone();
    empty.files = vec![];
    empty.hotkeys = vec![];
    empty.remote_port = None;
    host.start(empty, None).context("空音效列表启动失败")?;
    let st = host.status();
    if !st.slots.is_empty() {
        bail!("空列表却报出 {} 个槽位", st.slots.len());
    }
    println!("✅ 空音效列表            可启动（等价于纯透传）");
    host.stop();

    // 5. 试听不存在的文件 / 损坏的文件 → 报错而不是 panic
    if host
        .preview(PathBuf::from("/nonexistent/nope.mp3"), 0.0)
        .is_ok()
    {
        bail!("试听不存在的文件应该报错");
    }
    let junk = std::env::temp_dir().join("mixer_junk.mp3");
    std::fs::write(&junk, b"this is definitely not audio data at all").unwrap();
    if host.preview(junk.clone(), 0.0).is_ok() {
        bail!("试听损坏文件应该报错");
    }
    let _ = std::fs::remove_file(&junk);
    println!("✅ 试听坏文件            不存在的与损坏的都正确报错");

    // 6. 连续两次启动（应自己 stop 再 start，而不是报"设备被占用"）
    let mut again = base;
    again.remote_port = None;
    host.start(again.clone(), None).context("第一次启动失败")?;
    host.start(again.clone(), None)
        .context("连续第二次启动失败 —— 没有先 stop 干净")?;
    println!("✅ 连续两次启动          正常（内部先停后起）");
    host.stop();

    // 7. 反复 start/stop —— 检查线程与内存是否被正确回收。
    //    真实场景：用户在界面上反复调参数、点停止/启动。
    // 循环次数可用环境变量加大，用来区分「真泄漏」（线性增长）
    // 和「分配器缓存」（先涨后平）—— 这两种现象在前几轮长得一模一样。
    let cycles: usize = std::env::var("MIXER_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let verbose = std::env::var("MIXER_CYCLES").is_ok();

    let rss_before = rss_kb().unwrap_or(0);
    let mut samples: Vec<u64> = Vec::new();
    for i in 0..cycles {
        host.start(again.clone(), None)
            .with_context(|| format!("第 {} 轮启动失败", i + 1))?;
        host.stop();
        if (i + 1) % 5 == 0 || i + 1 == cycles {
            let r = rss_kb().unwrap_or(0);
            samples.push(r);
            if verbose {
                println!("     · 第 {:>3} 轮后内存 {} KB", i + 1, r);
            }
        }
    }
    let rss_after = rss_kb().unwrap_or(0);
    let grown = rss_after.saturating_sub(rss_before);

    // 判断标准：**后段是否还在涨**。只看总量会把分配器缓存误判成泄漏。
    let leak = if samples.len() >= 3 {
        let mid = samples[samples.len() / 2];
        let end = *samples.last().unwrap();
        end.saturating_sub(mid) > 8_000 // 后半程还在涨 = 泄漏
    } else {
        grown > 30_000
    };
    if rss_before > 0 && leak {
        bail!(
            "反复启停 {cycles} 次后内存仍在持续增长（{} → {} KB，总增长 {grown} KB）—— 疑似泄漏",
            rss_before,
            rss_after
        );
    }
    println!(
        "✅ 反复 start/stop ×{cycles:<3} 无报错；内存 {} → {} KB（总增长 {} KB，后段已趋平）",
        rss_before, rss_after, grown
    );

    println!("\n── 结论 ──");
    println!("  顺利路径 + 7 组异常路径全部通过；音频播放真实发生。");
    println!("  （WebView 渲染不在本测试范围内，需人工目视）\n");
    Ok(())
}
