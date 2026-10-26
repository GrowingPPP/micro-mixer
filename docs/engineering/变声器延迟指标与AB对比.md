# 变声器延迟指标与 A/B 旁路对比

## 1. 目标

在现有人声效果链上增加两项低风险能力：

1. **可观测性**：向界面提供人声算法延迟、块长、处理耗时、FIFO 溢出和湿声缺帧计数。
2. **A/B 对比**：面板内按住即可临时听原声，松开恢复当前变声；不写配置、不改变启用状态、不覆盖热键临时旁路。

两项都不引入 AI、模型、网络或新的音频算法，不改变正式音频输出。

## 2. 非目标

- 不做 AI 音色转换、模型下载或推理。
- 不做持续人声监听；仍只保留 3 秒试听。
- 不改变 10 ms 算法块、Signalsmith 后端、效果链顺序、路径限幅或停止尾部。
- 不增加可配置块长/延迟模式；当前固定 10 ms 是稳定性边界。
- 不把指标持久化到配置，也不跨会话累计。

## 3. 现状与缺口

现有底层已有部分数据：

- `VoiceChain::latency_frames()`：后端延迟加一个算法块。
- `VoiceChain::overflowed_frames()`：湿声 FIFO 满时丢弃帧数。
- `VoiceTee` 在湿声不足时用 `wet_hold` 补齐，但没有计数。
- `VoiceControl` 只暴露阶段、代次和参数，未暴露指标。
- `VoiceStatusDto` 没有延迟/耗时字段。
- 界面没有性能展示。
- 面板没有 A/B 按钮；只有全局热键临时旁路。

## 4. 用户体验

### 4.1 性能读数

人声面板底部增加一行只读性能信息，默认折叠或弱化显示：

```text
延迟 22.0 ms · 块 10 ms · 处理 0.05 / 0.12 ms · 溢出 0 · 缺帧 0
```

- 延迟：算法 + 适配延迟，按输出采样率换算。
- 块：当前算法块长。
- 处理：最近一次 / 本次会话最大值，单位毫秒。
- 溢出：湿声 FIFO 丢弃帧数。
- 缺帧：稳态湿声不足时用上一帧补齐的帧数。
- 没有音频会话时显示 `—`，不伪造 0。

数值只用于诊断，不作为产品指标承诺。界面不显示 P99；P99 仍由离线/真机验证记录承担。

### 4.2 A/B 按钮

人声面板顶部操作区增加：

```text
[按住听原声]  [试听 3 秒]  [编辑]
```

- 按住：临时旁路，听到原声；松开/取消/失焦：恢复变声。
- 不写配置，不改变启用开关，不改变当前预设，不改变全局热键设置的临时旁路。
- 按钮可用条件：音频会话存在、变声已启用、当前不是用户临时旁路/自检/校准暂停、试听空闲、阶段为 `wet`。
- 不可用时禁用并给出原因：
  - 音频未启动
  - 变声未启用
  - 当前已临时旁路
  - 正在试听
  - 正在预热
- 按下期间试听按钮禁用；开始试听会先释放 A/B。
- 键盘：Space/Enter 按下等同按住，keyup 释放；窗口失焦、页面隐藏、指针取消都必须释放。
- 移动端/触控：pointerdown/pointerup/pointercancel 同样处理。

### 4.3 可访问性

- 按钮有明确可访问名称「按住听原声」。
- 按下状态用 `aria-pressed` 或等价状态表达；释放后恢复。
- 性能行使用 `aria-live="off"`，避免每 250 ms 轮询被读屏反复播报。
- 数值变化只更新文本，不改变布局宽度。

## 5. 指标模型

### 5.1 audio-core

新增只读快照类型 `VoiceMetricsSnapshot`：

| 字段 | 类型 | 含义 |
|---|---|---|
| `latency_frames` | `usize` | 后端延迟 + 一个算法块 |
| `sample_rate` | `u32` | 输出采样率 |
| `block_frames` | `usize` | 算法块帧数 |
| `process_ns_last` | `u64` | 最近一次 `VoiceChain::process` 耗时 |
| `process_ns_max` | `u64` | 本次会话最大处理耗时 |
| `overflow_frames` | `u64` | 湿声 FIFO 丢弃帧数 |
| `gap_frames` | `u64` | 稳态湿声缺帧、由上一帧补齐的帧数 |

`VoiceControl` 增加：

- `metrics_snapshot() -> VoiceMetricsSnapshot`
- 内部 `Arc<VoiceMetrics>`，只含原子量；音频线程写，控制线程读。

`VoiceChain`：

- 构造时把 `latency_frames`、`sample_rate`、`block_frames` 写入指标。
- `process` 用单调时钟测量整次调用耗时，更新 last/max；不分配、不加锁、不做 IO。
- 每次调用比较 FIFO 丢弃计数，把增量累加到 `overflow_frames`。
- 旁路时仍可测量拷贝耗时，但延迟/块长保持不变；不记录为湿声处理。

`VoiceTee`：

- 仅在 `SteadyWet` 发生 `fill_wet_hold` 补齐时累加 `gap_frames`。
- Warmup/FadingOut 的补齐不计入，避免启动噪声。

### 5.2 宿主 DTO

`VoiceStatusDto` 增加：

- `ab_bypassed: bool`
- `metrics: VoiceMetricsDto`

