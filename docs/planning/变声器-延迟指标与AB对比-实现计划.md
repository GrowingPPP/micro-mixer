# 变声器延迟指标与 A/B 对比实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在不改变正式音频输出的前提下，为人声链增加延迟/处理耗时/FIFO 指标，并在人声面板提供按住听原声的 A/B 对比。

**Architecture:** 指标由 `audio-core` 的无锁原子量采集，经 `VoiceControl` 快照到宿主 DTO；A/B 使用独立的运行期 `ab_bypassed` 叠加层，复用现有 `request_enable`/`request_disable`，不写配置、不覆盖用户临时旁路。GUI 只增加只读性能行和按住按钮，所有状态仍以宿主快照为准。

**Tech Stack:** Rust 1.88、现有 `audio-core`/`audio-session`/Tauri 2、手写 `apps/mixer-gui/dist`、Node DOM/IPC 回归。

**本计划只描述方案、接口、测试与命令，不包含实现代码。**

## Global Constraints

- 实时 `process()` 路径不分配、不加锁、不做文件 IO、不调用平台设备 API。
- 指标只使用原子量；音频线程写，控制线程读，不改变音频样本。
- A/B 对比不写配置，不改变 `enabled`、当前预设、参数或用户临时旁路。
- A/B 对比与用户临时旁路、自检暂停、校准暂停相互独立；有效启用取交集。
- 无音频会话、未启用、已用户旁路或暂停时，A/B 命令返回明确错误；释放阶段幂等成功。
- 试听与 A/B 互斥；开始试听先释放 A/B；试听中 A/B 按钮禁用。
- 窗口失焦、页面隐藏、指针取消必须释放 A/B，不能卡在旁路。
- 新增 DTO 字段向后兼容；旧前端忽略即可；无会话时指标显示 `—`，不伪造 0。
- 不引入 AI、模型、网络、音频算法或新依赖。
- 不改变 10 ms 算法块、效果链、路径限幅、停止尾部与现有产品文案。
- `Instant` 测量只用于诊断；若影响实时预算，按每 N 次回调采样，保留 last/max 近似值。
- 测试名称使用中文行为描述；测试不使用真实声卡、系统默认设备或用户音频目录。
- 提交信息用中文，不加工具署名、工具名称或开发进度词。

## File Structure

| 文件 | 职责 |
|---|---|
| `crates/audio-core/src/voice/metrics.rs` | 原子指标、快照、配置与记录 |
| `crates/audio-core/src/voice/control.rs` | `VoiceControl` 指标快照入口 |
| `crates/audio-core/src/voice/chain.rs` | 处理耗时、FIFO 溢出、延迟/块长写入 |
| `crates/audio-core/src/engine.rs` | `VoiceTee` 湿声缺帧计数 |
| `crates/audio-core/src/voice/mod.rs` | 导出指标类型 |
| `apps/mixer-gui/src/voice.rs` | A/B 叠加层、指标 DTO、命令 |
| `apps/mixer-gui/src/main.rs` | `voice_set_ab_bypass` 注册 |
| `apps/mixer-gui/src/host.rs` | 状态 DTO 转发 |
| `apps/mixer-gui/src/voice_tests.rs` | A/B 与指标宿主测试 |
| `apps/mixer-gui/dist/index.html` | 性能行、A/B 按钮 |
| `apps/mixer-gui/dist/main.js` | 状态映射、按住/释放、禁用原因、指标格式 |
| `apps/mixer-gui/dist/style.css` | 性能行与 A/B 按钮样式 |
| `tools/test-gui-controls.js` | GUI 交互回归 |
| `docs/engineering/验证台账.md` | 验证记录 |

---

### Task 1: audio-core 指标结构与 VoiceChain 采集

**Files:**
- Create: `crates/audio-core/src/voice/metrics.rs`
- Modify: `crates/audio-core/src/voice/mod.rs`
- Modify: `crates/audio-core/src/voice/control.rs`
- Modify: `crates/audio-core/src/voice/chain.rs`

