# 项目上下文

本文说明当前代码的职责和行为，作为定位实现的地图。
开发约定见[根 AGENTS.md](../../AGENTS.md)，流程见[WORKFLOW.md](WORKFLOW.md)，
构建与验证入口见[CONTRIBUTING.md](../../CONTRIBUTING.md)。

## 1. 产品与使用模型

- 公开名称：**麦克风混音助手**，面向 macOS 和 Windows。
- 将本地音频与真实麦克风人声实时混合，输出到已安装的虚拟音频设备。
- 目标语音、会议、直播或游戏应用选择对应虚拟麦克风，对方才能接收混合声音。
- 软件本身不安装自研虚拟音频驱动，也不是变声器；当前依赖外部环回设备。
- 正式播放发送音频；监听让自己同步听到正式播放；试听仅走本机输出。
- 关闭窗口会隐藏窗口并保留常驻会话，退出或彻底释放设备才会断开转发。
- 用户功能与操作说明以[项目 README](../../README.md)为入口。

## 2. 架构与模块边界

仓库是 Rust workspace，桌面 GUI 使用 Tauri 2，CLI 与 GUI 共用音频核心和会话层。
`apps/mixer-gui/dist/` 是直接交付的 HTML、CSS 和 JavaScript，无前端打包步骤。
Node.js 用于 Tauri CLI 和交付脚本回归；它不是桌面 App 的音频运行时。

| 模块 | 当前职责 |
|---|---|
| `crates/audio-core` | 设备抽象、解码、麦克风重采样、单音效混音、限幅、控制队列和统计 |
| `crates/audio-session` | 素材库、监听素材转换、双输出 `Playback` 门控和音频流所有者线程 |
| `crates/backend-cpal` | cpal 后端、原生设备枚举、稳定设备标识、格式转换和回调异常隔离 |
| `crates/hotkeys` | 热键解析、注册、事件投递及平台线程所有权 |
| `crates/app-config` | TOML 持久化、音效稳定 ID、旧格式迁移及平台路径解析 |
| `crates/remote` | 局域网 HTTP 服务、手机控制页、二维码和宿主控制路由 |
| `crates/selftest` | 使用真实后端执行音频链路自检与诊断 |
| `apps/mixer-gui` | 静态界面、Tauri IPC、音频宿主、托盘、权限引导和设备恢复 |
| `apps/mixer-cli` | 设备查询、体检、自检、播放、热键及遥控的命令行入口 |

主数据流：真实麦克风 → 下混 → 漂移控制重采样 → 环形缓冲 → 与素材混音 → 限幅 → 输出。
素材在控制侧解码并准备到输出采样率，实时渲染消费已准备样本。
监听使用独立且持久的输出流，素材适配监听设备采样率；试听由独立工作线程持有流。
试听和正式输出使用共用的立体声联动限幅，再映射设备声道；试听所有者等前视尾部排完后关闭流。
音频流在创建它的所有者线程关闭，主会话、监听、试听和自检各有独立 `StreamHealth`。
核心实时 DSP 不执行文件 IO、解码或阻塞等待；生命周期协调位于会话与宿主控制侧。

### 控制入口

| 入口 | 调用方向 |
|---|---|
| 卡片、窗口数字键、空格、设置 | `dist/main.js` → Tauri `main.rs` 命令 → `AudioHost` |
| 全局热键 | `hotkeys` 事件 → Host 会话回调 → 共用 `Playback` |
| 手机播放与停止 | HTTP → `remote::Shared` 的宿主路由 → 共用 `Playback` |
| 托盘操作 | Tauri 菜单处理 → Host 的播放、停止或生命周期操作 |
| CLI | 命令解析 → `audio-session` 与 `Playback`；不经过桌面 DOM |

GUI 的重操作通过后台 worker 执行。`AudioHost` 的生命周期锁协调启动、待机、释放、自检和恢复。
`remote::Shared::set_routed` 一次发布控制器、条目名称与播放/停止处理器，避免观察到半套新路由。

## 3. 播放与最新意图

