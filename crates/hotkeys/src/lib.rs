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
#[cfg(not(target_os = "windows"))]
use global_hotkey::GlobalHotKeyManager;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use std::sync::Arc;

#[cfg(any(target_os = "windows", test, feature = "test-support"))]
mod owner;

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

/// Windows：泵当前线程的消息队列。管理器自己的队列由专属 owner 线程持续派发。
#[cfg(target_os = "windows")]
pub fn pump_carbon_events(timeout_secs: f64) -> bool {
    let handled = owner::pump_windows_messages();
    if !handled {
        std::thread::sleep(std::time::Duration::from_secs_f64(
            timeout_secs.clamp(0.001, 0.05),
        ));
    }
    handled
}

/// 其他平台没有 Carbon，短暂等待下一轮退出检查。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn pump_carbon_events(timeout_secs: f64) -> bool {
    std::thread::sleep(std::time::Duration::from_secs_f64(
        timeout_secs.clamp(0.001, 0.05),
    ));
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

/// Windows：查询由 `note_main_thread` 显式登记的应用主线程。
///
/// 这个信息供 GUI 的应用调度使用。热键的原生窗口由专属 owner 线程拥有，
/// 因此 Windows 绑定和重绑不依赖调用方是否在主线程，也不依赖 Tauri 的消息循环。
/// 未登记时返回 false，避免误把普通工作线程当成应用主线程。
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
    events: Arc<EventInbox>,
}

#[derive(Default)]
struct EventInbox {
    #[cfg(test)]
    local: Option<Arc<std::sync::Mutex<std::collections::VecDeque<GlobalHotKeyEvent>>>>,
}
impl EventInbox {
    fn next(&self) -> Option<GlobalHotKeyEvent> {
        #[cfg(test)]
        if let Some(events) = &self.local {
            return events.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
        }
        GlobalHotKeyEvent::receiver().try_recv().ok()
    }
    fn discard_pending(&self) {
        while self.next().is_some() {}
    }
}