**Interfaces:**
- `VoiceMetricsSnapshot`（公开）：`latency_frames: usize`、`sample_rate: u32`、`block_frames: usize`、`process_ns_last: u64`、`process_ns_max: u64`、`overflow_frames: u64`、`gap_frames: u64`。
- `VoiceMetrics`（crate 内部）：`configure(sample_rate, block_frames, latency_frames)`、`record_process(ns)`、`record_overflow(frames)`、`record_gap(frames)`、`snapshot()`。
- `VoiceControl::metrics_snapshot() -> VoiceMetricsSnapshot`；`VoiceControl` 内部持有 `Arc<VoiceMetrics>`，控制句柄克隆共享同一份指标。
- `VoiceChain::new_with_control` 构造时写入 `sample_rate`、`block_frames`、`latency_frames`；`VoiceChain::new` 兼容入口的 `sample_rate = 0`，只保留延迟/块长。
- `VoiceChain::process` 用单调时钟测量整次调用，更新 `process_ns_last` 与 `process_ns_max`。
- `VoiceChain` 每次 `process` 比较 `WetFifo::dropped()` 增量，累加到 `overflow_frames`。
- `VoiceChain::record_gap(frames)` 供 `VoiceTee` 更新 `gap_frames`。

- [ ] **步骤 1: 写失败测试**

在 `voice/metrics.rs` 与 `chain.rs` 测试模块增加：
1. `人声指标包含延迟块长与采样率`：构造 10 ms 块、48 kHz 与已知延迟后端，快照的延迟等于 `后端延迟 + block_frames`，块长与采样率正确。
2. `人声处理耗时被记录且最大值单调`：连续处理多块，`process_ns_last > 0`，`process_ns_max` 不小于任一次 last。
3. `FIFO 溢出计数进入指标`：输出缓冲长期小于输入，FIFO 丢弃后 `overflow_frames` 增加，且与 `overflowed_frames()` 一致。
4. `指标读取不分配且不改变音频输出`：用 `count_allocations` 包住 `process` 与 `metrics_snapshot`，计数为 0；对比无指标读取时 PCM 相同。
5. `旁路不改变延迟与块长指标`：旁路前后 latency/block/sample rate 不变。

- [ ] **步骤 2: 运行测试确认失败**

```sh
cargo test --locked -p audio-core --features voice 人声指标 -- --test-threads=1
```

Expected: 编译失败，提示 `VoiceMetricsSnapshot` 或指标方法不存在。

- [ ] **步骤 3: 实现指标结构**

- 指标槽在 `VoiceControl` 构造时一次分配；所有写入使用 `Relaxed` 或 `AcqRel` 原子操作，不扩容。
- `VoiceChain` 在构造时配置指标；`new` 兼容入口 sample rate 写 0。
- `process` 入口记录 `Instant`，出口写 last、用 `fetch_max` 写 max；旁路也测量拷贝耗时。
- 溢出按 `dropped` 增量累加；不把 `dropped` 绝对值重复累加。
- 指标读取只做原子 load，不加锁、不分配。
- 不改变 `VoiceChain` 的 PCM 输出、效果顺序、FIFO 容量或旁路行为。

- [ ] **步骤 4: 运行测试与检查**

```sh
cargo test --locked -p audio-core --features voice 人声指标 -- --test-threads=1
cargo test --locked -p audio-core --features voice -- --test-threads=1
cargo clippy --locked -p audio-core --all-targets --features voice -- -D warnings
cargo fmt --all --check
```

Expected: 新增测试通过；现有 152 项 voice 测试不变；实时预算不退化。

- [ ] **步骤 5: 提交**

```sh
git add crates/audio-core/src/voice
git commit -m "音频核心：增加人声延迟与处理指标"
```

---

### Task 2: VoiceTee 湿声缺帧计数

**Files:**
- Modify: `crates/audio-core/src/engine.rs`
- Modify: `crates/audio-core/src/voice/chain.rs`（必要时补充 `record_gap` 可见性）

