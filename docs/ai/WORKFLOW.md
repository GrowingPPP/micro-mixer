# 实现与验证工作流

本文补充[根 AGENTS.md](../../AGENTS.md)，规定怎样选择回归并报告证据。
命令从仓库根目录执行，依赖与门禁以[双端 CI](../../.github/workflows/quality.yml)、锁文件和交付脚本为准。

## 1. 从任务到可验证改动

1. 查看工作区状态，确认用户目标、已有改动和受影响模块。
2. 阅读[项目上下文](PROJECT_CONTEXT.md)、目录级约定及相关行为回归。
3. 明确正常行为、失败行为和平台影响。复杂状态组合先列操作表，再实施。
4. 缺陷修复先复现；使用实际交付脚本、可控后端、样本或 DOM 建立有效回归。
5. 优先修改共用层，保持 GUI、CLI、热键和手机入口的规则一致。
6. 完成有针对性的验证与独立复核，再执行提交所需门禁。检查充分后停止无目的重复测试。

本仓库已有 Rust 与 Node 内置测试，不另建测试框架。新增依赖才运行必要安装步骤；
Node 构建依赖可用 `npm ci` 从 `package-lock.json` 恢复，常规修改不刷新锁文件。

## 2. 选择验证范围

| 改动 | 首先验证 | 额外关注 |
|---|---|---|
| 纯文档 | 相对链接、实际命令、规则一致性、`git diff --check` | 不为文档启动真实音频 |
| GUI 模型 / 状态 / 控件 | `test-gui-model.js`、`test-gui-controls.js` | 交付 DOM、稳定 ID、旧回复、失败与取消、模态焦点、默认及最小窗口尺寸 |
| Host / IPC / 生命周期 | `mixer-gui` Rust 测试及 GUI 控件回归 | 票据、迟到工作、会话所有权、总增益叠加、试听解码取消及真实样本 |
| 原生窗口隐藏 / 恢复 | `window_lifecycle::tests`；对应平台打包 App | 关闭后保持同一窗口；macOS 实际 Dock 重新打开；双端托盘恢复 |
| DSP / 播放 / 监听 | `audio-core`、`audio-session` 测试 | PCM 单音效、停止尾音、零分配、双输出接纳与循环 |
| 配置 / 路径 | `app-config` 测试 | 旧格式、未知键、布尔 false、Unicode/标点、Windows 平台注入 |
| 手机控制 | `remote` Rust 测试、`test-remote-controls.js` | localhost 路由、实际交付页、请求顺序、退休会话和端口回收 |
| 原生设备 / 热键 | `backend-cpal`、`hotkeys` 测试及双端类型检查 | 注册/关闭线程亲和性；对应平台真机验收另行安排 |
| 名称 / 打包 | `test-app-packaging.js`、完整配置 schema、对应宿主打包 | 权限文案、中文名称、签名/安装内容、旧产物不得代替本次包 |

有针对性的命令示例：

```sh
cargo test --locked -p app-config
cargo test --locked -p audio-core -p audio-session
cargo test --locked -p audio-core --features voice
cargo test --locked -p audio-session --features voice
cargo test --locked -p mixer-gui --bin mixer-gui combo_ -- --nocapture
cargo test --locked -p remote
node --test tools/test-gui-controls.js tools/test-gui-model.js
node --test tools/test-remote-controls.js tools/test-app-packaging.js
```

`voice` 特性捆绑 C++ 源码并通过 bindgen 生成绑定，需要 C++ 工具链与 libclang；
`voice-signalsmith` 是兼容旧命令的别名。`mixer-gui` 与 `mixer-cli` 默认启用 `voice`，
因此默认 `cargo build/clippy/test --workspace` 会编译 C++，本地与 `quality.yml` 双端 CI
都需要可用的 C++ 工具链，并设置 `LIBCLANG_PATH`。`quality.yml` 在 macOS/Windows 安装 LLVM，
并在默认特性门禁之外显式运行 `cargo test -p audio-core --features voice` 与
`cargo test -p audio-session --features voice`。