impl HotkeyPoller {
    /// 非阻塞取出一个被按下的槽位。没有则返回 None。
    ///
    /// 读的是 global-hotkey 的**全局通道** —— 这正是它可以跨线程用的原因。
    pub fn poll(&self) -> Option<usize> {
        let mut hit = None;
        while let Some(ev) = self.events.next() {
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
///
/// Windows 上只持有可跨线程的消息句柄；原生窗口始终由专属线程拥有。
pub struct Hotkeys {
    manager: Arc<HotkeyManager>,
    id_to_slot: HashMap<u32, usize>,
    bindings: Vec<(String, usize)>,
    keys: Vec<HotKey>,
}

/// 平台管理器的安全句柄。
///
/// macOS 在主线程创建并由应用运行循环派发 Carbon 事件。
/// Windows 在专属线程创建原生管理器，所有注册、注销和析构均发到该线程。
/// 句柄中没有 HWND，也不需要声明不成立的 `unsafe impl Send/Sync`。
pub struct HotkeyManager {
    inner: ManagerOwner,
    events: Arc<EventInbox>,
}

enum ManagerOwner {
    #[cfg(not(target_os = "windows"))]
    Native(GlobalHotKeyManager),
    #[cfg(any(target_os = "windows", test, feature = "test-support"))]
    Worker(owner::OwnerClient),
}

impl HotkeyManager {
    fn register_batch(&self, keys: &[HotKey]) -> Result<()> {
        match &self.inner {
            #[cfg(not(target_os = "windows"))]
            ManagerOwner::Native(manager) => {
                self.events.discard_pending();
                register_transaction(
                    keys,
                    |key| manager.register(key).map_err(Into::into),
                    |key| manager.unregister(key).map_err(Into::into),
                )
            }
            #[cfg(any(target_os = "windows", test, feature = "test-support"))]
            ManagerOwner::Worker(owner) => owner.register(keys),
        }
    }

    fn unregister_all(&self, keys: &[HotKey]) -> Result<()> {
        match &self.inner {
            #[cfg(not(target_os = "windows"))]
            ManagerOwner::Native(manager) => {
                let result =
                    unregister_every_key(keys, |key| manager.unregister(key).map_err(Into::into));
                self.events.discard_pending();
                result
            }
            #[cfg(any(target_os = "windows", test, feature = "test-support"))]
            ManagerOwner::Worker(owner) => owner.unregister(keys),
        }
    }
}

/// 宿主回归使用的不注册系统热键的管理器替身。
///
/// 仅供 `test-support` 特性下的测试构建使用；生产构建不会编译这段代码。
#[cfg(feature = "test-support")]
pub mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecordingManager {
        registrations: Arc<AtomicUsize>,
    }

    impl owner::NativeManager for RecordingManager {
        fn register(&self, _: HotKey) -> Result<()> {
            self.registrations.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn unregister(&self, _: HotKey) -> Result<()> {
            Ok(())
        }

        fn pump(&self) {}
    }

    /// 返回共享管理器与累计注册次数；次数用于断言重建不重复绑定。
    pub fn manager() -> Result<(Arc<HotkeyManager>, Arc<AtomicUsize>)> {
        let events = Arc::new(EventInbox::default());
        let registrations = Arc::new(AtomicUsize::new(0));
        let worker_counts = registrations.clone();
        let owner = owner::OwnerClient::spawn(
            move || {
                Ok(RecordingManager {
                    registrations: worker_counts,
                })
            },
            events.clone(),
        )?;
        Ok((
            Arc::new(HotkeyManager {
                inner: ManagerOwner::Worker(owner),
                events,
            }),
            registrations,
        ))
    }
}

/// 注册任一键失败时立即撤销本次已经成功的注册。
fn register_transaction(
    keys: &[HotKey],
    mut register: impl FnMut(HotKey) -> Result<()>,
    mut unregister: impl FnMut(HotKey) -> Result<()>,
) -> Result<()> {
    for (index, &key) in keys.iter().enumerate() {
        if let Err(error) = register(key) {
            for &registered in &keys[..index] {
                if let Err(rollback_error) = unregister(registered) {
                    log::error!("撤销热键注册失败: {rollback_error:#}");
                }
            }
            return Err(error);
        }
    }
    Ok(())
}

/// 注销错误不能阻止释放同一批次的其他按键。
fn unregister_every_key(
    keys: &[HotKey],
    mut unregister: impl FnMut(HotKey) -> Result<()>,
) -> Result<()> {
    let mut first_error = None;
    for &key in keys {
        if let Err(error) = unregister(key) {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// 跨组解析并检查重复键，错误列出冲突键与所属组。
///
/// 同一个物理键只能绑定一个动作；错误信息带组号，调用方据此指出冲突位置。
pub fn validate_disjoint(groups: &[&[String]]) -> Result<()> {
    let mut seen: HashMap<u32, (String, usize)> = HashMap::new();
    for (group_index, group) in groups.iter().enumerate() {
        for spec in group.iter() {
            let spec = spec.trim();
            if spec.is_empty() {
                continue;
            }
            let key = parse_hotkey(spec)
                .with_context(|| format!("第 {} 组热键「{spec}」无法解析", group_index + 1))?;
            match seen.get(&key.id()) {
                Some((first_spec, first_group)) if *first_group == group_index => {
                    anyhow::bail!(
                        "热键「{spec}」在第 {} 组内重复（已有「{first_spec}」）",
                        group_index + 1
                    );
                }
                Some((first_spec, first_group)) => {
                    anyhow::bail!(
                        "热键「{spec}」同时出现在第 {} 组（「{first_spec}」）与第 {} 组；每个键只能绑定一个动作",
                        first_group + 1,
                        group_index + 1
                    );
                }
                None => {
                    seen.insert(key.id(), (spec.to_owned(), group_index));
                }
            }
        }
    }
    Ok(())
}

fn parse_bindings(keys: &[String]) -> Result<Vec<(usize, HotKey)>> {
    let mut seen = std::collections::HashSet::new();
    keys.iter()
        .enumerate()
        .filter(|(_, spec)| !spec.trim().is_empty())
        .map(|(slot, spec)| {
            let key = parse_hotkey(spec)
                .with_context(|| format!("热键「{spec}」无法解析（第 {} 个）", slot + 1))?;
            if !seen.insert(key.id()) {
                anyhow::bail!("热键「{spec}」重复；每个键只能绑定一个音效");
            }
            Ok((slot, key))
        })
        .collect()
}

impl Hotkeys {
    /// 把 `keys[i]` 绑定到槽位 `i`。
    pub fn bind_to_slots(keys: &[String]) -> Result<Self> {
        // 先完整验证配置，不为无效配置创建平台窗口。
        parse_bindings(keys)?;
        Self::bind_with(Arc::new(Self::new_manager()?), keys)
    }

    /// 创建安全管理器句柄；macOS 必须在主线程调用，Windows 可从任意线程调用。
    pub fn new_manager() -> Result<HotkeyManager> {
        let events = Arc::new(EventInbox::default());
        #[cfg(target_os = "windows")]
        let inner = ManagerOwner::Worker(owner::OwnerClient::new(events.clone())?);
        #[cfg(not(target_os = "windows"))]
        let inner = {
            if !is_main_thread() {
                anyhow::bail!("macOS 全局热键管理器必须在主线程创建，且主线程需要运行应用事件循环");
            }
            ManagerOwner::Native(GlobalHotKeyManager::new().context("初始化全局热键管理器失败")?)
        };
        Ok(HotkeyManager { inner, events })
    }

    /// 用已有的安全管理器句柄绑定键位。
    pub fn bind_with(manager: Arc<HotkeyManager>, keys: &[String]) -> Result<Self> {
        let parsed = parse_bindings(keys)?;
        let raw: Vec<_> = parsed.iter().map(|(_, key)| *key).collect();
        manager
            .register_batch(&raw)
            .context("注册热键失败；可能已被系统或其他程序占用")?;
        Ok(Self {
            manager,
            id_to_slot: parsed.iter().map(|(slot, key)| (key.id(), *slot)).collect(),
            bindings: parsed
                .iter()
                .map(|(slot, _)| (keys[*slot].clone(), *slot))
                .collect(),
            keys: raw,
        })
    }

    /// 注销全部热键，把按键还给游戏；保留显示用的配置供重新激活。
    pub fn unbind_all(&mut self) -> Result<()> {
        self.manager
            .unregister_all(&self.keys)
            .context("注销热键失败")?;
        self.keys.clear();
        self.id_to_slot.clear();
        Ok(())
    }

    pub fn is_bound(&self) -> bool {
        !self.keys.is_empty()
    }

    /// 使用同一平台管理器重新绑定：新键全部注册成功后才释放旧键。
    ///
    /// 系统级冲突时旧绑定保持不变，调用方可以把错误直接展示给用户。
    /// 新配置里仍保留的键不重复注册，避免把自己的旧注册当成冲突。
    pub fn rebind(&mut self, keys: &[String]) -> Result<()> {
        let parsed = parse_bindings(keys)?;
        let raw: Vec<_> = parsed.iter().map(|(_, key)| *key).collect();
        let old_ids: std::collections::HashSet<u32> = self.keys.iter().map(HotKey::id).collect();
        let new_ids: std::collections::HashSet<u32> = raw.iter().map(HotKey::id).collect();
        let to_register: Vec<HotKey> = raw
            .iter()
            .copied()
            .filter(|key| !old_ids.contains(&key.id()))
            .collect();
        if let Err(error) = self.manager.register_batch(&to_register) {
            return Err(error.context("重新注册热键失败，原绑定保持不变"));
        }
        let to_unregister: Vec<HotKey> = self
            .keys
            .iter()
            .copied()
            .filter(|key| !new_ids.contains(&key.id()))
            .collect();
        if let Err(error) = self.manager.unregister_all(&to_unregister) {
            if let Err(rollback_error) = self.manager.unregister_all(&to_register) {
                log::error!("撤销新热键失败: {rollback_error:#}");
            }
            return Err(error.context("释放旧热键失败，已保留原绑定"));
        }
        self.id_to_slot = parsed.iter().map(|(slot, key)| (key.id(), *slot)).collect();
        self.bindings = parsed
            .iter()
            .map(|(slot, _)| (keys[*slot].clone(), *slot))
            .collect();
        self.keys = raw;
        Ok(())
    }

    pub fn poller(&self) -> HotkeyPoller {
        HotkeyPoller {
            id_to_slot: self.id_to_slot.clone(),
            events: self.manager.events.clone(),
        }
    }

    pub fn poll(&self) -> Option<usize> {
        self.poller().poll()
    }

    pub fn len(&self) -> usize {
        self.id_to_slot.len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_to_slot.is_empty()
    }

    pub fn bindings(&self) -> &[(String, usize)] {
        &self.bindings
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        if let Err(error) = self.manager.unregister_all(&self.keys) {
            log::error!("释放热键失败: {error:#}");
        }
    }
}

/// 事件来自哪一组热键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyGroup {
    Slots,
    Voice,
}

/// 音效槽位与人声效果共用一个平台管理器，但分属两组独立绑定。
///
/// 槽位集合随音频会话启停：待机只归还槽位按键，会话释放时丢弃该集合。
/// 变声集合从程序启动保留到进程退出，因此无音频会话时仍能切换待用预设。
/// 两个轮询器必须由同一个消费者排水：global-hotkey 的事件通道是进程共享的，
/// 分别调用 [`HotkeyPoller::poll`] 会让先调用者吞掉另一组的按键。
#[derive(Default)]
pub struct HotkeyGroups {
    slot_hotkeys: Option<Hotkeys>,
    slot_poller: Option<HotkeyPoller>,
    voice_hotkeys: Option<Hotkeys>,
    voice_poller: Option<HotkeyPoller>,
}

impl HotkeyGroups {
    pub fn new(slot_hotkeys: Option<Hotkeys>, voice_hotkeys: Option<Hotkeys>) -> Self {
        let slot_poller = slot_hotkeys.as_ref().map(Hotkeys::poller);
        let voice_poller = voice_hotkeys.as_ref().map(Hotkeys::poller);
        Self {
            slot_hotkeys,
            slot_poller,
            voice_hotkeys,
            voice_poller,
        }
    }

    pub fn slot_hotkeys(&self) -> Option<&Hotkeys> {
        self.slot_hotkeys.as_ref()
    }

    pub fn voice_hotkeys(&self) -> Option<&Hotkeys> {
        self.voice_hotkeys.as_ref()
    }

    pub fn set_slot_hotkeys(&mut self, hotkeys: Option<Hotkeys>) {
        self.slot_poller = hotkeys.as_ref().map(Hotkeys::poller);
        self.slot_hotkeys = hotkeys;
    }

    pub fn set_voice_hotkeys(&mut self, hotkeys: Option<Hotkeys>) {
        self.voice_poller = hotkeys.as_ref().map(Hotkeys::poller);
        self.voice_hotkeys = hotkeys;
    }

    pub fn slots_bound(&self) -> bool {
        self.slot_hotkeys.as_ref().is_some_and(Hotkeys::is_bound)
    }

    pub fn voice_bound(&self) -> bool {
        self.voice_hotkeys.as_ref().is_some_and(Hotkeys::is_bound)
    }

    /// 是否持有任一热键集合；空集合也代表平台管理器已介入。
    pub fn has_sets(&self) -> bool {
        self.slot_hotkeys.is_some() || self.voice_hotkeys.is_some()
    }

    /// 待机只注销槽位热键；人声效果热键保持注册。
    pub fn standby(&mut self) -> Result<()> {
        self.slot_poller = None;
        match self.slot_hotkeys.as_mut() {
            Some(hotkeys) => hotkeys.unbind_all(),
            None => Ok(()),
        }
    }

    /// 音频会话结束：注销并丢弃槽位集合；变声集合跨会话保留。
    ///
    /// 集合在这里丢弃，确保平台句柄的析构发生在调用方调度的线程上。
    pub fn end_session(&mut self) -> Result<()> {
        self.slot_poller = None;
        let mut slots = self.slot_hotkeys.take();
        let result = match slots.as_mut() {
            Some(hotkeys) => hotkeys.unbind_all(),
            None => Ok(()),
        };
        drop(slots);
        result
    }

    /// 注销并丢弃变声集合；只在进程退出或显式关闭时调用。
    pub fn clear_voice(&mut self) -> Result<()> {
        self.voice_poller = None;
        let mut voice = self.voice_hotkeys.take();
        let result = match voice.as_mut() {
            Some(hotkeys) => hotkeys.unbind_all(),
            None => Ok(()),
        };
        drop(voice);
        result
    }

    /// 进程退出：两组都注销并丢弃。
    pub fn shutdown(&mut self) -> Result<()> {
        let slots = self.end_session();
        let voice = self.clear_voice();
        slots.and(voice)
    }

    pub fn rebind_slots(&mut self, keys: &[String]) -> Result<()> {
        let hotkeys = self
            .slot_hotkeys
            .as_mut()
            .context("音效槽位热键集合不存在")?;
        hotkeys.rebind(keys)?;
        self.slot_poller = Some(hotkeys.poller());
        Ok(())
    }

    pub fn rebind_voice(&mut self, keys: &[String]) -> Result<()> {
        let hotkeys = self
            .voice_hotkeys
            .as_mut()
            .context("人声效果热键集合不存在")?;
        hotkeys.rebind(keys)?;
        self.voice_poller = Some(hotkeys.poller());
        Ok(())
    }

    /// 取走本次轮询到的按键，按所属组区分。
    ///
    /// 同一个键同时出现在两组属于配置错误；路由时槽位优先，避免一次按键
    /// 触发两个动作。调用方在绑定前用 [`validate_disjoint`] 拒绝这种配置。
    pub fn poll_actions(&self) -> Vec<(HotkeyGroup, usize)> {
        let events = match (&self.slot_poller, &self.voice_poller) {
            (Some(poller), _) => &poller.events,
            (None, Some(poller)) => &poller.events,
            (None, None) => return Vec::new(),
        };
        let mut hits = Vec::new();
        while let Some(event) = events.next() {
            if event.state != HotKeyState::Pressed {
                continue;
            }
            if let Some(slot) = self
                .slot_poller
                .as_ref()
                .and_then(|poller| poller.id_to_slot.get(&event.id()))
            {
                hits.push((HotkeyGroup::Slots, *slot));
            } else if let Some(slot) = self
                .voice_poller
                .as_ref()
                .and_then(|poller| poller.id_to_slot.get(&event.id()))
            {
                hits.push((HotkeyGroup::Voice, *slot));
            }
        }
        hits
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

    static LOOP_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn 事件循环必须能被外部打断() {
        let _guard = LOOP_TEST_LOCK.lock().unwrap();
        // 回归测试：早期用 Carbon 原生的 RunApplicationEventLoop，
        // QuitApplicationEventLoop 在纯命令行进程里打不断它 —— 实测挂死。
        //
        // 这里用**假泵**测控制逻辑，不碰真实 Carbon：
        // 真实 ReceiveNextEvent 不是线程安全的，并行测试会偶发 SIGSEGV。
        // 真实泵由 examples/app_loop_test.rs 单线程验证。
        reset_app_event_loop();
        let worker = std::thread::spawn(|| {
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
        worker.join().unwrap();
    }

    #[test]
    fn 退出标志必须在起线程前重置才不丢信号() {
        let _guard = LOOP_TEST_LOCK.lock().unwrap();
        // 如果工作线程在进入循环前就决定退出，而循环入口又重置了标志，
        // 主线程就会永久阻塞。这里锁住"重置由调用方负责"这个约定。
        reset_app_event_loop();
        quit_app_event_loop(); // 工作线程在进入循环前提前置位
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

    #[test]
    fn 跨组重复热键被拒绝并列出冲突() {
        let slots = vec!["F1".to_owned(), "F9".to_owned()];
        let voice = vec!["F9".to_owned(), String::new(), "F10".to_owned()];
        let error = validate_disjoint(&[&slots, &voice])
            .unwrap_err()
            .to_string();
        assert!(error.contains("F9"), "错误必须点名冲突键：{error}");
        assert!(
            error.contains("第 1 组"),
            "错误必须指出第一个所属组：{error}"
        );
        assert!(
            error.contains("第 2 组"),
            "错误必须指出第二个所属组：{error}"
        );
    }

    #[test]
    fn 无关热键与空位允许跨组共存() {
        let slots = vec!["F1".to_owned(), String::new(), " ".to_owned()];
        let voice = vec!["F2".to_owned(), "F3".to_owned(), "F4".to_owned()];
        validate_disjoint(&[&slots, &voice]).unwrap();
        let empty: &[String] = &[];
        validate_disjoint(&[empty, empty]).unwrap();
    }

    #[test]
    fn 组内重复热键同样被拒绝() {
        let voice = vec!["F2".to_owned(), "f2".to_owned(), String::new()];
        let error = validate_disjoint(&[&[], &voice]).unwrap_err().to_string();
        assert!(error.contains("F2"), "错误必须点名重复键：{error}");
        assert!(error.contains("第 2 组"), "错误必须指出所属组：{error}");
    }

    #[test]
    fn 跨组校验报告无法解析的键和组() {
        let slots = vec!["NotAKey".to_owned()];
        let voice = vec!["F2".to_owned()];
        let error = validate_disjoint(&[&slots, &voice])
            .unwrap_err()
            .to_string();
        assert!(error.contains("NotAKey"), "错误必须点名非法键：{error}");
        assert!(error.contains("第 1 组"), "错误必须指出所属组：{error}");
    }
}

#[cfg(test)]
mod main_thread_tests {
    #![allow(non_snake_case)]
    use super::*;

    static MAIN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn 没登记时必须返回_false_而不是_true() {
        let _guard = MAIN_TEST_LOCK.lock().unwrap();
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
        let _guard = MAIN_TEST_LOCK.lock().unwrap();
        // 让调用方不必到处写 cfg
        note_main_thread();
    }

    #[test]
    fn 当前平台上_is_main_thread_能安全调用() {
        let _guard = MAIN_TEST_LOCK.lock().unwrap();
        let _ = is_main_thread();
    }
}