| 操作 | 当前语义 |
|---|---|
| 正式播放 A → B | A 短淡出完成后开始 B，不叠加两条音效；连续选择保留最后一次有效选择 |
| 再点当前音效 | 从头重播当前音效 |
| 正式播放 ↔ 试听 | 互相替换；试听等待正式输出与监听停止及限幅尾音结束才消耗样本 |
| 停止播放 / 试听 | 停止音效、监听及试听，并取消尚在解码或等待的旧请求；麦克风继续转发 |
| 关闭音效 | 进入待机，停止已排队音效、拒绝新正式触发；麦克风转发保留，试听仍可使用 |
| 开启音效 | 按配置复用待机会话或启动所需会话，恢复热键及手机控制 |
| 彻底释放 / 退出 | 取消试听、关闭音频流并释放设备；目标应用可能需要改回真实麦克风 |
| 开启循环 | 正式播放与监听重复当前一条音效；试听始终只播一次 |
| 播放中关闭循环 | 保留游标，本轮结束后停止，后续触发只播一次；设置会保存并恢复 |

主输出与监听先共同检查接纳条件，再发布同一选择；任一队列满则整个播放请求被拒绝。
无效槽位、待机或满队列的拒绝不会打断合法试听；循环是独立持久参数，不占播放命令容量。
单条增益可实时更新正式播放、监听和试听，更新不要求重播。
Host 在音频会话之外保存麦克风与音效总音量；人声增益只用于正式麦克风转发，
音效总增益与卡片增益叠加，共用于试听、正式播放和监听。GUI 全局控件范围 −60～+36 dB，−60 dB 静音。
启动票据记录音量版本，会话发布时再次同步最新值，避免排队或开流中的旧配置覆盖用户调整。

“最新意图”由 Host 操作票据、会话代数及界面操作代次共同实现。
GUI 在耗时工作前领取票据；解码、开流、关流或等待结束后再次确认有效性。
热键、手机和托盘也参与宿主控制顺序，旧会话的回调不能修改新会话。
停止后的旧任务不会重新出声；迟到回复、失败提示和轮询状态不能恢复旧高亮或覆盖新操作。
前端分别串行处理设置保存、循环更新和总音量更新，试听与正式播放前冲刷未到期的音量输入；手机页面按点击顺序发送请求。
完整组合约定及证据见[播放操作组合回归](../engineering/播放操作组合回归.md)。

## 4. 素材身份、配置与目录

- TOML 使用 `[[sounds]]` 保存 `id`、路径、名称、增益、标签和热键。
- 旧并行数组仍可读取；旧记录由路径及重复序号生成确定性 ID，下次保存为结构化条目。
- 排序、删除和跳过坏文件按稳定 ID 维护关联；实际加载结果决定卡片与播放槽位映射。
- 未识别配置键可保留往返；配置文本的注释不是持久化保证。
- GUI 与 CLI 共用配置；`Config::collect_sounds` 在显式文件列表非空时优先使用该列表。
  CLI 命令参数中的文件与目录可组合加载，按命令实现收集。
- 音频来源不限于 `~/Downloads`：支持任意可读取目录、文件及已挂载的外接存储。
- GUI 载入目录替换列表，仅扫描该目录当前层；添加文件可以跨目录追加，不复制或移动源文件。
- 素材原路径必须保持可读取；删除卡片只移除库条目，不删除磁盘文件。
- `~/` 展开采用 macOS 的 `HOME`、Windows 的 `USERPROFILE`；绝对路径及 UNC 路径保持原值。

配置通常位于 macOS 的 `$XDG_CONFIG_HOME/mixer/config.toml`，未设置时回退 `~/.config/mixer/config.toml`；
Windows 使用 `%APPDATA%\mixer\config.toml`，环境缺失时按 `app-config` 的明确回退规则处理。
公开名称来自 `tauri.conf.json.productName`；identifier `dev.local.mixer`、可执行文件 `mixer-gui` 和配置目录保持稳定。
名称与产物兼容性见[打包名称与兼容性](../engineering/打包名称与兼容性.md)。

## 5. macOS 与 Windows 的差异

