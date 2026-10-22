//! Windows 原生管理器的线程所有权边界。
//!
//! 只有消息和 JoinHandle 能离开线程；GlobalHotKeyManager 从未跨线程传递。

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{mpsc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use global_hotkey::hotkey::HotKey;

use crate::{register_transaction, unregister_every_key, EventInbox};

pub(super) trait NativeManager {
    fn register(&self, key: HotKey) -> Result<()>;
    fn unregister(&self, key: HotKey) -> Result<()>;
    fn pump(&self);
    fn flush_pending(&self) {
        self.pump();
    }
    #[cfg(all(target_os = "windows", test))]
    fn post_test_hotkey(&self, _id: u32) -> Result<()> {
        anyhow::bail!("不是 Windows 原生管理器")
    }
}

enum Command {
    Register(Vec<HotKey>, mpsc::Sender<Result<()>>),
    Unregister(Vec<HotKey>, mpsc::Sender<Result<()>>),
    Shutdown,
    #[cfg(all(target_os = "windows", test))]
    PostTestHotkey(u32, mpsc::Sender<Result<()>>),
}

pub(super) struct OwnerClient {
    tx: mpsc::Sender<Command>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl OwnerClient {
    #[cfg(target_os = "windows")]
    pub(super) fn new(events: std::sync::Arc<EventInbox>) -> Result<Self> {
        Self::spawn(
            || {
                global_hotkey::GlobalHotKeyManager::new()
                    .map(WindowsManager)
                    .context("初始化 Windows 全局热键管理器失败")
            },
            events,
        )
    }

    // M 不要求 Send：factory 在 owner 线程内部构造它，此后不会移出该线程。
    pub(super) fn spawn<M: NativeManager + 'static>(
        factory: impl FnOnce() -> Result<M> + Send + 'static,
        events: std::sync::Arc<EventInbox>,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("hotkey-owner".into())
            .spawn(move || {
                let native = match factory() {
                    Ok(native) => native,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let registered = RefCell::new(HashMap::<u32, HotKey>::new());
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                loop {
                    // 每次处理命令前派发队列，防止持续重绑导致 WM_HOTKEY 饥饿。
                    native.pump();
                    match rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(Command::Register(keys, reply)) => {
                            native.flush_pending();
                            events.discard_pending();
                            let result = register_transaction(
                                &keys,
                                |key| {
                                    native.register(key)?;
                                    registered.borrow_mut().insert(key.id(), key);
                                    Ok(())
                                },
                                |key| {
                                    native.unregister(key)?;
                                    registered.borrow_mut().remove(&key.id());
                                    Ok(())
                                },
                            );
                            let _ = reply.send(result);
                        }
                        Ok(Command::Unregister(keys, reply)) => {
                            let result = unregister_every_key(&keys, |key| {
                                native.unregister(key)?;
                                registered.borrow_mut().remove(&key.id());
                                Ok(())
                            });
                            // Flush old native WM_HOTKEY messages before acknowledging standby.
                            native.flush_pending();
                            events.discard_pending();
                            let _ = reply.send(result);
                        }
                        #[cfg(all(target_os = "windows", test))]
                        Ok(Command::PostTestHotkey(id, reply)) => {
                            let _ = reply.send(native.post_test_hotkey(id));
                        }
                        Ok(Command::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                // 即使调用方忘记 unbind，或某次注销失败，退出仍尝试释放全部注册。
                let remaining: Vec<_> = registered.borrow().values().copied().collect();
                if let Err(error) = unregister_every_key(&remaining, |key| native.unregister(key)) {
                    log::error!("关闭 Windows 热键线程时注销失败: {error:#}");
                }
                native.flush_pending();
                events.discard_pending();
                // native 在这里析构：DestroyWindow 与创建/注册发生于同一线程。
            })
            .context("启动 Windows 热键线程失败")?;
        match ready_rx.recv().context("Windows 热键线程提前退出")? {
            Ok(()) => Ok(Self {
                tx,
                thread: Mutex::new(Some(thread)),
            }),
            Err(error) => {
                let _ = thread.join();
                Err(error)
            }
        }
    }

    pub(super) fn register(&self, keys: &[HotKey]) -> Result<()> {
        self.request(|reply| Command::Register(keys.to_vec(), reply))
    }

    pub(super) fn unregister(&self, keys: &[HotKey]) -> Result<()> {
        self.request(|reply| Command::Unregister(keys.to_vec(), reply))
    }

    #[cfg(all(target_os = "windows", test))]
    fn post_test_hotkey(&self, id: u32) -> Result<()> {
        self.request(|reply| Command::PostTestHotkey(id, reply))
    }

    fn request(&self, command: impl FnOnce(mpsc::Sender<Result<()>>) -> Command) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(command(tx))
            .context("Windows 热键线程已停止")?;
        rx.recv().context("Windows 热键线程未返回操作结果")?
    }
}

impl Drop for OwnerClient {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(thread) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            if thread.join().is_err() {
                log::error!("Windows 热键 owner 线程异常退出");
            }
        }
    }
}

#[cfg(target_os = "windows")]
struct WindowsManager(global_hotkey::GlobalHotKeyManager);

#[cfg(target_os = "windows")]
impl NativeManager for WindowsManager {
    fn register(&self, key: HotKey) -> Result<()> {
        self.0.register(key).map_err(Into::into)
    }

    fn unregister(&self, key: HotKey) -> Result<()> {
        self.0.unregister(key).map_err(Into::into)
    }

    fn pump(&self) {
        pump_windows_messages();
    }
    fn flush_pending(&self) {
        while pump_windows_messages() {}
    }

    #[cfg(test)]
    fn post_test_hotkey(&self, id: u32) -> Result<()> {
        use windows_sys::Win32::Foundation::{HWND, LPARAM};
        use windows_sys::Win32::System::Threading::GetCurrentThreadId;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            EnumThreadWindows, PostMessageW, WM_HOTKEY,
        };

        struct Injection {
            id: u32,
            posted: bool,
        }
        unsafe extern "system" fn post(hwnd: HWND, parameter: LPARAM) -> i32 {
            // SAFETY: EnumThreadWindows 同步调用，指针指向尚存活的栈上 Injection。
            let injection = unsafe { &mut *(parameter as *mut Injection) };
            // F23 的 VK 为 0x86；低字是 Ctrl+Alt+Shift 的 modifier flags。
            // 仅向本 owner 的隐藏窗口投递消息，不生成系统键盘输入。
            injection.posted =
                unsafe { PostMessageW(hwnd, WM_HOTKEY, injection.id as usize, (0x86 << 16) | 7) }
                    != 0;
            0
        }
        let mut injection = Injection { id, posted: false };
        unsafe {
            EnumThreadWindows(
                GetCurrentThreadId(),
                Some(post),
                &mut injection as *mut Injection as LPARAM,
            );
        }
        anyhow::ensure!(injection.posted, "向热键隐藏窗口投递测试事件失败");
        Ok(())
    }
}

