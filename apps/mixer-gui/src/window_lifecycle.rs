//! 主窗口关闭与恢复的共用策略，不持有或改变音频会话。

#[derive(Clone, Copy)]
pub enum MainWindowEvent {
    CloseRequested,
    ShowRequested,
    #[cfg(any(target_os = "macos", test))]
    Reopen,
}

pub trait MainWindowActions {
    fn hide(&self) -> tauri::Result<()>;
    fn show(&self) -> tauri::Result<()>;
    fn unminimize(&self) -> tauri::Result<()>;
    fn focus(&self) -> tauri::Result<()>;
}

impl<R: tauri::Runtime> MainWindowActions for tauri::WebviewWindow<R> {
    fn hide(&self) -> tauri::Result<()> {
        self.hide()
    }

    fn show(&self) -> tauri::Result<()> {
        self.show()
    }

    fn unminimize(&self) -> tauri::Result<()> {
        self.unminimize()
    }

    fn focus(&self) -> tauri::Result<()> {
        self.set_focus()
    }
}

pub fn handle_main_window_event<W: MainWindowActions>(event: MainWindowEvent, window: Option<&W>) {
    let Some(window) = window else {
        return;
    };
    match event {
        MainWindowEvent::CloseRequested => {
            let _ = window.hide();
        }
        MainWindowEvent::ShowRequested => restore_window(window),
        #[cfg(any(target_os = "macos", test))]
        MainWindowEvent::Reopen => restore_window(window),
    }
}

fn restore_window(window: &impl MainWindowActions) {
    // 显示、取消最小化、聚焦都需要尝试；单个系统调用失败不能阻断后续恢复。
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.focus();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Debug, PartialEq, Eq)]
    struct WindowState {
        visible: bool,
        minimized: bool,
        focused: bool,
        focus_attempts: u32,
    }

    struct FakeWindow {
        state: RefCell<WindowState>,
        reject_show: bool,
    }

    impl FakeWindow {
        fn new(minimized: bool) -> Self {
            Self {
                state: RefCell::new(WindowState {
                    visible: true,
                    minimized,
                    focused: !minimized,
                    focus_attempts: 0,
                }),
                reject_show: false,
            }
        }
    }

    impl MainWindowActions for FakeWindow {
        fn hide(&self) -> tauri::Result<()> {
            let mut state = self.state.borrow_mut();
            state.visible = false;
            state.focused = false;
            Ok(())
        }

        fn show(&self) -> tauri::Result<()> {
            if self.reject_show {
                return Err(tauri::Error::WindowNotFound);
            }
            self.state.borrow_mut().visible = true;
            Ok(())
        }

        fn unminimize(&self) -> tauri::Result<()> {
            self.state.borrow_mut().minimized = false;
            Ok(())
        }

        fn focus(&self) -> tauri::Result<()> {
            let mut state = self.state.borrow_mut();
            state.focus_attempts += 1;
            state.focused = state.visible && !state.minimized;
            Ok(())
        }
    }

    #[test]
    fn 关闭隐藏后点击程序图标必须恢复同一窗口并聚焦() {
        let window = FakeWindow::new(false);
        handle_main_window_event(MainWindowEvent::CloseRequested, Some(&window));
        assert!(!window.state.borrow().visible);
        handle_main_window_event(MainWindowEvent::Reopen, Some(&window));
        assert_eq!(
            *window.state.borrow(),
            WindowState {
                visible: true,
                minimized: false,
                focused: true,
                focus_attempts: 1,
            }
        );
    }

    #[test]
    fn 恢复入口都能显示隐藏且最小化的窗口() {
        for event in [MainWindowEvent::ShowRequested, MainWindowEvent::Reopen] {
            let window = FakeWindow::new(true);
            handle_main_window_event(MainWindowEvent::CloseRequested, Some(&window));
            handle_main_window_event(event, Some(&window));
            let state = window.state.borrow();
            assert!(state.visible && state.focused && !state.minimized);
        }
    }

    #[test]
    fn 窗口已经显示时再点击程序图标仍会聚焦() {
        let window = FakeWindow::new(false);
        window.state.borrow_mut().focused = false;
        handle_main_window_event(MainWindowEvent::Reopen, Some(&window));
        handle_main_window_event(MainWindowEvent::Reopen, Some(&window));
        let state = window.state.borrow();
        assert!(state.visible && state.focused && !state.minimized);
        assert_eq!(state.focus_attempts, 2);
    }

    #[test]
    fn 某个恢复动作失败也要继续尝试取消最小化和聚焦() {
        let mut window = FakeWindow::new(true);
        window.reject_show = true;
        handle_main_window_event(MainWindowEvent::Reopen, Some(&window));
        let state = window.state.borrow();
        assert!(!state.minimized);
        assert_eq!(state.focus_attempts, 1);
    }

    #[test]
    fn 主窗口不存在时关闭或恢复请求不会创建窗口() {
        for event in [
            MainWindowEvent::CloseRequested,
            MainWindowEvent::ShowRequested,
            MainWindowEvent::Reopen,
        ] {
            handle_main_window_event::<FakeWindow>(event, None);
        }
    }
}