`VoiceMetricsDto`：

| 字段 | 类型 |
|---|---|
| `latency_ms` | `f32` |
| `block_ms` | `f32` |
| `sample_rate` | `u32` |
| `process_us_last` | `u32` |
| `process_us_max` | `u32` |
| `overflow_frames` | `u64` |
| `gap_frames` | `u64` |

没有会话/没有控制句柄时返回 `None` 或全零的 `unavailable` 形状，界面显示 `—`。新增字段向后兼容，旧前端忽略即可。

## 6. A/B 状态模型

`VoiceState` 增加运行期字段：

- `ab_bypassed: bool`：按住对比层，不持久化。

有效状态：

- `effective_enabled = enabled && !bypassed && !ab_bypassed && !paused()`
- `effective_bypassed = bypassed || ab_bypassed || paused()`

交互规则：

- `voice_set_ab_bypass(true)`：只置位 `ab_bypassed`；若有效启用则请求 disable，取消试听。
- `voice_set_ab_bypass(false)`：清位；若有效启用则请求 enable。
- 不修改 `enabled`、`bypassed`、预设、参数、配置。
- 用户热键旁路与 A/B 相互独立：
  - 热键旁路期间 A/B 按钮禁用。
  - A/B 期间按热键：热键状态保留；A/B 释放后仍按热键的旁路状态决定是否恢复。
- 显式启用/关闭、切换预设、开始试听、会话释放、设备重建：清除 `ab_bypassed`。
- 自检/校准暂停期间 A/B 按钮禁用；暂停层不因 A/B 改变。

新增命令：

```text
voice_set_ab_bypass(active: bool) -> VoiceStatusDto
```

命令与现有 `voice_set_bypass` 分开，避免把「按住对比」误写成用户临时旁路。

## 7. 实时与并发约束

- 指标写入只使用原子量；音频回调不分配、不加锁、不做文件 IO。
- `Instant::now()` 只用于测量，不参与音频数据路径；需在性能回归中确认不改变现有 P99 门槛。
- A/B 只走现有 `request_enable` / `request_disable` 路径，不重建链、不重置 FIFO、不改参数。
- 所有命令在 `VoiceService` 状态锁内串行；音频线程只读状态原子量。
- 旧会话/旧命令通过现有代次规则丢弃；A/B 状态在 `attach` / `detach` 时清除。

## 8. 错误与边界

- 没有控制句柄：`voice_set_ab_bypass` 返回明确错误，不建立待用旁路。
- 变声未启用：A/B 命令拒绝，界面按钮禁用。
- 当前已用户旁路/暂停：A/B 命令拒绝，避免覆盖。
- 阶段不是 `wet`：按钮禁用并显示「正在预热」。
- 试听非空闲：按钮禁用；开始试听会先释放 A/B。
- 窗口失焦/页面隐藏/指针取消：前端必须发送 `false`；宿主状态锁内清除，防止卡在旁路。
- 会话重建/释放：清除 `ab_bypassed`；界面按新状态刷新。

## 9. 测试

### audio-core

1. `人声指标包含延迟块长与采样率`
2. `人声处理耗时被记录且最大值单调`
3. `FIFO 溢出计数进入指标`
4. `稳态湿声缺帧计数进入指标`
5. `指标读取不分配且不改变音频输出`
6. `旁路不改变延迟与块长指标`

### 宿主

1. `按住对比只改变运行旁路不写配置`
2. `按住对比不覆盖用户临时旁路`
3. `按住对比释放后恢复有效启用`
4. `切换预设/启停/试听/重建清除按住对比`
5. `无会话或未启用时按住对比返回明确错误`
6. `状态 DTO 带性能指标与可用性`

### GUI

1. `人声面板显示延迟与处理耗时`
2. `按住听原声发送开始与释放命令`
3. `指针取消/失焦/页面隐藏会释放对比`
4. `试听中或预热中按钮禁用并显示原因`
5. `用户旁路时按钮禁用`
6. `按住对比不触发配置保存`
7. `性能数值变化不改变面板布局`

### 回归

- 现有 532 项 workspace 测试不变。
- `audio-core --features voice`、`audio-session --features voice`、GUI/遥控/Node 套件全过。
- 实时预算回归仍满足：P99 ≤ 周期 15%，最大值 < 周期；Rust 分配为 0。
- 现有启用/关闭/旁路过渡、限幅、停止尾部、试听互斥不变。

## 10. 交付边界

- 本阶段只做指标与按住对比，不做持续监听、不做 AI、不做新 DSP。
- 指标只在本机会话内有效，不写配置、不参与预设、不参与远程状态。
- 不改变现有产品文案中的 30 ms 目标；指标用于验证该目标，不替代真机验收。
- 若 `Instant` 测量在真机/低配机器上影响实时预算，回退方案是每 N 个回调采样一次，保留 last/max 近似值；不改变音频输出。

## 11. 实施顺序

1. audio-core：指标结构、`VoiceChain`/`VoiceTee` 记录、单元测试。
2. 宿主：`VoiceStatusDto.metrics`、`voice_set_ab_bypass`、状态机与测试。
3. GUI：性能行、按住按钮、键盘/失焦/试听联动、DOM/IPC 回归。
4. 全量门禁与真机抽测；更新验证台账。
