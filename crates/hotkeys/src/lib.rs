//! 全局热键 —— 不切出游戏就能触发播放。
//!
//! 这是产品的核心体验：用户 99% 的时间在游戏里，任何需要切出去的操作都是设计失败。
//!
//! ## 为什么走系统级热键注册而不是键盘钩子
//!
//! macOS 用 Carbon `RegisterEventHotKey`、Windows 用 `RegisterHotKey`：
//! - **不需要辅助功能权限**（键盘钩子需要，会劝退大量用户）
//! - **不是按键监听**，因此不涉及隐私敏感的全局键盘记录
//! - 在绝大多数全屏游戏里可用
//!
//! ⚠️ 已知风险：少数全屏独占游戏会吞掉全局热键。真机实测见 M0 实验 4；
//! 预案是"手机遥控"（见 `docs/planning/任务清单.md` 的 M1-15）。
//!
//! ## ⚠️ macOS 的硬性要求（踩过坑，别再踩）
//!
//! `global-hotkey` 的文档写得很明确：
//!
//! > On macOS, an event loop must be running on the main thread so you also need to
//! > create the global hotkey manager on the main thread.
//!
//! 也就是说，在 macOS 上要收到热键，**两个条件缺一不可**：
//!
//! 1. **管理器必须在主线程创建**（本模块的 [`is_main_thread`] 可检测）
//! 2. **主线程必须真的在跑运行循环**
//!
//! 第 2 条极易踩坑：命令行程序的主线程如果写成 `loop { poll(); sleep(10ms) }`，
//! 热键**永远不会触发**，而且不报任何错 —— 看起来就像"按键没反应"。
//!
//! ⚠️ **实测更正**：光把 `sleep` 换成 [`pump_events`]（`CFRunLoopRunInMode`）**并不够**。
//! 真机验证结果：
//!
//! | 场景 | 主线程在做什么 | 热键（**均为真人按真键实测**） |
//! |---|---|---|
//! | GUI（Tauri） | tao 的 NSApplication 事件循环 | ✅ 响应 |
//! | 命令行 + [`run_app_event_loop`] | 手泵 `ReceiveNextEvent` | ✅ **响应**（F1/F2 均收到） |
//! | 命令行 + 裸 `CFRunLoopRunInMode` | 只跑 CFRunLoop | ❌ 无反应 |
//! | 命令行 + `thread::sleep` | 什么都没跑 | ❌ 无反应 |
//! | 命令行 + Carbon 原生 `RunApplicationEventLoop` | 能收事件但**停不下来** | ⚠️ `QuitApplicationEventLoop` 打不断它（实测挂死） |
//!
//! 结论：Carbon 热键需要**真正在泵事件队列的应用事件循环**（[`run_app_event_loop`]），
//! 只跑裸 CFRunLoop 不够。
//!
//! ## 命令行程序的正确写法
//!
//! 主线程**不能**用来做别的事，必须让出来跑应用事件循环；其余逻辑放工作线程：
//!
//! ```no_run
//! # use std::time::Duration;
//! // 1. 主线程绑定（硬性要求）
//! let hk = hotkeys::Hotkeys::bind_to_slots(&["F1".into()])?;
//! // 2. 轮询移到工作线程
//! std::thread::spawn(move || loop {
//!     if let Some(slot) = hk.poll() { println!("触发 {slot}"); }
//!     std::thread::sleep(Duration::from_millis(10));
//! });
//! // 3. 主线程跑应用事件循环（阻塞）
//! hotkeys::run_app_event_loop();
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! GUI（Tauri）那边由 tao 负责事件循环，所以只要保证管理器在主线程创建即可。

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::sync::Arc;

// ─────────────────────────────────────────── 平台：事件循环

