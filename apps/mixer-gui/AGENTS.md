# GUI 开发约定

适用于 `apps/mixer-gui/`，在根 [AGENTS.md](../../AGENTS.md) 基础上补充。

## 源码职责

| 文件 | 职责 |
|---|---|
| `dist/index.html` / `style.css` | 双端共用界面结构与样式 |
| `dist/main.js` | DOM 事件、界面操作代次、IPC、状态轮询与即时反馈 |
| `dist/library-model.js` | 音效条目、标签、热键及设备选择的纯模型 |
| `src/main.rs` | Tauri 命令、托盘、退出、平台引导和看门狗 |
| `src/host.rs` | 请求接纳、音频会话、试听生命周期及状态 DTO |
| `src/host_tests.rs` | 使用可控后端的宿主行为与竞争回归 |
| `src/macos_audio.rs` | macOS 输入设备查询与系统默认输入设备设置 |
| `src/smoke.rs` | 需要真实设备的无窗口验收 |

`dist/` 是交付源码。静态 HTML 可以查看布局，但没有 Tauri IPC；直接打开 `index.html` 不能验证真实播放。
保持 `withGlobalTauri` 与 `window.__TAURI__.core.invoke` 的共用路径，不把一端修复写成独立前端。

## 命令与异步状态

- 重操作在 Tauri 后台工作线程执行。开流、关流、解码、权限等待不能阻塞 WebView。
- Tauri 命令顶层参数使用调用端 camelCase，嵌套 `StartConfig` 使用 serde 的 snake_case 字段。
  新增命令同时维护 `generate_handler!` 注册和 DOM→IPC 回归。
- 播放、试听、停止和启停请求在调度重操作前，通过 `begin_playback_operation` 领取 Host 票据。
  Host 在接纳、关闭旧试听后的发布点及提交会话时复核票据，过期请求只能清理自己的资源。
- 不让解码、关流、等待和系统 API 占用票据分配的短状态锁。
  `operationReservationTail` 只串行预定票据，不串行等待慢操作完成。
- 保持界面操作代次、原生操作有效性返回值和状态刷新协同；旧成功、旧失败及旧轮询都不能覆盖最新操作。
  返回取消时跟随原生真实状态，不伪造成功提示或恢复旧高亮。
- 热键、托盘和手机请求走同一 Host 意图序列；不能直接绕过 Host 调用底层播放控制。
- 试听的稳定音频 ID、解码代次、增益和真实样本进度必须一致；合法试听不因拒绝的播放而被取消。
- 启动时跳过坏文件后，从实际加载槽位重建卡片，不能继续使用原始列表下标。
- 停止确认后立即更新本次界面反馈；晚到的新入口状态仍要正确显示。

## 交互与跨平台

- 控件用途要明确。音量滑杆显示 dB，拖动实时影响正式播放、监听及试听；保存按稳定 ID 寻址。
- 循环开关保留显式 `false`。启动过程中的改变必须同步至新会话；改变循环不停止或重播音频。
- 打开 About 不改变音频会话。模态窗口阻止数字/空格播放快捷键，支持 Escape、关闭按钮、焦点恢复和内容滚动。
- 复核默认和最小窗口尺寸（见 `tauri.conf.json`）；关键操作不能被长名称或展开面板挤出可见区域。
- 用户条目、设备名、路径和错误文本进入 HTML 时转义；优先使用 `textContent`。`open_url` 与系统设置保留原生白名单。
- 平台指导由 `platform_info_for` 等可注入逻辑提供。VB-CABLE 输出 `CABLE Input`，目标应用麦克风选 `CABLE Output`；
  不对所有平台要求输入输出名称相同，不在共用错误提示中硬编码 macOS 日志路径。
- 关闭窗口仅隐藏，待机继续人声转发，释放关闭设备，退出等待所有者收尾；不要混淆四种操作。

## 打包与验证

- 产品名来自 `tauri.conf.json.productName`，About 元信息通过 `app_info` 获取；改名同步两端名称回归和权限说明。
- `Info.plist`、`entitlements.plist` 与签名顺序影响 macOS 权限。App/DMG 走 `build-app.sh`，
  Windows 包走 `build-app-windows.ps1`；生成 Rust 二进制不等于刷新了安装包。
- Windows PowerShell 脚本保留 UTF-8 BOM，避免 Windows PowerShell 5.1 误读中文；其他文本使用 UTF-8 / LF。
- GUI 变更使用交付 DOM→IPC 回归；宿主变更加入可控回调与样本断言。不要用颜色变化或命令返回成功证明音频停止。
- 涉及播放状态组合时覆盖主播放↔试听、切换、停止、循环、迟到任务和跨入口请求。
  相关现成命令见 [验证工作流](../../docs/ai/WORKFLOW.md)。
- `smoke.rs`、双击验收和设备顺序工具会操作真实设备；先确认授权与受控配置，不能对正在使用的 App 直接运行历史脚本。
