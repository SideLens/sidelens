# 音频输出（sidelens-audio-out）

> 第四十一轮新建。把 Opus 解码出的 PCM 送到声卡。

## 为什么是 WASAPI 共享模式（有实测依据）

| 方案 | 延迟 | 代码量 | 独占声卡 |
| --- | --- | --- | --- |
| waveOut | 60-80ms（3-4 buffer x 20ms，**与帧长无关**） | ~120 行 | 否 |
| **WASAPI 共享** | **18.3-32ms** | 608 行 | 否 |
| WASAPI 独占 | 3-10ms | ~700 行 | **是** |

### 关键实测：延迟 ≈ 设备周期 + 帧长

```text
帧长     采样数   latency_ms   write p50
2.5ms      120      18.30ms       3.5us
5.0ms      240      20.00ms       6.3us
10.0ms     480      27.50ms       9.0us
20.0ms     960      31.72ms      10.8us
40.0ms    1920      32.00ms      13.8us
60.0ms    2880      32.00ms      12.1us
```

帧长超过缓冲容量（22ms）后延迟封顶 32ms。

**决定性对比**：2.5ms 帧时共享模式 18.3ms，waveOut 仍 60ms+ —— 差 40ms 以上。
waveOut 会吃掉整条链路的延迟优势（当前视频 p50 12-14ms）。

**为什么不用独占模式**：能再省约 10ms，但会静音用户正在播放的音乐/会议/通知。
省下的还不到 waveOut 与共享模式差距的四分之一。

## 性能

```text
真写路径（缓冲有空间，按 20ms 节拍喂）：
  960 采样 (20ms)    p50 10.400us  p95 20.000us  max 26.600us
  2880 采样 (60ms)   p50 10.600us  p95 22.200us  max 25.100us
缓冲满（Ok(0) 路径）：p50 0.200us
headless：            p50 1.200us
```

预算是 100µs（不能拖慢 1ms tick），余量约 10 倍。

### 一个「假基准」的教训

第一版连续写入 2000 次，测出 p50 = **0.1µs**。那是假的：
99% 的调用走了「缓冲满」路径，**根本没接触 COM**。

改成按 20ms 节拍喂才得到真实的 10.4µs。
**基准测试必须模拟真实调用节奏**，否则测的是空转。

## `latency_ms` 的一个陷阱

`GetStreamLatency` 在本机 Realtek 驱动上返回 **0**（多数板载驱动如此）。
直接采信会得到「空缓冲时延迟 0ms」这种明显错误的乐观估计。

修法：下限取 `max(stream_latency, device_period)`。实测设备周期 10ms。

## 真设备验证：不止于计数器

接了 `IAudioClock::GetPosition`：

```text
播放前 0 帧 -> 播放后 48000 帧，前进 48000 帧
提交 48000 帧
设备消费 / 提交 = 100.0%
```

设备时钟由驱动维护，不受我们的计数逻辑影响 —— 这是「PCM 真的到声卡」的确凿证据。

## 接入 daemon

### 三种模式

| `SIDELENS_AUDIO_OUT` | 行为 |
| --- | --- |
| `1` / `true` | 开真声卡。失败降级 headless |
| `headless` | 只累计计数不发声 |
| 未设置 | 只解码不播放 |

### 接线位置

```rust
// udp_server.rs，Opus 解码之后、audio_played += 1 之前
if let Some(sink) = audio_sink.as_mut() {
    let n = decoded_samples * opus_cfg.channels as usize;
    let n = n.min(opus_pcm.len());
    match sink.write(&opus_pcm[..n]) {
        Ok(0) => audio_write_stalls += 1,   // 缓冲满，丢这一帧
        Ok(_) => {}
        Err(e) => { audio_write_errors += 1; }
    }
}
```

### 接线时的两个坑

**坑 1：`r.samples` 在解码块外不可见**

`r` 只在 `Ok(r)` 分支里可见，而声卡写入在分支外（要保证「解码失败也推进主时钟」
的语义不变）。修法：加 `let mut decoded_samples: usize = 0;` 在块外。

**坑 2：`OpusConfig::channels` 是 `u8`，`PcmFormat::channels` 是 `u16`**

需要显式 `as u16`。

### `AudioSink` 是 `!Send`

COM 对象限制。`udp_server::run` 的 future 不能进 `tokio::spawn`。
当前 `main.rs` 是直接 `.await`，所以没问题 —— 但将来若要把 `run` 挪进 spawn，
需要注意这一条。

## 实测

8 秒，Opus 48kHz 单声道：

```text
audio_decoded         400      （8 秒 x 50fps = 400 帧）
audio_written         383136   采样
audio_write_stalls    0
audio_write_errors    0
audio_out_latency_ms  32.0
```

`383136` vs 期望 `400 x 960 = 384000`，差 864（0.9 帧）—— 最后几帧还在缓冲里。

## 已知限制

| 限制 | 说明 |
| --- | --- |
| 共享模式延迟 32ms（20ms 帧） | 帧长 2.5ms 时可降到 18.3ms |
| 单声道 | `PcmFormat` 支持多声道但当前只测了单声道 |
| 未做设备热插拔 | 声卡切换后需重启 daemon |
| `AudioSink` 是 `!Send` | 不能跨 tokio 任务移动 |