/// 手动泵一次 **Carbon 事件队列**（返回是否处理了一个事件）。
///
/// 这是 `RunApplicationEventLoop()` 内部做的事，但我们自己来 ——
/// 因为 Carbon 那两个函数在纯命令行进程里是**单向门**：
/// `QuitApplicationEventLoop()` 打不断 `RunApplicationEventLoop()`（实测挂死）。
///
/// 只跑裸 `CFRunLoop` 也不够：真机实测收不到热键事件，
/// 必须把事件 `SendEventToEventTarget` 到应用事件目标。
#[cfg(target_os = "macos")]
pub fn pump_carbon_events(timeout_secs: f64) -> bool {
    use std::ffi::c_void;

    // Carbon 的事件队列**不是线程安全的**：两个线程同时 ReceiveNextEvent
    // 会偶发 SIGSEGV（实测并行跑单元测试时 8 次崩 1 次）。
    // 生产环境只从应用循环线程调用，这里的锁永远不竞争，代价可忽略。
    static PUMP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = PUMP_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    type EventRef = *mut c_void;
    type EventTargetRef = *mut c_void;
    type OSStatus = i32;

    extern "C" {
        fn ReceiveNextEvent(
            in_num_types: u32,
            in_list: *const c_void,
            in_timeout: f64,
            in_pull_event: u8, // Carbon Boolean = unsigned char
            out_event: *mut EventRef,
        ) -> OSStatus;
        fn SendEventToEventTarget(event: EventRef, target: EventTargetRef) -> OSStatus;
        fn ReleaseEvent(event: EventRef) -> OSStatus;
        fn GetApplicationEventTarget() -> EventTargetRef;
    }

    unsafe {
        let mut ev: EventRef = std::ptr::null_mut();
        // inNumTypes=0 + inList=NULL → 接收任意类型事件
        let status = ReceiveNextEvent(0, std::ptr::null(), timeout_secs, 1, &mut ev);
        if status == 0 && !ev.is_null() {
            SendEventToEventTarget(ev, GetApplicationEventTarget());
            ReleaseEvent(ev);
            return true;
        }
        false
    }
}

/// 非 macOS：没有 Carbon，退化为 sleep。
#[cfg(not(target_os = "macos"))]
pub fn pump_carbon_events(timeout_secs: f64) -> bool {
    std::thread::sleep(std::time::Duration::from_secs_f64(timeout_secs.max(0.001)));
    false
}

static APP_LOOP_QUIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 运行**应用事件循环**（阻塞，直到 [`quit_app_event_loop`]）。
///
/// 这是 Carbon 热键能工作的真正前提。实测对照：
///
/// | 主线程在做什么 | 热键 |
/// |---|---|
/// | tao 的 NSApplication 事件循环（GUI） | ✅ 响应 |
/// | 裸 `CFRunLoopRunInMode` | ❌ 无反应 |
/// | `thread::sleep` | ❌ 无反应 |
/// | **本函数（ReceiveNextEvent 手泵）** | ✅ **响应**（真人按 fn+F1/F2 实测通过） |
///
/// 与 Carbon 原生 `RunApplicationEventLoop` 的区别：本实现每轮把控制权还回来，
/// 因此**可以被 [`quit_app_event_loop`] 打断**（原生那个打不断）。
pub fn run_app_event_loop() {
    // 50ms 超时：既不会空转烧 CPU，按键延迟也感知不到
    run_loop_until_quit(|| {
        pump_carbon_events(0.05);
    });
}

/// 事件循环的**纯控制逻辑**：反复调用 `pump` 直到要求退出。
///
/// 与 Carbon 解耦是为了可测 —— 在单元测试里调用真实的 `ReceiveNextEvent`
/// 会与其它并行测试争抢同一个非线程安全的事件队列，导致偶发 SIGSEGV。
fn run_loop_until_quit<F: FnMut()>(mut pump: F) {
    use std::sync::atomic::Ordering;
    // 注意：**不在这里重置**退出标志。
    // 否则若工作线程在进入本函数之前就决定退出，重置会把信号吃掉 → 永久阻塞。
    // 调用方应在**启动工作线程之前**调用 [`reset_app_event_loop`]。
    while !APP_LOOP_QUIT.load(Ordering::Relaxed) {
        pump();
    }
}