**Interfaces:**
- `VoiceTee` 在 `SteadyWet` 调用 `fill_wet_hold` 时计算 `gap = frames - wet_frames`，调用 `chain.record_gap(gap)`。
- `Warmup`、`FadingIn`、`FadingOut` 的补齐不计入；`Idle` 预览路径不计入。
- 指标只在 `features = voice` 路径存在；无 voice 构建不引用。

- [ ] **步骤 1: 写失败测试**

在 `engine.rs` 测试模块增加：
1. `稳态湿声缺帧被计数`：用可控后端在 SteadyWet 故意少产出湿声，断言 `gap_frames > 0`。
2. `预热与淡出补齐不增加缺帧`：启用/关闭过渡期间 `gap_frames` 保持 0。
3. `试听采集路径不增加缺帧`：关闭态试听只写记录区，`gap_frames` 不增加。
4. `缺帧计数不改变正式转发`：缺口场景下环形缓冲输出仍连续，PCM 与无计数路径一致。

- [ ] **步骤 2: 运行测试确认失败**

```sh
cargo test --locked -p audio-core --features voice 缺帧 -- --test-threads=1
```

Expected: 指标字段存在但 `gap_frames` 始终为 0，断言失败。

- [ ] **步骤 3: 实现缺帧计数**

- 只在 `SteadyWet` 的 `fill_wet_hold` 缺帧时记录。
- `wet_hold` 的补齐逻辑、输出样本和状态机不变。
- `record_gap` 使用 saturating add，避免极端长会话溢出。
- 不新增音频线程分配、锁或 IO。

- [ ] **步骤 4: 运行测试与检查**

```sh
cargo test --locked -p audio-core --features voice 缺帧 -- --test-threads=1
cargo test --locked -p audio-core --features voice -- --test-threads=1
cargo test --locked -p audio-session --features voice -- --test-threads=1
cargo clippy --locked -p audio-core --all-targets --features voice -- -D warnings
cargo fmt --all --check
```

Expected: 全部通过；现有过渡/限幅/停止尾部测试不变。

- [ ] **步骤 5: 提交**

```sh
git add crates/audio-core/src/engine.rs crates/audio-core/src/voice/chain.rs
git commit -m "音频核心：统计稳态湿声缺帧"
```

---

### Task 3: 宿主指标 DTO 与 A/B 叠加层

**Files:**
- Modify: `apps/mixer-gui/src/voice.rs`
- Modify: `apps/mixer-gui/src/main.rs`
- Modify: `apps/mixer-gui/src/host.rs`
- Modify: `apps/mixer-gui/src/voice_tests.rs`

**Interfaces:**
- `VoiceMetricsDto`：`latency_ms: f32`、`block_ms: f32`、`sample_rate: u32`、`process_us_last: u32`、`process_us_max: u32`、`overflow_frames: u64`、`gap_frames: u64`。
- `VoiceStatusDto` 增加 `ab_bypassed: bool`、`metrics: Option<VoiceMetricsDto>`。
- `VoiceState` 增加 `ab_bypassed: bool`；`paused()` 不变；`effective_enabled()` / `effective_bypassed()` 纳入 `ab_bypassed`。
- `VoiceService::set_ab_bypass(active: bool) -> Result<VoiceStatusDto>`。
- 命令 `voice_set_ab_bypass(active: bool) -> VoiceStatusDto`，注册到 `generate_handler!`。
- `metrics` 从 `state.control.as_ref().map(|c| c.metrics_snapshot())` 转换；无控制句柄时为 `None`。
- A/B 只改运行态；`enabled`、`bypassed`、预设、参数、配置不变。