| 方面 | macOS | Windows |
|---|---|---|
| 音频后端 / 设备身份 | CoreAudio / UID | WASAPI / endpoint ID |
| 全局热键 | Carbon 操作调度到主线程 | 管理器由专属消息循环线程持有 |
| 系统授权 / 运行时 | 麦克风与受保护目录授权；签名、用途说明及 entitlement 校验 | WebView2 运行时及音频、热键、安装环境检查 |
| 虚拟设备引导 | BlackHole 等外部环回设备 | VB-CABLE；本软件输出 CABLE Input，目标应用录入 CABLE Output |
| 默认麦克风处理 | 启动记录系统默认输入，提供「切回真实麦克风」；普通启动不自动切到虚拟设备 | 引导用户在目标应用选择设备，不修改系统默认输入 |
| 关闭与恢复主窗口 | 关闭仅隐藏；Dock `Reopen` 或菜单栏图标恢复现有主窗口 | 关闭仅隐藏；系统托盘图标恢复现有主窗口 |
| 分发 | 官方脚本重签 App 后生成并挂载验证 DMG | Windows 宿主构建 NSIS，并按当前产品名与版本筛选产物 |

业务语义由共用引擎、会话、Host 和静态界面实现；平台差异集中在后端、热键及系统集成。
设备枚举和热插拔检查读取原生属性，不通过打开音频流探测；恢复通知携带会话代数。
macOS 已有真实链路及 App/DMG 验证记录，具体结论需结合[验证台账](../engineering/验证台账.md)的时间和条件。
Windows 代码与交叉类型检查不等同于真机验收；真实 WASAPI、热键、PowerShell、NSIS 和托盘行为仍需对应宿主验证。
受控后端 PCM 验证实际渲染样本；DOM 或浏览器的模拟 IPC 验证界面，均不能替代真实设备环回。

## 6. 常见改动定位

| 改动 | 主要文件 |
|---|---|
| 界面、About、提示与快捷键 | `apps/mixer-gui/dist/index.html`、`main.js`、`style.css` |
| 库排序、增删、ID 与热键映射 | `apps/mixer-gui/dist/library-model.js`；`crates/app-config/src/lib.rs` |
| IPC 参数与返回结果 | `apps/mixer-gui/src/main.rs` |
| 生命周期、试听、票据、恢复 | `apps/mixer-gui/src/host.rs`；`host_tests.rs` |
| 主窗口关闭、Dock 与托盘恢复 | `apps/mixer-gui/src/window_lifecycle.rs`；`main.rs` 的原生事件绑定 |
| 单音效切换、循环、增益与渲染 | `crates/audio-core/src/engine.rs`；`crates/audio-session/src/lib.rs` |
| 设备格式、枚举及异常隔离 | `crates/backend-cpal/src/lib.rs`、`native.rs`；`crates/audio-core/src/device.rs` |
| 热键线程与事件 | `crates/hotkeys/src/lib.rs`、`owner.rs` |
| 手机控制页及 HTTP 路由 | `crates/remote/src/control-page.js`、`lib.rs`、`tests.rs` |
| 自检与无窗口冒烟 | `crates/selftest/src/lib.rs`；`apps/mixer-gui/src/smoke.rs` |
| 名称、权限与打包 | `tauri.conf.json`、`Info.plist`、两端 `build-app` 脚本；`tools/app-product-name.sh` |
| 交付脚本回归 / 双端 CI | `tools/test-*.js`；`.github/workflows/quality.yml` |

## 7. 历史资料的适用范围

`m0/`、M0 执行手册和验证方案属于早期可行性阶段，不是当前完整使用流程。
产品方案、商业调研、任务清单与技术设计记录设计意图，可能含旧名称、未落地驱动方案或旧阶段状态。
CHANGELOG 和历史台账保留当时的名称、路径、数量与证据，不能据此恢复旧产品行为。
发生矛盾时，以当前代码、明确的行为回归及可重复验证结果确认实际实现。
本文保留长期上下文；具体测试数量、构建时间及本轮验收结果记录在验证台账中。