/// 清除退出标志。调用方应在启动工作线程**之前**调用，避免竞态。
pub fn reset_app_event_loop() {
    APP_LOOP_QUIT.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// 让 [`run_app_event_loop`] 返回。可从任意线程调用。
pub fn quit_app_event_loop() {
    APP_LOOP_QUIT.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// 当前线程是否为主线程。
///
/// macOS 上热键管理器必须在主线程创建；这个函数用来在出错时给出**可操作的**
/// 报错，而不是让用户对着"按键没反应"发呆。
#[cfg(target_os = "macos")]
pub fn is_main_thread() -> bool {
    extern "C" {
        fn pthread_main_np() -> i32;
    }
    unsafe { pthread_main_np() == 1 }
}

/// Windows：主线程必须**显式登记**，否则一律当成"不是主线程"。
///
/// ## 为什么不能像原来那样直接返回 `true`
///
/// 原来这里是 `true` —— 那不是检查，是个**桩**。后果是一条完整的静默失效链：
///
///   ① `bind_hotkeys_on_main_thread` 以为已经在主线程 → 不做调度
///   ② `GlobalHotKeyManager` 在 **tokio 工作线程**上创建（隐藏窗口也建在那儿）
///   ③ 那个线程**不泵消息** → `WM_HOTKEY` 永远不派发
///   ④ **热键完全不响应**，而且不报任何错
///
/// Windows 上 `WM_HOTKEY` 是投递到**创建窗口那个线程**的消息队列的，
/// 所以创建线程必须在跑消息循环 —— 这一点和 macOS 的 Carbon 要求**是同一类约束**。
/// Tauri 的主线程在跑事件循环，tokio 工作线程不在。
///
/// 拿不准时返回 `false` 是**安全方向**：那会强制把绑定调度到主线程，
/// 多一次往返而已；反过来的错误是热键彻底不工作。
#[cfg(target_os = "windows")]
pub fn is_main_thread() -> bool {
    use std::sync::atomic::Ordering;
    let noted = MAIN_THREAD_ID.load(Ordering::Acquire);
    noted != 0 && noted == current_thread_id()
}

/// Windows 上记录"主线程是谁"。**必须从主线程调用一次**（启动时）。
///
/// 不做自动探测：靠"第一次调用发生在主线程"这种假设太脆，
/// 而错了的代价是热键静默失效。显式登记，错了立刻看得出来。
#[cfg(target_os = "windows")]
pub fn note_main_thread() {
    MAIN_THREAD_ID.store(current_thread_id(), std::sync::atomic::Ordering::Release);
}

#[cfg(target_os = "windows")]
static MAIN_THREAD_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(target_os = "windows")]
fn current_thread_id() -> u32 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    // SAFETY: GetCurrentThreadId 无参数、无副作用失败，恒返回当前线程 ID
    unsafe { GetCurrentThreadId() }
}

/// 其他平台（Linux 等）：没有"必须在主线程"的约束，永远返回 true 即可。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn is_main_thread() -> bool {
    true
}

/// 其他平台不需要登记，留一个空实现让调用方不用写 cfg。
#[cfg(not(target_os = "windows"))]
pub fn note_main_thread() {}

/// 轮询句柄：**只读 global-hotkey 的全局事件通道 + 一张 id→槽位表**。
///
/// 为什么要从 [`Hotkeys`] 里拆出来：
/// `GlobalHotKeyManager` 在 **Windows 上不实现 `Send`**（它就是个 `HWND` 包装，
/// 而且 `Drop` 会调 `DestroyWindow` —— 那个**必须**在创建窗口的线程上做）。
/// 但事件走的是 global-hotkey 的**全局通道**，轮询线程压根不需要碰管理器。
///
/// 拆开之后：轮询线程只拿 `HotkeyPoller`（天然 `Send`），管理器留在创建它的线程上 ——
/// **两边都不需要 `unsafe impl Send`**。macOS 上本来靠 global-hotkey 的
/// `unsafe impl Send` 蒙混过关，Windows 上没有那个 impl，问题才暴露出来。
#[derive(Clone, Default)]
pub struct HotkeyPoller {
    /// 系统热键 id → 槽位索引
    id_to_slot: HashMap<u32, usize>,
}

impl HotkeyPoller {
    /// 非阻塞取出一个被按下的槽位。没有则返回 None。
    ///
    /// 读的是 global-hotkey 的**全局通道** —— 这正是它可以跨线程用的原因。
    pub fn poll(&self) -> Option<usize> {
        let rx = GlobalHotKeyEvent::receiver();
        let mut hit = None;
        while let Ok(ev) = rx.try_recv() {
            if ev.state != HotKeyState::Pressed {
                continue;
            }
            if let Some(&slot) = self.id_to_slot.get(&ev.id()) {
                hit = Some(slot);
            }
        }
        hit
    }