A/B 状态规则：
- `active=true`：要求 control 存在、`enabled=true`、用户 `bypassed=false`、`paused()=false`、试听空闲、阶段为 `wet`；否则返回错误。成功时置 `ab_bypassed=true`、推进 operation、调用 `apply_control`。
- `active=false`：幂等清除 `ab_bypassed`；若有效启用则 `request_enable`，否则保持关闭。
- `apply_enabled`、切换预设、`preview_begin`、`attach`、`detach` 清除 `ab_bypassed`。
- 用户热键/`set_bypass` 只改变用户旁路标志，不清除 `ab_bypassed`；热键在 A/B 期间翻转用户旁路，释放后由用户旁路状态决定是否恢复变声。
- `status().bypassed` 仍返回有效旁路；额外 `ab_bypassed` 让界面区分「用户旁路」与「按住对比」。

- [ ] **步骤 1: 写失败测试**

在 `voice_tests.rs` 增加：
1. `按住对比只改变运行旁路不写配置`：开始/结束 A/B 后 `[voice]` 的 enabled、active_preset、热键不变。
2. `按住对比不覆盖用户临时旁路`：先 `set_bypass(true)`，A/B 命令拒绝；释放不会清除用户旁路。
3. `按住对比释放后恢复有效启用`：启用态 A/B 开始后阶段关闭，释放后重新预热/恢复。
4. `按住对比与试听互斥`：试听中 A/B 拒绝；A/B 中开始试听会取消 A/B。
5. `预热阶段按住对比被拒绝`：阶段不是 Wet 时命令返回「正在预热」，不进入旁路。
6. `切换预设与启停清除按住对比`：每个入口都清 `ab_bypassed`，不会卡在旁路。
7. `无会话或未启用时按住对比返回明确错误`。
8. `会话重建清除按住对比`：attach/detach 后 `ab_bypassed=false`。
9. `状态 DTO 带性能指标`：attach 后 metrics 非空，字段与 `metrics_snapshot` 一致；detach 后为空。
10. `指标浮点换算正确`：48 kHz、480 帧块、960 帧延迟得到 10 ms 块、20 ms 延迟；微秒取整不为负。

- [ ] **步骤 2: 运行测试确认失败**

```sh
cargo test --locked -p mixer-gui --features voice 按住对比 -- --test-threads=1
cargo test --locked -p mixer-gui --features voice 性能指标 -- --test-threads=1
```

Expected: 编译失败，提示字段或命令不存在。

- [ ] **步骤 3: 实现 DTO 与状态机**

- `VoiceMetricsDto` 由 `VoiceMetricsSnapshot` 转换；sample rate 为 0 时 `latency_ms/block_ms` 为 0，前端显示 `—`。
- `process_us_last/max` 由纳秒除以 1000，饱和到 `u32`。
- A/B 命令与现有 `set_bypass` 分开；错误文案明确「音频未启动」「变声未启用」「当前已旁路」「正在试听」。
- `set_ab_bypass(false)` 不依赖 control，释放路径可幂等成功。
- 所有状态变更在 `VoiceService` 状态锁内完成；不触碰生命周期锁。
- 无 voice 特性时命令返回明确错误，不建立待用旁路。
- `StatusDto` 的 `voice` 字段保持原有转发方式。

- [ ] **步骤 4: 运行测试与检查**

```sh
cargo test --locked -p mixer-gui --features voice 按住对比 -- --test-threads=1
cargo test --locked -p mixer-gui --features voice 性能指标 -- --test-threads=1
cargo test --locked -p mixer-gui --features voice -- --test-threads=1
cargo test --locked -p mixer-gui -- --test-threads=1
cargo clippy --locked -p mixer-gui --all-targets --features voice -- -D warnings
cargo fmt --all --check
```

Expected: 全部通过；现有 A/B 之外的宿主/试听/生命周期回归不变。

- [ ] **步骤 5: 提交**

```sh
git add apps/mixer-gui/src
git commit -m "桌面端：增加人声指标与按住对比状态"
```

---

### Task 4: GUI 性能行与按住听原声

**Files:**
- Modify: `apps/mixer-gui/dist/index.html`
- Modify: `apps/mixer-gui/dist/main.js`
- Modify: `apps/mixer-gui/dist/style.css`
- Modify: `tools/test-gui-controls.js`

