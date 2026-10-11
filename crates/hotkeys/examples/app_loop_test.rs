//! 极简验证：RunApplicationEventLoop 能被 QuitApplicationEventLoop 打断吗？
fn main() {
    println!("[主线程] 进入 RunApplicationEventLoop");
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(2));
        println!("[工作线程] 调用 QuitApplicationEventLoop");
        hotkeys::quit_app_event_loop();
        println!("[工作线程] 调用完毕");
    });
    let t0 = std::time::Instant::now();
    hotkeys::run_app_event_loop();
    let el = t0.elapsed().as_secs_f32();
    println!("[主线程] 事件循环返回，耗时 {el:.2}s");
    if el > 1.5 && el < 5.0 {
        println!("结论: ✅ QuitApplicationEventLoop 有效");
    } else {
        println!("结论: ❌ 没能正常打断（耗时 {el:.2}s）");
    }
}
