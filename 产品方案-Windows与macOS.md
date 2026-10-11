# 产品方案：跨平台游戏声卡（Windows + macOS）

> 目标：做一款把本地音频播进麦克风、让游戏队友和语音通话对方听到的产品。
> 定位平台：Windows + macOS 双端；覆盖游戏内置语音与第三方语音软件。

---

## 1. 市场缺口：领导厂商主动放弃了 macOS

这是整个产品决策里最重要的一条事实。

**Soundpad（这个品类的领导者）官方 FAQ 原文：**

> "Soundpad is built upon a driver model for Windows desktops. **It won't work on Linux or macOS** and also not on any mobile phones... **It's not clear if the core feature is actually possible on other operating systems.** With that in mind, Soundpad was implemented in a non-portable way."
> —— [leppsoft.com FAQ](https://www.leppsoft.com/soundpad/en/help/faq/es/other-os/)

两个结论：

1. **他们明确不做 macOS，而且公开表示"不确定核心功能在其他系统上是否可行"。**
2. **这个判断是错的。** macOS 上完全可行——你机器上的"豆包会议记录虚拟设备"、开源的 BlackHole、以及 Apple 官方的 AudioServerPlugIn 示例都证明了这一点。Soundpad 只是被自己的 Windows 架构锁死了。

**但要准确：macOS 并非完全空白。** 深入调研后确认，**Voicemod 是唯一一个在 macOS 上做成了"自带虚拟麦克风 + 音效板"的产品**（支持 Monterey 12+）。

所以缺口不是"没有人做"，而是更窄、但也更清晰的一条缝：

| | Windows | macOS |
|---|---|---|
| 以 soundboard 为核心、**一次性买断**、装完即用 | ✅ **Soundpad $4.99** | ❌ **没有** |

Mac 上的现有选择，全都有硬伤：

- **Voicemod**：变声为主、soundboard 是附属且有额度限制；**订阅制**（€6.99/月、€29.90/年、€49.99 买断）。更麻烦的是它自己的官方文档承认：**macOS 12.1 起存在影响虚拟音频设备的已知 bug**（虚拟线被忽略/静音/无法识别），并且对 FaceTime 这类没有设备选择器的 App，必须把 Voicemod 设为系统默认麦克风——**"只要不打开 Voicemod，你的麦克风就不工作"**，用完还得手动改回。
- **Rogue Amoeba Loopback $99 + Farrago $55**（组合 $123）：两个 App，专业工具，不是给玩家用的。
- **BlackHole + 播放器**：免费，但用户要自己配 Audio MIDI Setup、建 Multi-Output Device，**没有任何统一的热键面板和人声+音效混音**。
- **所有开源音效板**（AmplitudeSoundboard、aks1n3d/SoundPad）都是**"自带驱动"模式**——README 里直接写着"请自行安装 BlackHole / Virtual Audio Cable"。产品作者自己承认不含驱动。

> **Mac 上所有方案的本质缺陷是同一个：要么是"自带驱动"的半成品，要么是订阅制的变声器，要么是 123 美元的专业工具组合。没有一个是"$10–20 买断、装完即用、音效板为核心"的。**

同时要注意：Soundpad 真正的护城河**不是音频引擎**，而是下面这套东西（见 §5）——兼容性处理 + 逐应用配置指南。这才是要打的仗。

---

## 2. 竞品格局与我们切入的位置

| 产品 | Win | macOS | 自带虚拟驱动 | 价格 | 定位 |
|---|---|---|---|---|---|
| **Soundpad** | ✅ | ❌ | ✅ | **$4.99 买断** | 品类标杆。Steam **28,101 条英文评测、95% 好评**（近 30 天 911 条） |
| **Voicemod** | ✅ | ✅ 12+ | ✅ | 免费 + **€6.99/月 · €29.90/年 · €49.99 买断** | 变声为主，soundboard 附属且有额度 |
| **Loopback** | ❌ | ✅ | ✅ | **$99** | 专业路由，Mac 原生 |
| **Farrago** | ❌ | ✅ | ❌ **依赖 Loopback** | **$55**（组合 **$123**） | 音效板，**不注入麦克风** |
| **VB-CABLE** | ✅ | ✅ | ✅（就是驱动） | 捐赠制（$5 起） | 只有"线"，无 UI / 无音效库 / 无混音 |
| **VoiceMeeter** | ✅ | ❌ | ✅ | 捐赠制 | 专业调音台，学习曲线陡峭 |
| **BlackHole** | ❌ | ✅ | ✅（就是驱动） | 捐赠 $10 / **GPL-3.0** | 纯驱动，无 UI |
| **Virtual Audio Cable** | ✅ | ❌ | ✅ | Lite 免费 / Full **$15–50** | Windows 虚拟线 |
| **AmplitudeSoundboard**（开源） | ✅ | ✅ | ❌ **需自装驱动** | 免费 | README 明文要求 Mac 用户自装 BlackHole |
| **aks1n3d/SoundPad**（开源） | ❌ | ✅ | ❌ **需自装 BlackHole** | 免费 | 自我描述："works with OpenSource app **BlackHole**" |

**关键读数：在「自带驱动 + 播放器 + 支持 macOS」这三项上，只有 Voicemod 一家达标。** 而且它的 soundboard 是变声器的附属功能，不是核心。

**两个说明市场规模的事实：**
1. 一款 **$4.99、Windows-only、功能单一**的小工具，9 年积累了 2.8 万条英文评测、95% 好评 —— **需求稳定、长期、非小众。**
2. Mac 用户要么付 $123（Loopback + Farrago），要么手工折腾 BlackHole，要么订阅 Voicemod。

**我们的切入位置：**

> **「Mac 上唯一买断制、装完即用、以音效板为核心的声卡；Windows 上体验最好的那个。」**

差异性 = **自带驱动**（对手都是"自带驱动半成品"）+ 自动配置引导 + 自动电平校准 + 跨平台一致体验。

---

## 3. 产品定义

**一句话：** 一个常驻后台的声卡 App，你在游戏里按一个键，队友就听到你选的那段音频——同时你的真实说话声完全不受影响。

**目标用户：**
- 主力：Mac 玩家（Steam/云游戏/Discord 重度用户），目前无解
- 次主力：Windows 玩家，对现有方案不满（配置繁琐、音质差、免费方案难用）
- 延伸：直播主、开黑车队、VRChat 用户、线上会议整活

**明确不做：**
- 不做手机端（系统层面不可行，见可行性分析）
- 不做主机端
- 不做专业 DAW/路由工具（不跟 Loopback/VoiceMeeter 抢）

---

## 4. 功能范围

### P0 — 核心闭环（第 1–2 周）

| 功能 | 说明 |
|---|---|
| 音频导入 | mp3 / wav / ogg / m4a / flac，拖拽即导入 |
| 音效库 | 列表 + 搜索 + 收藏 + 自定义名称 |
| **全局热键** | 不切出游戏即可触发。**这是核心体验，做不好整个产品就废了** |
| 播放控制 | 音量、循环、打断、叠加 |
| **混音引擎** | 真实麦克风 + 音效实时混音，独立增益 |
| 输出路由 | 选择虚拟声卡作为输出；麦克风选择虚拟声卡作为输入 |
| 驱动引导 | 检测 VB-CABLE 是否安装，未装则引导安装 |

### P1 — 可用产品（第 3–8 周）

| 功能 | 为什么重要 |
|---|---|
| **麦克风电平校准** | 见 §5.1，这是能不能被听到的生死线 |
| **逐应用配置引导** | 见 §5.2，最大的支持成本来源，也是护城河 |
| **自检工具** | 见 §5.4，避免用户用错误的测试方法得出"没声音"的结论 |
| PTT 自动按键 | 自动帮你按住"按住说话"键，Soundpad 的杀手锏功能 |
| 播放模式 | 只发麦克风 / 只发扬声器 / 两者都发 |
| 托盘常驻 + 开机自启 | |
| 音频裁剪 | 只放副歌，不用整首 |

### P2 — 差异化（第 3–6 月）

- 智能 ducking（放音时自动压低音乐，保证人声清晰）
- 一键"透传纯人声"（紧急沟通时瞬间压掉所有音效）
- 音效云库 / 分享 / 导入合集
- 变声、降噪、EQ
- 游戏内 Overlay

---

## 5. 真正的难点：兼容性与配置引导

**这一节是整个产品成败的关键，比音频引擎重要得多。**

Soundpad 的官方排障文档 [Others hear no sound](https://www.leppsoft.com/soundpad/en/help/faq/zh/others-hear-no-sound/) 暴露了这个品类的全部真实坑：

### 5.1 "74 dB 问题"——最致命也最容易被忽略

Soundpad 官方原文：

> "The voice volume should be above **74 dB**, otherwise certain voice applications may filter out played sounds **misinterpreting them as background noise**."

也就是说：**如果你的麦克风信号电平偏低，对方的降噪算法会把你的音乐直接当成噪音删掉。** 这不是 bug，是语音软件的正常行为。

- **Soundpad 的做法**：做一个测量工具，让用户手动测、手动调，不够响就自己去 Windows 里调麦克风音量。
- **我们应该做的**：**自动电平校准**。App 内测量你的真实麦克风电平 → 自动施加补偿增益 → 保证混音输出始终在安全阈值以上，同时对真实人声做限幅避免爆音。
- **这是可以做得比标杆产品好的地方。**

### 5.2 逐应用配置引导——最大的支持成本，也是护城河

Soundpad 为 **11 个语音应用**分别维护了"关闭降噪/回声消除"的图文指南：Discord、Google Meet、GTA V、Microsoft Teams、Mumble、Red Dead Redemption 2、SCP:Secret Laboratory、Steam、TeamSpeak、Xbox Party、Zoom。

**这说明：用户装完 App 之后，80% 的问题出在"对方语音软件里的设置没关对"。**

我们的做法：
- 内置**语音应用配置向导**，覆盖 Discord / Steam / QQ / 微信 / YY / 各主流游戏内置语音
- 能自动改的就自动改（macOS 可直接设系统默认输入设备；部分应用有配置文件）
- 不能自动改的，做**分步引导 + 截图**
- **一键检测**：读取当前默认输入设备，判断是否指向我们的虚拟麦克风，不对就提示

### 5.3 兼容模式（Soundpad 叫 "setup mode"）

不同声卡/驱动组合下，虚拟麦克风的写入方式需要不同策略。Soundpad 提供了多种 setup mode 让用户切换。

我们至少要准备**两套后端**：
- WASAPI 共享模式（兼容性优先）
- WASAPI 独占 / 低延迟模式（音质优先）

并在检测到异常时自动降级。

### 5.4 自检工具——防止用户用错方法测试

Soundpad 特别提醒：**不要用 Windows 声音设置或网页麦克风测试来验证**，因为它们会绕过虚拟声卡、或者做"降噪"处理，导致明明正常却看起来没声音。

我们必须内置一个**自检回环**：录一段自己的麦克风 + 播放一段测试音 → 回放给用户听，确认链路通畅。

---

## 6. 技术架构

### 6.1 总体结构

```
┌─────────────────────────────────────────────────────────┐
│                      App (Tauri)                        │
│  ┌────────────┐  ┌──────────────┐  ┌────────────────┐   │
│  │  音效库 UI  │  │  全局热键系统 │  │  配置引导/自检  │   │
│  └────────────┘  └──────────────┘  └────────────────┘   │
│                          │                              │
│  ┌───────────────────────▼───────────────────────────┐  │
│  │            音频引擎（Rust 实时线程）                │  │
│  │  解码 → 重采样 → 混音 → 电平校准/限幅 → 输出       │  │
│  │        ▲                        │                 │  │
│  │   真实麦克风采集                  ▼                 │  │
│  └────────────────────────────────┼──────────────────┘  │
└───────────────────────────────────┼─────────────────────┘
                                    ▼
                       虚拟声卡「播放端」
                    (VB-CABLE Input / 自研驱动)
                                    │ 驱动内部环回
                                    ▼
                       虚拟声卡「录音端」= 虚拟麦克风
                                    │
                                    ▼
                    游戏 / Discord / QQ —— 选为输入设备
```

**关键设计：混音必须在 App 内完成。** 这是与"用 VoiceMeeter 拼"的本质区别——只有自己混音，才能做自动增益、ducking、PTT 透传、74dB 补偿。这正是免费方案体验差、也是我们做出差异的地方。

### 6.2 驱动策略：分三步走（**这是最重要的技术决策**）

不要一上来就写驱动。写驱动是 6 个月后的活，不是现在的活。

| 阶段 | 方案 | 理由 |
|---|---|---|
| **阶段 1（现在）** | **捆绑 VB-CABLE** | Win 和 Mac **都有**官方版本；官方明确**允许捆绑进免费或商业应用**（见下） |
| **阶段 2（3–6 月）** | **macOS 自研驱动** | 用 libASPL（MIT）做 AudioServerPlugIn，摆脱第三方依赖 |
| **阶段 3（6–12 月）** | **Windows 自研驱动** | SysVAD 改造 + EV 签名；或继续 VB-CABLE OEM 授权 |

#### 为什么阶段 1 可以用 VB-CABLE

VB-Audio 的[授权页](https://vb-audio.com/Services/licensing.htm)明确写着：

> "We allow to **distribute VB-CABLE package with your Application (free or commercial)** and possibly embed it in your installation package (VB-CABLE silent installation) if our donationware model is applicable."

条件：
- 用户必须能识别出这是 VB-Audio 的产品，并有机会捐赠
- 需要标注来源 www.vb-cable.com
- 官方"鼓励"分发方支付一笔授权费（文中举例 500 / 1000 / 2000 美元），并给出正式发票和分发/再授权许可
- ⚠️ **注意：只有免费的 VB-CABLE 可以捆绑，VB-CABLE A+B / C+D 明确禁止捆绑**。好在单个 cable 对我们足够
- VB-CABLE Mac 版官方标注支持 **macOS 10.10+（Intel / Apple Silicon）**

**收益：几周内就能出可用产品，把精力全部投在体验和配置引导上，而不是驱动开发。**

#### 阶段 2：macOS 自研驱动（技术路径已确定）

调研结论非常明确：

| 方案 | 能否分发虚拟音频设备 | 结论 |
|---|---|---|
| **AudioServerPlugIn**（HAL 插件） | ✅ **可以**，是 Apple 官方推荐的虚拟设备方式 | **选它** |
| AudioDriverKit（dext） | ❌ Apple 明确表示：**不与硬件关联的虚拟音频驱动不给分发 entitlement** | 排除 |

Apple 在 WWDC21 Session 10190 的原话：

> "creates a virtual audio driver that is not associated with a hardware device, and so **entitlements will not be granted for that kind of use case**. ...if a virtual audio driver or device is all that is needed, **the audio server plug-in driver model should continue to be used**."

**AudioServerPlugIn 的实际好处：**
- **不需要任何 entitlement**，不需要 Apple 审批
- 只需 Developer ID 签名 + 公证（notarization）
- 安装到 `/Library/Audio/Plug-Ins/HAL`，重启 `coreaudiod` 即可，**不需要重启 Mac**
- 运行在独立用户态进程，崩溃不会内核恐慌
- 代价：**不能上 Mac App Store**，必须官网分发

**实现基座：[gavv/libASPL](https://github.com/gavv/libASPL) — MIT 许可的 C++17 库**，专门用来快速构建 AudioServerPlugIn 虚拟设备，官方称"已被商业与开源应用使用，可视为生产可用"，实际案例包括 Roc Virtual Audio Device。**MIT 许可意味着可以安全地用于闭源商业产品。**

> ⚠️ **不要用 BlackHole 做基座。** 它的授权是三层结构，很容易踩坑：
> 1. **源码**：GPL-3.0（传染性）
> 2. **官方编译的二进制/安装包**：Copyright © Existential Audio，**All rights reserved**（≠ GPL）
> 3. **名称 / Logo / 商标**：GPL 不授予，需书面许可
>
> README 原文：*"A license is required for all non-GPLv3 projects."* 官网那个 "Donate $10" **是捐赠，不是商业许可**，付了也不改变 GPL 传染性。改 bundle ID 换皮同样不解决。闭源商用必须单独询价。
>
> **所以：自研驱动走 libASPL（MIT），不要走 BlackHole。**

#### 阶段 3：Windows 自研驱动

**先说结论：Windows 上不存在"不签名就能造虚拟麦克风"的后门。** 已逐条排除：

| 候选路径 | 结论 |
|---|---|
| 自研内核驱动 + EV 签名 | ✅ **唯一完全可控的路径**。微软官方明文：自 Win10 1607 起，**未由 Dev Portal 签名的内核驱动一律不加载**；申请签名账号**必须持有 EV 代码签名证书** |
| Audio Processing Object (APO) | ❌ APO 是插在**已有端点**信号链上的 DSP，必须依附现成设备驱动和 INF，**无法凭空产生录音端点** |
| USB/IP 虚拟 USB 音频设备 | ❌ 看似绕开签名，实际**仍需要内核侧的 USB 设备端驱动**（USB/IP + VHCI 或虚拟 USB 控制器），一样要签名；且延迟与时钟同步更差。**未找到任何成熟的产品化先例** |
| 新 Windows API | ❌ 未发现。WASAPI 是客户端 API，没有"注册虚拟 capture endpoint"的能力 |

- 工程上基于微软官方 **SysVAD** 示例改造（它本身就是虚拟音频设备的参考实现），省掉"从零写驱动"，但**签名要求完全不变**
- 反作弊方面：**只要不注入游戏进程就没有问题**。VoiceMeeter / Soundpad 用户量巨大，Vanguard / EAC / BattlEye 均与之共存

### 6.3 技术选型

| 层 | 选型 | 理由 |
|---|---|---|
| 音频引擎 | **Rust**（cpal + symphonia + rubato） | 跨平台一套代码；实时线程安全；无 GC 抖动 |
| UI | **Tauri 2**（Rust 后端 + Web 前端） | 一套 UI 复用双端；安装包小；迭代快；原生质感 |
| 全局热键 | Tauri global-shortcut / rdev | 需要全局、低延迟、不被游戏吞掉 |
| macOS 驱动 | C++ / Objective-C++ + libASPL | AudioServerPlugIn |
| Windows 驱动 | C++ / WDF（SysVAD） | 阶段 3 |

**备用方案**：如果团队更熟 C#/C++，音频层可用 NAudio（Win）/ CoreAudio（Mac）或直接 JUCE。但 Rust + Tauri 是当前性价比最高的组合。

---

## 7. 路线图

| 里程碑 | 时间 | 交付标准 | 验证方式 |
|---|---|---|---|
| **M0 可行性验证** | 第 1 周 | VB-CABLE + 现成播放器，在 Discord + 一款游戏内置语音里跑通 | **队友亲口确认"听到了，而且清楚"** |
| **M1 核心闭环** | 第 2–3 周 | 能导入音频、热键播放、真实麦克风混音不丢 | 自己在游戏里连续用 1 小时不炸音 |
| **M2 可用产品** | 第 4–8 周 | 电平校准 + 配置引导 + 自检 + 托盘 | 找 10 个真实玩家试用，**记录每一个"没声音"的案例并归类** |
| **M3 公测** | 第 3–4 月 | Mac 端 + Windows 端同时发布，官网分发 | 100+ 用户，建立配置问题知识库 |
| **M4 自研驱动** | 第 6 月 | macOS 换自研 AudioServerPlugIn | 摆脱 VB-CABLE 依赖 |
| **M5 商业化** | 第 6–12 月 | 定价上线；Windows 驱动自研或 OEM | |

> **M0 是唯一的 go/no-go 关卡。** 花一周时间、零开发成本。如果队友说听不清，先别写代码——那说明问题在降噪链路，得先解决 §5.1。

---

## 8. 成本与商业模式

### 现金成本

| 项目 | 金额 | 何时需要 |
|---|---|---|
| Apple Developer Program | $99 / 年 | 阶段 2 |
| Windows EV 代码签名证书 | 约 $300–600 / 年 | 阶段 3 |
| VB-CABLE 分发授权费 | $500–2000（官方建议区间） | 正式商业化时 |
| 服务器（音效云库） | 视规模 | 阶段 P2 |
| **前期总投入** | **< $1000** | |

### 人力

| 阶段 | 单人全职 |
|---|---|
| M0–M2（可用产品，双端） | 1.5–2 个月 |
| M3–M4（含 macOS 自研驱动） | 再 3–4 个月 |
| M5（含 Windows 自研驱动） | 再 3–6 个月 |

### 定价锚点（均为官方定价，已核实）

| 竞品 | 价格 | 模式 |
|---|---|---|
| Soundpad | **$4.99** | 一次性买断 |
| Voicemod | €6.99/月 · €29.90/年 · **€49.99 买断** | Freemium + 订阅 |
| Loopback | **$99** | 一次性买断 |
| Farrago | **$55**（与 Loopback 组合 **$123**） | 一次性买断 |
| Virtual Audio Cable | $15–50 | Lite 免费 / Full 付费 |
| BlackHole | 捐赠 $10 | 捐赠（非商业许可） |

**两个读数：**
1. Soundpad 用 $4.99 锚定了"这个功能值 5 美元"——**但这个价格属于 Windows。**
2. **Mac 用户面对的现实是 $123（Loopback + Farrago）或 €29.90/年（Voicemod）**——付费意愿明显高一个量级。

**建议定价：**
- 一次性买断 **$12.99–19.99**（双端授权）。高于 Soundpad，理由充分：自带驱动、自动配置引导、自动电平校准、Mac 原生。
- 或 **免费 + Pro**（免费版限音效数量与热键数，Pro 解锁云库 / 变声 / 无限音效）
- **Mac 单端可以单独定价**：它对应的替代方案是 $123 的两个专业软件
- ⚠️ **不要定价 $4.99。** 那是在跟 Soundpad 的 Windows 舒适区正面拼价格，而我们的价值主要在 Mac。

---

## 9. 风险登记册

| 风险 | 严重度 | 缓解措施 |
|---|---|---|
| **Apple 平台政策不可预测** | 🔴 极高 | 见下方专条 |
| **对方降噪吃掉音频（74 dB 问题）** | 🔴 极高 | 自动电平校准；M0 先验证；逐应用引导关闭降噪 |
| **多声卡/驱动兼容性** | 🔴 高 | 双后端（共享/独占）+ 自动降级 + 自检工具；参考 Soundpad 的 setup mode |
| **macOS 26 上 AudioServerPlugIn 稳定性** | 🟡 中 | 早期就在 macOS 26（你的机器就是 26.6.1）上做兼容测试；调研中已发现 Sequoia 时代有 AudioServerPlugIn 相关的社区问题报告 |
| **驱动安装体验劝退用户** | 🟡 中 | 阶段 1 用 VB-CABLE 静默安装；阶段 2 自研后做一键安装 |
| **不能上 Mac App Store** | 🟡 中 | 官网 + 公证分发；用 Homebrew Cask 做分发渠道 |
| **领导厂商跟进做 Mac 版** | 🟡 中 | 速度优先；用自动配置/自动电平建立体验差 |
| **反作弊误伤** | 🟢 低 | 绝不注入游戏进程；只走系统音频 API |
| **版权/社区规范争议** | 🟢 低 | 产品条款明确禁止用于骚扰；不做版权音乐内置库 |

### 9.1 关于「Apple 平台政策不可预测」——这是 Mac 产品最大的风险

**它不是技术难度问题，而是"你的技术可能一夜之间被系统更新废掉，而且求助可能被拒绝"。**

Rogue Amoeba（Mac 音频领域的头部厂商，做了 20 多年）公开记录过这件事：

> "Beta versions of MacOS 11 broke ACE, our then-current audio capture technology, and the damage looked permanent. When we spoke briefly to Apple during WWDC 2020, **our appeals for assistance were flatly rejected.** ... ACE as we knew it was dead in the water... **things looked grim for the future of our products.**"

他们花了数年切换到全新的 ARK 后端才活下来。**一家头部厂商都经历了这种事，我们必须把它写进风险清单。**

对 AudioServerPlugIn 的具体影响：
- 它是 Apple 官方指定的虚拟设备路径（WWDC21 明确说"虚拟音频设备请继续用 audio server plug-in"），**短期内没有替代品，也没有被废弃**
- 但调研中已发现 **macOS Sequoia 时代**有 AudioServerPlugIn 接口相关的社区问题报告
- **Voicemod 官方承认**：**macOS 12.1 起存在影响虚拟音频设备的已知 bug**（虚拟线被忽略 / 静音 / 无法识别）

**缓解措施：**
1. **在 macOS 26（你的机器就是 26.6.1）上第一天就做兼容测试**，不要等发布前才发现
2. 把驱动层做成**可替换的薄层**，业务逻辑与驱动解耦——万一 Apple 变政策，只换驱动
3. 同时保留"引导用户自装 VB-CABLE"的降级路径作为兜底
4. 关注 Apple 每年的 WWDC 与 macOS 大版本更新，预留兼容性维护工时（**每年都要留**）

---

## 10. 立即行动（本周）

1. **今天（30 分钟，零成本）**：在 Mac 上装 VB-CABLE Mac，用现成播放器手动搭出"播放 → 虚拟麦克风 → Discord"的链路，**让队友确认能听到**。
   - 同时在你的 Windows 机器上做同样验证。
   - **这一步决定要不要继续投入。**
2. **验证降噪影响**：让队友分别在"开启降噪"和"关闭降噪"两种设置下听，记录差异。**这直接决定 §5.1 要投多少工程量。**
3. **验证 Voicemod 的坑**（如果方便）：Voicemod 是 Mac 上唯一的现成对手，装它的免费版体验一遍，重点看它的 soundboard 额度限制和"必须设为系统默认麦克风"的割裂感——**那就是我们要打的位置。**
4. **确定技术栈**：确认 Rust + Tauri 路线。
5. **搭 M1 骨架**：跑通"导入音频 → 全局热键 → 混音真实麦克风 → 输出到虚拟线"。

---

## 附：关键事实来源

**竞品与市场（一手）**
- [Soundpad 官方 FAQ：为何不支持 macOS/Linux](https://www.leppsoft.com/soundpad/en/help/faq/es/other-os/)
- [Soundpad 官方排障：别人听不到声音（74 dB 问题、11 个语音应用的逐应用指南）](https://www.leppsoft.com/soundpad/en/help/faq/zh/others-hear-no-sound/)
- [Soundpad Steam 商店页（$4.99 / 28,101 条评测）](https://store.steampowered.com/app/629520/Soundpad/)
- [Voicemod 支持平台（Windows / macOS Monterey 12+）](https://support.voicemod.net/hc/en-us/articles/360013372900-Voicemod-on-other-platforms)
- [Voicemod：macOS 与 FaceTime 共用的已知问题（macOS 12.1 虚拟设备 bug）](https://support.voicemod.net/hc/en-us/articles/22169835107090)
- [Rogue Amoeba Loopback（$99）](https://rogueamoeba.com/loopback/) ｜ [Farrago（$55）](https://rogueamoeba.com/farrago/)
- [AmplitudeSoundboard README（macOS 需自装 BlackHole）](https://github.com/dan0v/AmplitudeSoundboard)
- [aks1n3d/SoundPad（"works with BlackHole"）](https://github.com/aks1n3d/SoundPad)

**授权与分发**
- [VB-Audio 授权条款（VB-CABLE 可捆绑分发 / 批量价 / 禁止条款）](https://vb-audio.com/Services/licensing.htm)
- [VB-Audio 虚拟线产品页（含 macOS 驱动包）](https://vb-audio.com/Cable/VirtualCables.htm)
- [BlackHole README（"A license is required for all non-GPLv3 projects"）](https://github.com/ExistentialAudio/BlackHole)
- [libASPL：MIT 许可的 macOS 虚拟音频设备库](https://github.com/gavv/libASPL)

**平台技术路径**
- [Apple：创建 Audio Server Plug-in](https://developer.apple.com/documentation/coreaudio/creating-an-audio-server-driver-plug-in)
- [WWDC21 Session 10190：Create audio drivers with DriverKit（虚拟设备不给 entitlement）](https://developer.apple.com/videos/play/wwdc2021/10190/)
- [Microsoft：内核模式驱动签名策略（Win10 1607 起必须 Dev Portal 签名 + EV 证书）](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-)
- [Microsoft：APO 架构（依附已有端点）](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/audio-processing-object-architecture)
- [macOS 虚拟音频设备实现方式对比（AudioServerPlugIn vs AudioDriverKit vs Process Tap）](https://iroirotool.com/knowledge/macos/virtual-audio-driver)
- [Rogue Amoeba：Apple 音频技术变更的亲历记录](https://weblog.rogueamoeba.com/2024/12/13/the-developers-who-came-in-from-the-cold/)

> ⚠️ **数据可信度说明**：Voicemod 的"2500 万注册用户"与"变声器市场规模 $380M–520M"来自第三方统计汇编，**未经官方一手确认**，仅作量级参考。用户痛点部分因 Steam 社区、SteamDB、Apple Developer Forums、Reddit 在本环境被反爬/人机验证拦截，**未能取得社区原始引语**，§5 的痛点均来自厂商官方文档原文。