    pub fn is_empty(&self) -> bool {
        self.id_to_slot.is_empty()
    }

    pub fn len(&self) -> usize {
        self.id_to_slot.len()
    }
}

/// 热键管理器：把一组键位绑定到音效槽位。
pub struct Hotkeys {
    /// 必须持有 —— Drop 时会注销所有热键
    _manager: ManagerOwner,
    /// 系统热键 id → 槽位索引
    id_to_slot: HashMap<u32, usize>,
    /// 便于日志与报错
    bindings: Vec<(String, usize)>,
    /// 已注册的键对象 —— 待机时要拿它去注销
    keys: Vec<HotKey>,
}

/// 管理器的持有者。
///
/// ## 为什么需要这一层
///
/// `GlobalHotKeyManager` 在 **Windows 上不实现 `Send`**（它就是个 `HWND` 包装），
/// 而我们的架构里 `Hotkeys` 必须能跨线程传递：
/// 它在主线程创建（两个平台都要求），然后通过 channel 送回调用方，
/// 还要存进 Tauri 的 `State<AudioHost>`（要求 `Send + Sync`）。
///
/// ## 为什么断言 `Send` 是成立的
///
/// 逐个操作核对过线程亲和性：
///
/// | 操作 | 线程亲和性 |
/// |---|---|
/// | `RegisterHotKey(hwnd, …)` | **无** —— 关联表是进程全局的；`WM_HOTKEY` 仍然投递到**创建窗口那个线程**的消息队列 |
/// | `UnregisterHotKey(hwnd, …)` | **无** |
/// | `Drop` → `DestroyWindow(hwnd)` | ⚠️ **有** —— 必须在创建窗口的线程上调用 |
///
/// 所以前两个跨线程没问题（消息的派发线程由窗口决定，而我们本来就在主线程泵消息），
/// **只有析构是问题** —— 见下面 `Drop` 里的处理。
///
/// > macOS 上不需要这套：global-hotkey 自己写了 `unsafe impl Send`。
struct ManagerOwner(Option<Arc<GlobalHotKeyManager>>);

// SAFETY: 依据见上面那张表。
// · register / unregister 无线程亲和性，跨线程调用安全；
// · 唯一有亲和性的 DestroyWindow 被下面的 Drop 显式绕开了。
#[cfg(target_os = "windows")]
unsafe impl Send for ManagerOwner {}
#[cfg(target_os = "windows")]
unsafe impl Sync for ManagerOwner {}

impl Drop for ManagerOwner {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        {
            // Windows 的 `GlobalHotKeyManager::drop` 会调 `DestroyWindow`，
            // 而那个**必须**在创建窗口的线程上做。我们保证不了析构发生在那里
            // （`Hotkeys` 会随 `AudioHost` 一起被释放，那可能在任何线程）。
            //
            // 与其在错误的线程上析构（`DestroyWindow` 会失败，行为未定义），
            // 不如**故意泄漏**：代价是一个隐藏窗口，进程退出时由系统回收。
            // 触发频率很低 —— 只在待机↔开启切换、重新绑定时发生。
            if let Some(m) = self.0.take() {
                std::mem::forget(m);
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.0.take();
        }
    }
}

impl Hotkeys {
    /// 把 `keys[i]` 绑定到槽位 `i`。
    ///
    /// 键位写法：`F1`、`F12`、`Cmd+Shift+A`、`Ctrl+Alt+1`、`Numpad1`、`Backslash`。
    pub fn bind_to_slots(keys: &[String]) -> Result<Self> {
        let manager = Arc::new(Self::new_manager()?);
        Self::bind_with(manager, keys)
    }

    /// 只创建管理器，不绑定。**macOS 上必须在主线程调用**。
    ///
    /// GUI 用它先在主线程把管理器建好（Tauri 的 `run_on_main_thread`），
    /// 之后再按用户选的音效去绑定。
    pub fn new_manager() -> Result<GlobalHotKeyManager> {
        if !is_main_thread() {
            anyhow::bail!(
                "全局热键管理器必须在**主线程**创建（当前不在主线程）。\
                 macOS 的 Carbon 热键事件由主线程运行循环派发，在别的线程创建会静默失效。"
            );
        }
        GlobalHotKeyManager::new().context("初始化全局热键管理器失败")
    }

