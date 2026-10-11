# 参与开发

面向要改这个仓库的人。**目标是把「怎么构建、怎么测、怎么打包、按什么风格写」
集中在一处**，不用去翻散落的文档。

> 只想用这个软件？看 [README.md](README.md) 就够了。

---

## 1. 环境

| 需要 | 说明 |
|---|---|
| **Rust** | `rust-version = "1.75"`（`Cargo.toml`）。国内网络建议走 [USTC 镜像](https://mirrors.ustc.edu.cn/help/rust-static.html) |
| **Node.js** | 只用于 Tauri 打包（`tauri-cli`）。前端本身**没有构建步骤** —— 是纯静态的 `dist/` |
| **macOS** | 当前只实现了 macOS 后端。Windows 后端尚未开始 |
| **虚拟声卡** | 跑真实链路需要（本机开发推荐 `brew install --cask blackhole-2ch`） |

```bash
cargo build --release          # 编译全部
cargo run -p mixer-cli -- list # 命令行版，看设备
```

开发时更常用图形界面：

```bash
cd apps/mixer-gui && ../../node_modules/.bin/tauri dev
```

---

## 2. 项目结构

```
apps/
  mixer-cli/      命令行版
  mixer-gui/      Tauri 图形界面（dist/ 是纯静态前端，无打包器）
crates/
  audio-core/     引擎：混音、重采样、限幅、解码、设备抽象 —— **平台无关**
  backend-cpal/   音频后端（cpal：macOS CoreAudio / Windows WASAPI）
  hotkeys/        全局热键（macOS Carbon）
  app-config/     配置读写（手写解析器，无 serde）
  remote/         手机遥控（局域网 HTTP）
  selftest/       端到端自检
m0/               M0 可行性验证的工具与产物（已结项）
docs/             设计与计划文档，见 docs/README.md
```

**分层原则**：引擎和界面 100% 跨平台共用，**只有后端和打包方式分平台**。
加 Windows 支持是"补后端"，不是"重写"。

---

## 3. 编码规范

### 硬性要求

```bash
cargo fmt --all --check                    # 格式必须干净
cargo clippy --workspace --all-targets     # 必须 0 告警
```

lint 策略集中在根 `Cargo.toml` 的 `[workspace.lints]`，各 crate 用
`[lints] workspace = true` 继承。**规则只维护一份**，别在单个 crate 里另开。

编码与行尾由 `.editorconfig` + `.gitattributes` 统一（UTF-8 / LF）。
这个仓库全是中文内容和 shell 脚本，**CRLF 会让脚本直接跑不起来**
（报 `bad interpreter`，而且看不出是行尾问题）。

### 代码风格

- **注释写「为什么」，不写「是什么」。** 代码本身说明是什么；
  注释的价值在于记下**当时为什么这么选**、以及**踩过什么坑**。
  例：
  ```rust
  // ⚠️ 必须校验设备还在不在，不能直接拿配置里的名字去开流。
  //    CoreAudio 对不存在设备的查询会**永久阻塞** —— 界面表现就是"点了启动没反应"。
  ```
- **测试函数名用中文**，读起来就是一句断言：`fn 坏文件不应拖垮整批()`。
- **音效/用户可见的文案用中文**；日志前缀用 `[模块]`，如 `[待机]`、`[热插拔]`。
- **实时音频回调里不许分配、不许加锁、不许 IO。** 用 SPSC 环形缓冲传数据、
  原子量传增益。这条没有例外。
- **异步/回调里不许让 panic 跑出去。** 见下。

### panic 策略（改之前先读完这段）

release 用的是 `panic = "unwind"` 而**不是** `abort` —— 因为菜单栏常驻程序
最怕"悄无声息地死掉"：用户正在游戏里，队友突然听不到他，屏幕上却毫无提示。

但 unwind 有个**硬前提**：

> **cpal 的音频回调是从 `extern "C" fn` 里调过来的**（coreaudio-rs 的
> `input_proc` / render proc）。panic 一旦 unwind 出去就是**跨 FFI 边界 = 未定义行为**。

所以 `crates/backend-cpal` 里给两个回调都套了 `catch_unwind`（`guard_input` /
`guard_output`）。**动回调相关代码时不要把这两层去掉。**

配套的另外三层：

| 层 | 在哪 | 作用 |
|---|---|---|
| panic hook | `apps/mixer-gui/src/host.rs` | 落盘 `~/Library/Logs/音效板/panic.log` + 上报界面（打包后没有控制台） |
| 音频看门狗 | `apps/mixer-gui/src/main.rs` | `rendered_frames` 3 秒不涨，或回调 panic 过 → 自动重启音频 |
| 自测钩子 | `MIXER_PANIC_AFTER=<秒>` | **真的诱发一次 panic** 来验证上面这套还有效 |

> 最后一条别删。这套兜底如果不真的诱发一次，就只是"看起来做了" ——
> 冒烟测试里已经固化了这个检查。

---

## 4. 测试

### 提交前必须跑

```bash
cargo test --workspace                       # 68 个单元测试
cargo build --release --workspace            # 0 告警
```

### 改到音频链路时，还要跑

```bash
# 无窗口冒烟：跑通界面调用的全部命令 + 异常路径（33 项）
target/release/bundle/macos/音效板.app/Contents/MacOS/mixer-gui --smoke --dir ~/Sounds

# 端到端自检：真的送测试音进虚拟声卡、再从环回录回来核对
./target/release/mixer selftest

# 双击启动路径（11 项）—— 见下，这条**不能省**
./m0/tools/app_verify.sh

# 设备占用顺序矩阵
./m0/tools/order_test.sh
```

### 为什么必须有 `app_verify.sh`

这个项目踩过一个**只在双击启动时才出现**的致命问题：Info.plist 缺
`NSMicrophoneUsageDescription` 时系统直接拒绝麦克风权限，录音全为零。
而**从终端启动时 TCC 会把权限归因给终端**（终端本来就有权限），
所以**所有命令行测试都会正常通过，完全掩盖这个 bug**。

> **改打包配置（Info.plist / 签名 / entitlements）之后，必须跑 `app_verify.sh`。**

### 两个容易踩的工具链陷阱

- **`cargo build` 不编译 `#[cfg(test)]`** —— 只在 `cargo test` 才暴露的编译错误。
- **`cargo build` 不会刷新 `.app`** —— 只有 `tauri build` 会。所以容易测到旧产物，
  然后困惑"为什么改了没生效"。**打包一律走 build-app.sh。**

---

## 5. 打包

```bash
./apps/mixer-gui/build-app.sh
```

**不要直接 `tauri build`。** 原因是签名的顺序问题：

1. `tauri build` 把 `Info.plist` 合并进 `.app` 之后，**原来的签名就失效了**
2. 而 macOS 的隐私权限（TCC）**绑定代码签名** —— 签名无效时系统既不弹窗也不授权，
   程序只能录到一片静音（不报错、不崩溃）
3. 所以必须在合并之后**重新签名**，然后**才能**打 DMG

脚本会依次做：编译 → 重新签名（带 entitlements）→ 校验 → 打 DMG → **挂载 DMG 再校验一次**。
缺任何一项都直接失败。

### 权限要求取决于签名方式（这条改错过两次）

| 签名方式 | `NSMicrophoneUsageDescription` | 目录用途说明 | `audio-input` entitlement |
|---|---|---|---|
| **ad-hoc**（`codesign -s -`，不开强化运行时） | 必需 | 不需要 | 不需要 |
| **真证书 + 强化运行时**（`--options runtime`） | 必需 | **必需** | **必需** |

"真证书 + 强化运行时"这一列缺任何一项的后果都是**不报错、不弹窗、就是没声音**：

- 缺 entitlement → 麦克风录回来全是零（peak 精确等于 −100 dBFS）
- 缺目录说明 → 读 `~/Downloads` **永久阻塞**，表现为"点了启动没反应"

脚本会把这三项和 DMG 内容一起校验。

### 公证

需要 **Developer ID Application** 证书（Apple Development / Distribution 都不行）。
拿到之后在 `build-app.sh` 里加 `notarytool submit` + `stapler staple` 即可。
在那之前，用户第一次打开需要「按住 Control 点它 → 打开」——
DMG 里附了说明文件解释这一步。

---

## 6. 许可证

本项目 **MIT**（见 `LICENSE`）。加依赖前先确认对方的许可证兼容：

- ✅ **MIT / Apache-2.0 / BSD / ISC / Zlib / MPL-2.0** —— 都可以用
- ❌ **GPL / AGPL** —— 不能用。它们会要求整个产品开源
- ⚠️ **LGPL** —— 动态链接可用，但要谨慎

当前依赖树里**没有任何 GPL/AGPL**（303 个 crate，见第三方声明）。
这条要保持住 —— `cargo tree` 或第三方声明的生成结果都能查。

> 注意区分：**依赖**的许可证（上面的规则）和**虚拟声卡驱动**的许可证
> （BlackHole 是 GPL-3.0）是两回事。自己用没问题，
> 打包进产品分发就受它约束。

## 7. 第三方依赖声明

```bash
python3 tools/gen-third-party-notices.py     # 重新生成 THIRD-PARTY-NOTICES.md
```

MIT / Apache-2.0 / BSD 这类许可证都要求**保留版权声明**，所以发布二进制时
必须带上这份声明。**加了、升级了、删了依赖之后要重新生成。**

生成器做的事：

- 只算**产品实际发布的平台**（macOS arm64 + x86_64 取并集）。
  不过滤的话会把 Windows / Android 专用的依赖也算进来（549 个 vs 实际 303 个），
  列一堆根本不随包分发的东西，反而掩盖了真正要关注的条目
- 排除测试专用依赖（`dev`），排除 workspace 自己的 crate
- 许可证原文**从各 crate 随包分发的 LICENSE 文件里读**，不是从网上抄的
- 有些 crate 只写 `license = "MIT"` 但包里不带 LICENSE 原文（实测 32/303）——
  这些按声明的标识回退到标准原文，并在文件里注明

---

## 8. 提交

- 提交信息用中文，**说清「为什么」**。这个仓库的历史是给人读的。
- **机械改动单独提交** —— 全量格式化、重命名这类不要和功能改动混在一起，
  否则 diff 没法看。
- 移动文件用 `git mv`（保留历史），并**把交叉引用一起改掉、验证链接可达**。

---

## 9. 文档

```
README.md          给使用者：怎么用、怎么装、遇到问题怎么办
CHANGELOG.md       给使用者 + 维护者：改了什么、踩过什么坑
CONTRIBUTING.md    给开发者：就是这份
docs/              设计与计划，见 docs/README.md
```

改代码时如果发现了「不写下来就会重踩」的东西，**加进 CHANGELOG，
并写清现象 → 根因**。这个项目的 CHANGELOG 重点不是功能列表，
而是那些「只在某条路径下才坏」「不报错但就是没声音」的问题。