**Interfaces:**
- 人声面板增加性能行 `#voiceMetrics`，只读文本，`aria-live="off"`。
- 人声操作区增加 `#voiceAbBypass`，文案「按住听原声」。
- `voiceState` 增加 `abBypassed: boolean`、`metrics: { ... } | null`。
- `applyVoiceSnapshot` 映射 `snapshot.ab_bypassed` 与 `snapshot.metrics`；同代次跳过条件必须包含指标字段，否则性能行不会刷新。
- `renderVoiceMetrics()` 只在文本变化时更新，避免读屏和布局抖动。
- `syncVoiceAbBypass()` 计算可用性：
  - 会话存活（running 或 standby）
  - `enabled=true`
  - 用户未旁路/未暂停（`bypassed` 且非 `ab_bypassed` 时禁用）
  - 试听空闲
  - 阶段为 `wet`
- 不可用原因：
  - 音频未启动
  - 变声未启用
  - 当前已临时旁路
  - 正在试听
  - 正在预热
- 事件：
  - `pointerdown` → 开始
  - `pointerup` / `pointercancel` / `lostpointercapture` → 结束
  - `keydown` Space/Enter → 开始；`keyup` → 结束
  - `window.blur`、`visibilitychange` 隐藏、关闭人声面板 → 结束
  - 指针捕获不可用时不影响基础行为。
- 开始/结束都调用 `voice_set_ab_bypass`；命令串行；失败 toast 并恢复按钮。
- 试听按钮点击前先结束 A/B；A/B 按住期间试听按钮禁用。
- A/B 不调用 `save_config`，不写配置。

- [ ] **步骤 1: 写失败测试**

在 `tools/test-gui-controls.js` 增加：
1. `人声面板显示延迟与处理耗时`：快照带 metrics，DOM 文本包含延迟、块长、处理耗时、溢出/缺帧。
2. `按住听原声发送开始与释放命令`：pointerdown/pointerup 各发一次 `voice_set_ab_bypass`，参数 true/false。
3. `指针取消会释放对比`：pointerdown 后 pointercancel，发送 false。
4. `窗口失焦与页面隐藏会释放对比`：模拟 blur/visibilitychange，发送 false。
5. `试听中与预热中按钮禁用并显示原因`：按钮 disabled，title/hint 正确。
6. `用户旁路时按钮禁用`：旁路状态不因 A/B 按钮改变。
7. `按住对比不触发配置保存`：命令列表中无 `save_config`。
8. `指标文本变化不重建面板`：只更新文本，不替换按钮/滑杆节点。
9. `无 metrics 时显示占位符`：显示 `—`，不显示 0。

- [ ] **步骤 2: 运行测试确认失败**

```sh
node --test --test-name-pattern='按住|性能指标|指标' tools/test-gui-controls.js
```

Expected: 控件或命令断言失败。

- [ ] **步骤 3: 实现界面**

- 性能行放在人声面板预设区之后、底部按钮之前，使用现有 hint 样式；窄窗口不产生横向滚动。
- A/B 按钮使用 `ghost` 样式，与试听/编辑并排；按住时加 `active` 类。
- `aria-pressed` 反映按住状态；`title` 给出当前不可用原因。
- `voiceState.bypassed` 为有效旁路；界面状态文案优先显示「对比原声」当 `abBypassed=true`，否则「临时旁路」。
- 指标格式化：
  - 延迟/块长：`22.0 ms` / `10.0 ms`
  - 处理：`0.05 / 0.12 ms`
  - 溢出/缺帧：整数
- 无 metrics：显示 `—`，不显示 `0 ms`。
- 关闭人声面板、切换预设、启停、开始试听都先结束本地 A/B。

- [ ] **步骤 4: 运行测试与检查**