    /// 用一个已有的管理器绑定键位。
    pub fn bind_with(manager: Arc<GlobalHotKeyManager>, keys: &[String]) -> Result<Self> {
        let mut id_to_slot = HashMap::new();
        let mut bindings = Vec::new();

        let mut raw = Vec::new();
        for (slot, spec) in keys.iter().enumerate() {
            let hk = parse_hotkey(spec)
                .with_context(|| format!("热键「{spec}」无法解析（第 {} 个）", slot + 1))?;
            manager
                .register(hk)
                .with_context(|| format!("热键「{spec}」注册失败 —— 可能已被系统或其他程序占用"))?;
            id_to_slot.insert(hk.id(), slot);
            bindings.push((spec.clone(), slot));
            raw.push(hk);
        }

        Ok(Self {
            _manager: ManagerOwner(Some(manager)),
            id_to_slot,
            bindings,
            keys: raw,
        })
    }

    /// 注销全部热键，**把按键还给系统（游戏）**。
    ///
    /// 用途：切到「待机（只转发麦克风）」时必须注销 —— 否则 F1/F2 仍然被我们截走，
    /// 用户在游戏里按这两个键会毫无反应。
    /// 保留 `bindings`（配置），这样重新激活时不用再问用户要键位。
    pub fn unbind_all(&mut self) -> Result<()> {
        if self.keys.is_empty() {
            return Ok(());
        }
        self.manager()?
            .unregister_all(&self.keys)
            .context("注销热键失败")?;
        self.keys.clear();
        self.id_to_slot.clear();
        Ok(())
    }

    /// 当前是否真的有键处于注册状态。
    pub fn is_bound(&self) -> bool {
        !self.keys.is_empty()
    }

    /// 注销后用**同一个管理器**重新绑定。
    ///
    /// 为什么不重新 `new_manager()`：macOS 上创建 Carbon 热键管理器要求主线程 +
    /// 事件循环正在跑，在"刚注销完"的时机再建一个会超时（实测报 Operation timed out）。
    /// 复用已有的管理器既稳又快。
    pub fn rebind(&mut self, keys: &[String]) -> Result<()> {
        self.unbind_all()?;
        let mut id_to_slot = HashMap::new();
        let mut bindings = Vec::new();
        let mut raw = Vec::new();
        for (slot, spec) in keys.iter().enumerate() {
            let hk = parse_hotkey(spec)
                .with_context(|| format!("热键「{spec}」无法解析（第 {} 个）", slot + 1))?;
            self.manager()?
                .register(hk)
                .with_context(|| format!("热键「{spec}」重新注册失败"))?;
            id_to_slot.insert(hk.id(), slot);
            bindings.push((spec.clone(), slot));
            raw.push(hk);
        }
        self.id_to_slot = id_to_slot;
        self.bindings = bindings;
        self.keys = raw;
        Ok(())
    }

    /// 取管理器。正常路径上它一定在 —— `None` 只可能出现在析构过程中。
    fn manager(&self) -> Result<&GlobalHotKeyManager> {
        self._manager.0.as_deref().context("热键管理器已被释放")
    }

    /// 拿一个可以跨线程用的轮询句柄。
    pub fn poller(&self) -> HotkeyPoller {
        HotkeyPoller {
            id_to_slot: self.id_to_slot.clone(),
        }
    }

    /// 非阻塞取出一个被按下的槽位（本线程内直接用，等价于 `self.poller().poll()`）。
    pub fn poll(&self) -> Option<usize> {
        self.poller().poll()
    }

    /// 已绑定的键位数。
    pub fn len(&self) -> usize {
        self.id_to_slot.len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_to_slot.is_empty()
    }

    /// 返回 `(键位写法, 槽位)` 列表，用于启动时打印。
    pub fn bindings(&self) -> &[(String, usize)] {
        &self.bindings
    }
}