/// 只访问当前线程的消息队列，每轮最多 64 条，确保控制命令能及时处理。
#[cfg(target_os = "windows")]
pub(super) fn pump_windows_messages() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    let mut message: MSG = unsafe { std::mem::zeroed() };
    let mut handled = false;
    for _ in 0..64 {
        // SAFETY: message 是有效 MSG 存储，null HWND 表示当前线程全部窗口。
        // 该函数由消息队列及其窗口的 owner 线程调用。
        if unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } == 0 {
            break;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        handled = true;
    }
    handled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HotkeyGroup, HotkeyGroups, HotkeyManager, Hotkeys, ManagerOwner};
    use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    type Calls = Arc<Mutex<Vec<(&'static str, std::thread::ThreadId, u32)>>>;

    struct FakeManager {
        // Rc 刻意让原生对象 !Send，证明只有 factory 而非实例被移到 owner 线程。
        _thread_local: Rc<()>,
        calls: Calls,
        pumps: Arc<AtomicUsize>,
        rejected: u32,
    }

    impl NativeManager for FakeManager {
        fn register(&self, key: HotKey) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(("register", std::thread::current().id(), key.id()));
            if key.id() == self.rejected {
                anyhow::bail!("模拟热键冲突");
            }
            Ok(())
        }

        fn unregister(&self, key: HotKey) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(("unregister", std::thread::current().id(), key.id()));
            Ok(())
        }

        fn pump(&self) {
            self.pumps.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Drop for FakeManager {
        fn drop(&mut self) {
            self.calls
                .lock()
                .unwrap()
                .push(("drop", std::thread::current().id(), 0));
        }
    }

    type TestEvents = Arc<Mutex<VecDeque<GlobalHotKeyEvent>>>;

    fn fake_with_events(
        rejected: u32,
    ) -> (Arc<HotkeyManager>, Calls, Arc<AtomicUsize>, TestEvents) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let pumps = Arc::new(AtomicUsize::new(0));
        let queue: TestEvents = Arc::new(Mutex::new(VecDeque::new()));
        let events = Arc::new(EventInbox {
            local: Some(queue.clone()),
        });
        let worker_calls = calls.clone();
        let worker_pumps = pumps.clone();
        let owner = OwnerClient::spawn(
            move || {
                worker_calls
                    .lock()
                    .unwrap()
                    .push(("create", std::thread::current().id(), 0));
                Ok(FakeManager {
                    _thread_local: Rc::new(()),
                    calls: worker_calls,
                    pumps: worker_pumps,
                    rejected,
                })
            },
            events.clone(),
        )
        .unwrap();
        (
            Arc::new(HotkeyManager {
                inner: ManagerOwner::Worker(owner),
                events,
            }),
            calls,
            pumps,
            queue,
        )
    }

    fn fake(rejected: u32) -> (Arc<HotkeyManager>, Calls, Arc<AtomicUsize>) {
        let (manager, calls, pumps, _) = fake_with_events(rejected);
        (manager, calls, pumps)
    }

    fn pressed(id: u32) -> GlobalHotKeyEvent {
        GlobalHotKeyEvent {
            id,
            state: HotKeyState::Pressed,
        }
    }

    #[test]
    fn 所有原生操作和销毁均在同一线程且空闲时持续泵事件() {
        let caller = std::thread::current().id();
        let (manager, calls, pumps) = fake(0);
        let mut keys = Hotkeys::bind_with(manager.clone(), &["F1".into()]).unwrap();
        std::thread::sleep(Duration::from_millis(35));
        keys.unbind_all().unwrap();
        keys.rebind(&["F2".into()]).unwrap();
        drop(manager);
        std::thread::spawn(move || drop(keys)).join().unwrap();
        let calls = calls.lock().unwrap();
        let owner = calls[0].1;
        assert_ne!(owner, caller);
        assert!(calls.iter().all(|call| call.1 == owner));
        assert_eq!(calls.last().unwrap().0, "drop");
        assert!(calls.iter().filter(|call| call.0 == "unregister").count() >= 2);
        assert!(pumps.load(Ordering::Relaxed) >= 3);
    }

    #[test]
    fn 部分注册失败时撤销已经占用的按键() {
        let f1 = crate::parse_hotkey("F1").unwrap().id();
        let f2 = crate::parse_hotkey("F2").unwrap().id();
        let (manager, calls, _) = fake(f2);
        assert!(Hotkeys::bind_with(manager.clone(), &["F1".into(), "F2".into()]).is_err());
        assert!(calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.0 == "unregister" && call.2 == f1));
        // 注册失败后同一个管理器仍可正常使用，没有遗留半套绑定。
        let keys = Hotkeys::bind_with(manager, &["F1".into()]).unwrap();
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn 重绑冲突恢复原绑定且重复键在操作前被拒绝() {
        let f2 = crate::parse_hotkey("F2").unwrap().id();
        let (manager, calls, _) = fake(f2);
        let mut keys = Hotkeys::bind_with(manager, &["F1".into()]).unwrap();
        assert!(keys.rebind(&["F2".into()]).is_err());
        assert_eq!(keys.bindings(), &[("F1".into(), 0)]);
        assert!(keys.is_bound());
        let before = calls.lock().unwrap().len();
        assert!(keys.rebind(&["F1".into(), "f1".into()]).is_err());
        assert_eq!(calls.lock().unwrap().len(), before);
    }

    #[test]
    fn owner退出主动释放调用方未显式注销的按键() {
        let (manager, calls, _) = fake(0);
        let key = crate::parse_hotkey("F1").unwrap();
        manager.register_batch(&[key]).unwrap();
        drop(manager);
        let calls = calls.lock().unwrap();
        assert_eq!(calls[calls.len() - 2].0, "unregister");
        assert_eq!(calls.last().unwrap().0, "drop");
    }

    #[test]
    fn 空键允许未绑定音效且不压缩槽位索引() {
        let (manager, _, _) = fake(0);
        let mut keys = Hotkeys::bind_with(manager, &["".into(), "F2".into()]).unwrap();
        let f2 = crate::parse_hotkey("F2").unwrap().id();
        assert_eq!(keys.poller().id_to_slot.get(&f2), Some(&1));
        assert_eq!(keys.bindings(), &[("F2".into(), 1)]);
        keys.rebind(&["F1".into(), " ".into(), "F3".into()])
            .unwrap();
        let f3 = crate::parse_hotkey("F3").unwrap().id();
        assert_eq!(keys.poller().id_to_slot.get(&f3), Some(&2));
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn 待机解绑先派发旧原生消息再丢弃事件而新绑定后的按键仍可派发() {
        use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
        use std::collections::VecDeque;
        struct QueuedManager {
            native: Arc<Mutex<VecDeque<GlobalHotKeyEvent>>>,
            inbox: Arc<Mutex<VecDeque<GlobalHotKeyEvent>>>,
        }
        impl NativeManager for QueuedManager {
            fn register(&self, _: HotKey) -> Result<()> {
                Ok(())
            }
            fn unregister(&self, key: HotKey) -> Result<()> {
                // Simulate an old WM_HOTKEY still queued at the moment unregister returns.
                self.native.lock().unwrap().push_back(GlobalHotKeyEvent {
                    id: key.id(),
                    state: HotKeyState::Pressed,
                });
                Ok(())
            }
            fn pump(&self) {
                let mut native = self.native.lock().unwrap();
                self.inbox.lock().unwrap().extend(native.drain(..));
            }
        }
        let id = crate::parse_hotkey("F1").unwrap().id();
        let old = GlobalHotKeyEvent {
            id,
            state: HotKeyState::Pressed,
        };
        let inbox = Arc::new(Mutex::new(VecDeque::from([old])));
        let native = Arc::new(Mutex::new(VecDeque::new()));
        let events = Arc::new(EventInbox {
            local: Some(inbox.clone()),
        });
        let factory_inbox = inbox.clone();
        let factory_native = native.clone();
        let owner = OwnerClient::spawn(
            move || {
                Ok(QueuedManager {
                    native: factory_native,
                    inbox: factory_inbox,
                })
            },
            events.clone(),
        )
        .unwrap();
        let manager = Arc::new(HotkeyManager {
            inner: ManagerOwner::Worker(owner),
            events,
        });
        let mut keys = Hotkeys::bind_with(manager, &["F1".into()]).unwrap();
        assert_eq!(
            keys.poll(),
            None,
            "bind must discard events buffered before registration"
        );
        inbox.lock().unwrap().push_back(old);
        assert_eq!(
            keys.poll(),
            Some(0),
            "new registered events must still work"
        );
        keys.unbind_all().unwrap();
        // These assertions occur after the unbind ACK: native and delivered stale queues
        // must both be empty, including a message inserted by unregister itself.
        assert!(native.lock().unwrap().is_empty());
        assert!(inbox.lock().unwrap().is_empty());
        keys.rebind(&["F1".into()]).unwrap();
        assert_eq!(
            keys.poll(),
            None,
            "old press must not cross standby/reactivation"
        );
        inbox.lock().unwrap().push_back(old);
        assert_eq!(keys.poll(), Some(0));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows原生消息泵能派发热键且退出后可重新注册() {
        // 不发模拟键盘输入；直接投递 WM_HOTKEY 验证隐藏窗口的真实消息派发。
        let specs = vec![String::new(), "Ctrl+Alt+Shift+F23".into()];
        let mut keys = std::thread::spawn(move || Hotkeys::bind_to_slots(&specs))
            .join()
            .unwrap()
            .unwrap();
        let id = crate::parse_hotkey("Ctrl+Alt+Shift+F23").unwrap().id();
        let ManagerOwner::Worker(owner) = &keys.manager.inner;
        owner.post_test_hotkey(id).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut hit = None;
        while hit.is_none() && std::time::Instant::now() < deadline {
            hit = keys.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(hit, Some(1));
        let ManagerOwner::Worker(owner) = &keys.manager.inner;
        owner.post_test_hotkey(id).unwrap(); // leave a stale press pending
        keys.unbind_all().unwrap();
        keys.rebind(&[String::new(), "Ctrl+Alt+Shift+F23".into()])
            .unwrap();
        assert_eq!(keys.poll(), None);
        let ManagerOwner::Worker(owner) = &keys.manager.inner;
        owner.post_test_hotkey(id).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut fresh = None;
        while fresh.is_none() && std::time::Instant::now() < deadline {
            fresh = keys.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(fresh, Some(1));
        std::thread::spawn(move || drop(keys)).join().unwrap();
        let rebound = Hotkeys::bind_to_slots(&["Ctrl+Alt+Shift+F23".into()]).unwrap();
        assert_eq!(rebound.len(), 1);
    }

    #[test]
    fn 变声热键在待机时保持注册() {
        let (manager, _, _, _) = fake_with_events(0);
        let slots = Hotkeys::bind_with(manager.clone(), &["F1".into()]).unwrap();
        let voice = Hotkeys::bind_with(manager, &["F9".into(), "F10".into(), "F8".into()]).unwrap();
        let mut groups = HotkeyGroups::new(Some(slots), Some(voice));

        groups.standby().unwrap();

        assert!(!groups.slots_bound(), "待机必须让出音效槽位按键");
        assert!(
            groups.voice_hotkeys().is_some_and(Hotkeys::is_bound),
            "待机时人声效果热键必须保持注册"
        );
    }

    #[test]
    fn 释放音频会话保留变声热键并注销槽位() {
        let (manager, _, _, events) = fake_with_events(0);
        let slots = Hotkeys::bind_with(manager.clone(), &["F1".into()]).unwrap();
        let voice = Hotkeys::bind_with(manager, &["F9".into()]).unwrap();
        let mut groups = HotkeyGroups::new(Some(slots), Some(voice));

        groups.end_session().unwrap();

        assert!(!groups.slots_bound(), "会话释放必须让出槽位按键");
        assert!(
            groups.voice_hotkeys().is_some_and(Hotkeys::is_bound),
            "会话释放后变声热键必须继续注册"
        );
        events
            .lock()
            .unwrap()
            .push_back(pressed(crate::parse_hotkey("F9").unwrap().id()));
        assert_eq!(
            groups.poll_actions(),
            vec![(HotkeyGroup::Voice, 0)],
            "无会话时变声按键仍必须被消费"
        );
        events
            .lock()
            .unwrap()
            .push_back(pressed(crate::parse_hotkey("F1").unwrap().id()));
        assert!(
            groups.poll_actions().is_empty(),
            "释放后旧槽位按键不能再被消费"
        );
    }

    #[test]
    fn 进程退出注销全部变声热键() {
        let (manager, _, _, events) = fake_with_events(0);
        let slots = Hotkeys::bind_with(manager.clone(), &["F1".into()]).unwrap();
        let voice = Hotkeys::bind_with(manager, &["F9".into()]).unwrap();
        let mut groups = HotkeyGroups::new(Some(slots), Some(voice));

        groups.shutdown().unwrap();

        assert!(!groups.slots_bound());
        assert!(
            groups.voice_hotkeys().is_none(),
            "进程退出必须注销并丢弃变声热键"
        );
        events
            .lock()
            .unwrap()
            .push_back(pressed(crate::parse_hotkey("F9").unwrap().id()));
        assert!(groups.poll_actions().is_empty(), "退出后不再消费事件");
    }

    #[test]
    fn 两组热键共享事件通道时按组路由() {
        let (manager, _, _, events) = fake_with_events(0);
        let slots = Hotkeys::bind_with(manager.clone(), &["F1".into()]).unwrap();
        let voice = Hotkeys::bind_with(manager, &["F9".into()]).unwrap();
        let groups = HotkeyGroups::new(Some(slots), Some(voice));

        events
            .lock()
            .unwrap()
            .push_back(pressed(crate::parse_hotkey("F1").unwrap().id()));
        events
            .lock()
            .unwrap()
            .push_back(pressed(crate::parse_hotkey("F9").unwrap().id()));

        assert_eq!(
            groups.poll_actions(),
            vec![(HotkeyGroup::Slots, 0), (HotkeyGroup::Voice, 0)],
            "同一消费者排水后必须把按键送回各自集合"
        );
    }

    #[test]
    fn 重绑失败保留旧键() {
        let f1 = crate::parse_hotkey("F1").unwrap().id();
        let f2 = crate::parse_hotkey("F2").unwrap().id();
        let (manager, _, _, events) = fake_with_events(f2);
        let mut keys = Hotkeys::bind_with(manager, &["F1".into()]).unwrap();

        assert!(keys.rebind(&["F2".into()]).is_err());
        assert_eq!(keys.bindings(), &[("F1".into(), 0)], "失败必须保留旧绑定");

        events.lock().unwrap().push_back(pressed(f1));
        assert_eq!(keys.poll(), Some(0), "冲突重绑后旧键仍必须可用");
    }
}
