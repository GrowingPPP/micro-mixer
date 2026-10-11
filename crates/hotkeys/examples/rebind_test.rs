//! 反复创建/销毁管理器并注册同一个键 —— 验证是否会失败或泄漏。
fn main() {
    for round in 1..=5 {
        let hk = hotkeys::Hotkeys::bind_to_slots(&["F1".into(), "F2".into()]);
        match hk {
            Ok(h) => println!("第 {round} 轮: 绑定成功（{} 个）", h.len()),
            Err(e) => println!("第 {round} 轮: ❌ 绑定失败 —— {e:#}"),
        }
        // h 在这里 drop
    }
    println!("结束");
}
