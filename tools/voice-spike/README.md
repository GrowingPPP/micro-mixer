# voice-spike

独立测量工程，不属于生产 workspace。

```sh
cd tools/voice-spike
cargo run --release
```

依赖 C++ 工具链与 libclang。macOS 使用 Xcode/LLVM，Windows 使用 MSVC + LLVM。

## pitch_shift 后端说明

`PitchShiftBackend` 是 `pitch_shift 2.1.0` 加一层 NaN 恢复：上游在 FFT bin 恰为 `(0, 0)` 时会产生 NaN 并永久污染相位状态，恢复层会将当前块输出清零并重置内部状态。基准与音质结果代表「pitch_shift + 恢复层」，不是纯上游行为。