`tools/win-cross-check.sh` 以 `--no-default-features` 运行，不编译 C++，也不要求上述工具；
它只证明不依赖变声后端的 Windows 目标源码可编译，替换不了 Windows 原生 CI 与真机构建。
`tools/voice-spike/` 是独立 workspace，另有自己的验证命令，不计入该门禁。

完整操作组合的测试选择见[播放操作组合回归](../engineering/播放操作组合回归.md)。

GUI 布局变更同时检查默认与最小窗口、明暗主题、长名称和多条音效，音效库与侧栏滚动不能挤走底部停止。
音效详情、筛选后的播放信息、全局音量双控件同步、点击后的空格停止、保存失败及旧面板回复使用交付 DOM 回归。
浏览器模拟 IPC 和注入平台信息可验证共用界面的布局与平台文案；实际 Windows WebView2 与原生设备另计验收。

## 3. 提交门禁：不使用真实音频设备

代码改动执行以下命令。Rust/JS 回归与双端 CI 保持一致；release 构建和完整 schema 校验是本地补充门禁：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
node --check apps/mixer-gui/dist/main.js
node --check apps/mixer-gui/dist/theme.js
node --check crates/remote/src/control-page.js
node --test tools/test-gui-model.js tools/test-gui-controls.js tools/test-remote-controls.js tools/test-app-packaging.js
cargo build --locked --release --workspace
```

默认命令启用 GUI/CLI 的 `voice` 特性，会编译 C++ 变声后端，需要 C++ 工具链与 libclang。
缺少这些工具时用 `cargo build --locked --workspace --no-default-features` 验证不依赖变声
后端的代码；该构建不含变声能力，不能作为交付产物。

macOS 的快捷巡检：

```sh
./tools/check-all.sh --fast
./tools/check-tauri-config.sh
./tools/win-cross-check.sh
```

- `--fast` 无真实音频步骤，但其 Cargo 调用未包含全部 locked/all-targets 选项，不能代替上面的精确门禁。
- Rust/JS 回归使用临时素材、受控 PCM 和模拟 IPC；HTTP 测试会监听 `127.0.0.1` 临时端口。
  沙箱拒绝本机套接字时，记录环境限制，按执行环境规则申请对应能力；不能删掉网络回归或虚报通过。
- schema 脚本可能下载官方 schema、准备 Python 校验器。下载失败可能跳过，校验器缺失可能退化为顶层字段检查。
  只有输出「完全符合 Tauri 官方 schema」才记录为完整 schema 通过。
- Windows 交叉脚本用临时 `llvm-rc` 桩执行
  `cargo check --locked --all-targets --target x86_64-pc-windows-msvc --workspace --no-default-features`。
  它不编译变声 C++ 后端、不链接、不运行 Windows 程序；输出中的跳过说明不代表变声代码已验证，桩不能用于发布构建。
- Windows 原生 CI 执行上列格式检查、Clippy、Rust 测试、显式变声测试及 JS 检查；它是变声 C++ 后端编译的权威检查，
  交叉检查不能替代；Bash 工具也不是 Windows 原生命令的替代品。

## 4. 打包与设备验收

| 命令 | 宿主 / 行为 |
|---|---|
| `./apps/mixer-gui/build-app.sh` | macOS 构建 App，放许可证，签名，生成 DMG，挂载验签并卸载；不启动 App、不打开音频 |
| `./apps/mixer-gui/build-app.sh --print-artifact-paths` | 只读产物路径，不构建或签名 |
| `./apps/mixer-gui/build-app.sh --print-first-open-note` | 只渲染交付的首次打开说明 |
| Windows `build-app-windows.ps1 -TestSmokeHarness` | 只运行名称、WAV、参数与失败结果门禁；不编译、不打开设备 |
| Windows `build-app-windows.ps1 -SkipSmoke` | 构建 EXE 与 NSIS，跳过真实音频 |
| Windows `build-app-windows.ps1 -NoInstaller -SkipSmoke` | 只构建 EXE，不安装、不打开音频 |
| Windows `build-app-windows.ps1` | 构建包并运行真实音频冒烟；`-NoInstaller` 单独使用仍会运行冒烟 |
| `mixer-gui --smoke --dir <受控目录>` | 真实麦克风、输出、监听、热键和遥控；macOS 还测试系统默认输入切换 |
| `mixer selftest --device <输出> --capture <环回输入>` | 播放测试音并从真实设备录回验证 |
| `mixer doctor` / `mixer list` | 本机设备/配置诊断；不播放音频，doctor 会读取用户配置与文件状态 |

Windows PowerShell 命令示例：

```powershell
.\apps\mixer-gui\build-app-windows.ps1 -TestSmokeHarness
.\apps\mixer-gui\build-app-windows.ps1 -SkipSmoke
```

`cargo build` 仅刷新二进制，不刷新 `.app`。macOS 分发包必须走官方脚本，不能直接 `tauri build` 后跳过重签与 DMG 验证。
Windows 原生打包和安装验收需要 Windows 宿主；两端发布包应来自同一提交与锁文件。
产物路径由 `productName` 推导，不要写死旧名称，也不要挑目录中任意旧安装包当成新结果。

### 真机验收的前提

执行前确认相关测试已经得到授权，复用已有授权；准备独立配置、可控素材、明确的输入与输出端点。
说明是否会播放、录音、注册热键、关闭 App 或切换系统默认设备，核对退出及恢复路径。
使用测试账户或明确的受控会话，不覆盖用户正在使用的配置，不将配置路径缺失视为安全隔离。
完整脚本 `tools/check-all.sh` 会执行真实设备步骤与历史双击验证，不能作为无副作用的默认巡检。

### 历史实验工具

`m0/tools/` 保留早期实验，有些脚本仍依赖固定设备、槽位、录音索引和 `m0/work/` 素材：

- `app_verify.sh` 会强制关闭现有 App，首次启动读取用户配置，后续依赖预先准备的配置和特定环回设备。
  运行前必须审阅当前脚本及前置条件；`--print-app-path` 才是提前退出的只读模式。
- `order_test.sh` 使用历史素材和固定录音索引；不能直接将其注释中的「无客户端」当作当前验收事实。
- 音量、默认设备切换以及播放/录音脚本会改变测试现场；不能把整个目录当作只读诊断工具集。

变更权限或启动流程时仍需双击路径验收，但应按当前设备准备受控方案，不能机械执行不自包含的历史脚本。

## 5. 复核、提交与报告

提交前查看 `git diff --check`、完整 diff 和新增文件，验证文档链接及交付脚本引用。
确认没有把 `target/`、依赖目录、录音、临时 fixture、密钥或用户配置加入暂存区。
按改动目的拆分提交，说明目的和验证；用户要求提交时完成本地提交，推送和发布按明确授权处理。

报告每项检查的实际结果、宿主和证据范围：

| 证据 | 可证明的范围 |
|---|---|
| 受控后端 / 实际 PCM | 进程内样本、播放门控、停止确认及生命周期行为 |
| DOM→模拟 IPC / 浏览器模拟 IPC | 交付界面的事件、显示与请求，不证明真实声卡已播放 |
| Windows 交叉类型检查 | Windows 目标源码可编译，不证明 WASAPI、WebView2 或安装已通过 |
| 真机环回 / 原生热键 | 对应机器、驱动、配置下的实际运行 |
| 签名 / DMG 内容 / NSIS 安装 | 已执行的对应产物步骤，不证明另一平台完成验收 |

在有日期的验证记录中保存命令、结果及未执行项。长期规则不固化测试总数、耗时、个人设备名和临时路径。