/// 解析键位写法，例如 `Cmd+Shift+A`。
pub fn parse_hotkey(spec: &str) -> Result<HotKey> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(anyhow!("键位为空"));
    }

    let mut mods = Modifiers::empty();
    let mut code: Option<Code> = None;

    for part in spec.split('+') {
        let p = part.trim();
        if p.is_empty() {
            return Err(anyhow!("键位里有空的片段：{spec}"));
        }
        match p.to_ascii_lowercase().as_str() {
            "cmd" | "command" | "super" | "meta" | "win" => mods |= Modifiers::SUPER,
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "alt" | "option" | "opt" => mods |= Modifiers::ALT,
            "shift" => mods |= Modifiers::SHIFT,
            other => {
                if code.is_some() {
                    return Err(anyhow!("一个热键只能有一个主键：{spec}"));
                }
                code = Some(parse_code(other)?);
            }
        }
    }

    let code = code.ok_or_else(|| anyhow!("缺少主键（只有修饰键）：{spec}"))?;
    let mods = if mods.is_empty() { None } else { Some(mods) };
    Ok(HotKey::new(mods, code))
}

/// 解析主键名。宽松匹配几种常见写法。
fn parse_code(s: &str) -> Result<Code> {
    let lower = s.to_ascii_lowercase();

    // F1..F24
    if let Some(n) = lower.strip_prefix('f') {
        if let Ok(n) = n.parse::<u8>() {
            if (1..=24).contains(&n) {
                return Code::from_str_with_fallback(&format!("F{n}"));
            }
        }
    }

    // 单个字母 a-z
    if lower.len() == 1 && lower.chars().all(|c| c.is_ascii_alphabetic()) {
        return Code::from_str_with_fallback(&format!("Key{}", lower.to_ascii_uppercase()));
    }

    // 单个数字 0-9
    if lower.len() == 1 && lower.chars().all(|c| c.is_ascii_digit()) {
        return Code::from_str_with_fallback(&format!("Digit{lower}"));
    }

    // 其它按标准名/别名解析
    let alias = match lower.as_str() {
        "space" | "空格" => "Space",
        "enter" | "return" => "Enter",
        "esc" | "escape" => "Escape",
        "tab" => "Tab",
        "backslash" | "\\" => "Backslash",
        "semicolon" | ";" => "Semicolon",
        "quote" | "'" => "Quote",
        "comma" | "," => "Comma",
        "period" | "." => "Period",
        "slash" | "/" => "Slash",
        "minus" | "-" => "Minus",
        "equal" | "=" => "Equal",
        "bracketleft" | "[" => "BracketLeft",
        "bracketright" | "]" => "BracketRight",
        "backquote" | "`" => "Backquote",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "insert" => "Insert",
        "delete" | "del" => "Delete",
        "arrowup" | "up" => "ArrowUp",
        "arrowdown" | "down" => "ArrowDown",
        "arrowleft" | "left" => "ArrowLeft",
        "arrowright" | "right" => "ArrowRight",
        other => {
            // numpad / 其它标准名原样尝试
            return Code::from_str_with_fallback(other);
        }
    };
    Code::from_str_with_fallback(alias)
}

/// `Code` 的 `FromStr` 对大小写敏感，这里做一次规范化重试。
trait CodeExt {
    fn from_str_with_fallback(s: &str) -> Result<Code>;
}

impl CodeExt for Code {
    fn from_str_with_fallback(s: &str) -> Result<Code> {
        use std::str::FromStr;
        if let Ok(c) = Code::from_str(s) {
            return Ok(c);
        }
        // Numpad1 / NumpadAdd 之类的驼峰名
        if let Some(rest) = s
            .strip_prefix("numpad")
            .or_else(|| s.strip_prefix("Numpad"))
        {
            let cand = format!("Numpad{}", capitalize(rest));
            if let Ok(c) = Code::from_str(&cand) {
                return Ok(c);
            }
        }
        Err(anyhow!(
            "无法识别的键名「{s}」。示例：F1 / F12 / A / 5 / Space / Numpad1"
        ))
    }
}