```sh
node --check apps/mixer-gui/dist/main.js
node --test tools/test-gui-controls.js
node --test tools/test-gui-model.js
node --test tools/test-app-packaging.js
cargo fmt --all --check
```

Expected: 全部通过；现有 GUI 回归不变。

- [ ] **步骤 5: 提交**

```sh
git add apps/mixer-gui/dist tools/test-gui-controls.js
git commit -m "界面：增加人声延迟指标与按住对比"
```

---

### Task 5: 完整回归与验证记录

**Files:**
- Modify: `docs/engineering/验证台账.md`
- Modify: `docs/engineering/变声器延迟指标与AB对比.md`（只写最终实现边界与实测）
- Modify: `docs/ai/WORKFLOW.md`（仅补新增测试命令）

**Interfaces:**
- 回归覆盖指标、A/B、试听互斥、热键旁路、会话重建、GUI 失焦/取消。
- 记录实时预算与指标开销；若 `Instant` 测量造成预算退化，记录采样降级方案。
- 真机抽测：按住对比、松开恢复、面板数值、无卡顿；仍不替代 V-18/V-19 真机验收。

- [ ] **步骤 1: 运行完整门禁**

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
cargo test --locked -p audio-core --features voice -- --test-threads=1
cargo test --locked -p audio-session --features voice -- --test-threads=1
cargo test --locked -p mixer-gui --features voice -- --test-threads=1
node --test tools/test-gui-model.js tools/test-gui-controls.js tools/test-remote-controls.js tools/test-app-packaging.js
./tools/win-cross-check.sh
git diff --check
```

Expected: 全部通过；新增指标不改变现有 PCM、过渡、限幅和停止尾部结果。

- [ ] **步骤 2: 实时预算复核**

```sh
cargo test --release --locked -p audio-core --features voice 实时回调 -- --ignored --test-threads=1
cargo test --release --locked -p audio-core --features voice 含移调后端 -- --ignored --test-threads=1
```

Expected: P99 ≤ 周期 15%，最大值 < 周期；与未加指标基线相比无数量级变化。

- [ ] **步骤 3: 写验证记录**

在台账增加带日期的记录：
- B：指标快照、A/B 状态机、试听互斥、重建清除、GUI 交互。
- 记录指标开销与采样降级（若触发）。
- D：真实设备听感、持续监听未实现、Windows 真机。

- [ ] **步骤 4: 提交**

```sh
git add docs/engineering/验证台账.md docs/engineering/变声器延迟指标与AB对比.md docs/ai/WORKFLOW.md
git commit -m "文档：记录人声指标与按住对比验证"
```

---

## 验收门槛

- `VoiceStatusDto.metrics` 在会话存在时返回正确延迟、块长、采样率、处理耗时、溢出与缺帧；无会话为 `None`。
- 指标读取零分配，不改变 PCM；实时预算不退化。
- A/B 按住只改运行态；不写配置、不改启用/预设/用户旁路。
- A/B 在试听、预热、用户旁路、暂停时不可用；释放/失焦/取消必恢复。
- 会话重建、切换预设、启停清除 A/B，不会卡在旁路。
- GUI 性能行与按钮在 920×660 与 660×500 下不溢出，不遮挡底部操作。
- 全部现有回归与新增回归通过；真机抽测完成前不宣称 V-18/V-19 通过。

## Self-Review

**Spec coverage:** 覆盖 spec 的指标模型、DTO、A/B 状态模型、实时约束、错误边界、音频/宿主/GUI 测试与验证记录。未引入 AI、持续监听、遥控或 CLI。

**Placeholder scan:** 每个任务都有文件、接口、失败测试、实现边界、验证命令与提交；未使用「TBD」「稍后补」或没有验收标准的步骤。

**Type consistency:** `VoiceMetricsSnapshot`、`VoiceMetricsDto`、`VoiceStatusDto.ab_bypassed`、`voice_set_ab_bypass`、`VoiceChain::record_gap` 在任务间名称一致；`VoiceChain::new` 的 sample rate 0 兼容语义已明确。
