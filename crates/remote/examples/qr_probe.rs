//! 打印一个 URL 的二维码矩阵（供脚本验证用）。
//!
//! 用法: qr_probe "http://10.0.0.1:8765"
fn main() {
    let url = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:8765".into());
    match remote::qr_matrix(&url) {
        Some((w, m)) => {
            println!("{w}");
            for y in 0..w {
                let row: String = (0..w)
                    .map(|x| if m[y * w + x] { '1' } else { '0' })
                    .collect();
                println!("{row}");
            }
        }
        None => {
            eprintln!("生成失败");
            std::process::exit(1);
        }
    }
}