fn capitalize(s: &str) -> String {
    let mut ch = s.chars();
    match ch.next() {
        Some(f) => f.to_uppercase().collect::<String>() + ch.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod platform_tests {
    use super::*;

    #[test]
    fn 事件循环必须能被外部打断() {
        // 回归测试：早期用 Carbon 原生的 RunApplicationEventLoop，
        // QuitApplicationEventLoop 在纯命令行进程里打不断它 —— 实测挂死。
        //
        // 这里用**假泵**测控制逻辑，不碰真实 Carbon：
        // 真实 ReceiveNextEvent 不是线程安全的，并行测试会偶发 SIGSEGV。
        // 真实泵由 examples/app_loop_test.rs 单线程验证。
        reset_app_event_loop();
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            quit_app_event_loop();
        });
        let t0 = std::time::Instant::now();
        let mut pumped = 0usize;
        run_loop_until_quit(|| {
            pumped += 1;
            std::thread::sleep(std::time::Duration::from_millis(5));
        });
        let el = t0.elapsed().as_secs_f64();
        assert!(el < 3.0, "事件循环没被打断，耗时 {el:.2}s");
        assert!(pumped > 5, "循环应该真的转了很多轮，实际 {pumped}");
    }

    #[test]
    fn 退出标志必须在起线程前重置才不丢信号() {
        // 如果工作线程在进入循环前就决定退出，而循环入口又重置了标志，
        // 主线程就会永久阻塞。这里锁住"重置由调用方负责"这个约定。
        quit_app_event_loop(); // 提前置位
        reset_app_event_loop();
        let t0 = std::time::Instant::now();
        run_loop_until_quit(|| {});
        assert!(t0.elapsed().as_secs_f64() < 0.5);
    }

    #[test]
    fn 主线程检测应可用() {
        // 测试线程一般不是主线程；这里只要求它能被调用且不 panic
        let _ = is_main_thread();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析功能键() {
        assert!(parse_hotkey("F1").is_ok());
        assert!(parse_hotkey("F12").is_ok());
        assert!(parse_hotkey("f5").is_ok(), "应接受小写");
        assert!(parse_hotkey("F25").is_err(), "F25 越界");
        assert!(parse_hotkey("F0").is_err(), "F0 越界");
    }

    #[test]
    fn 解析字母与数字键() {
        assert!(parse_hotkey("A").is_ok());
        assert!(parse_hotkey("z").is_ok());
        assert!(parse_hotkey("1").is_ok());
    }

    #[test]
    fn 解析组合键() {
        for spec in [
            "Cmd+Shift+A",
            "Ctrl+Alt+1",
            "Alt+F4",
            "cmd+option+space",
            "Shift+F1",
        ] {
            assert!(parse_hotkey(spec).is_ok(), "{spec} 应可解析");
        }
    }

    #[test]
    fn 解析符号键与别名() {
        for spec in ["Backslash", "\\", "Space", "Numpad1", "Enter", ";", "Up"] {
            assert!(parse_hotkey(spec).is_ok(), "{spec} 应可解析");
        }
    }

    #[test]
    fn 非法输入应报错而不崩溃() {
        for spec in ["", "   ", "Cmd+", "Cmd+Shift", "A+B", "NotAKey"] {
            assert!(parse_hotkey(spec).is_err(), "「{spec}」应报错");
        }
    }
}

#[cfg(test)]
mod main_thread_tests {
    #![allow(non_snake_case)]
    use super::*;

    #[test]
    fn 没登记时必须返回_false_而不是_true() {
        // 这是安全方向：false → 强制调度到主线程（多一次往返）；
        // true → 可能在 tokio 工作线程上建窗口，热键**静默失效**。
        //
        // 修之前这个函数在非 macOS 上恒返回 true —— 那不是检查，是桩。
        #[cfg(target_os = "windows")]
        {
            // 先把登记清掉（模拟"忘了登记"）
            MAIN_THREAD_ID.store(0, std::sync::atomic::Ordering::Release);
            assert!(
                !is_main_thread(),
                "没登记就返回 true，会让热键建在错误的线程上"
            );
            // 登记之后，当前线程就是主线程
            note_main_thread();
            assert!(is_main_thread(), "登记之后当前线程应当被认作主线程");
        }
    }

    #[test]
    fn 登记函数在不支持的平台上也能编译通过() {
        // 让调用方不必到处写 cfg
        note_main_thread();
    }

    #[test]
    fn 当前平台上_is_main_thread_能安全调用() {
        let _ = is_main_thread();
    }
}
