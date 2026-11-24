# 音频采集层（`sidelens-audio-in`）

> 与 [audio-out.md](audio-out.md) 同构：把「格式对不对」和「设备怎么用」分开。

## 为什么需要它

到第四十三轮为止，**发送端的所有媒体都是合成的**：视频是渐变 + 移动方块，
音频是 440Hz 正弦。链路每一环都验证过了，唯独**源头是假的**。

`sidelens-audio-in` 补上音频这一侧。视频侧由 `sidelens-capture` 负责。

## 三条路径

| 模式 | 端点类型 | 用途 | 本机可用 |
| --- | --- | --- | --- |
| `Microphone` | capture（`eCapture`） | 麦克风 | ❌ 无设备（`0x80070490`） |
| `Loopback` | **render**（`eRender`）+ `AUDCLNT_STREAMFLAGS_LOOPBACK` | 系统正在播放的声音 | ✅ |
| `Headless` | 无 | CI / 无声卡 / 用户关闭 | ✅ |

## 四个必踩的坑

### 坑 1：loopback 必须传 `eRender`

对 `eCapture` 端点传 `AUDCLNT_STREAMFLAGS_LOOPBACK` 会返回 `E_INVALIDARG`，
而且错误信息毫无线索。loopback 是「渲染端点的镜像」，端点类型必须是 `eRender`。

这是最容易踩的坑：直觉上「采集」就该用 `eCapture`。

### 坑 2：loopback 静音时根本不推进

不是给静音数据，是 `GetNextPacketSize` 恒返回 0。实测：

```text
read 调用 326 次，其中 326 次返回 0（100.0%）
```

**这是正常状态，不是设备故障。** 如果把「read 返回 0」当成错误重连，
会不断重建音频流（每次重建 ~10ms，还会丢设备缓冲里的数据）。

发送端的处理是**补零**而不是重复上一帧 —— 重复会制造出实际不存在的低频信号。

### 坑 3：`AUDCLNT_BUFFERFLAGS_SILENT` 时 `pData` 是 NULL

直接解引用会崩进程。必须按 `frames × channels` 自己填 0。

### 坑 4：`GetNextPacketSize` 必须在 `ReleaseBuffer` **之后**查

归还前引擎不挂新数据，提前查会永远拿到上一包大小。
第一版写成了循环外查一次，结果是「一次 read 只消费一个 packet」，
积压永远清不掉。

## 格式转换：设备给的不是你要的

实测本机 loopback 端点：`48000Hz / 2ch / 32f`。我们要 `48000Hz / 1ch / i16`。

三件事都得做：

| 转换 | 注意点 |
| --- | --- |
| f32 → i16 | 必须 clamp。`1.0 × 32767` 会溢出（`as i16` 是饱和的，但显式 clamp 更清楚） |
| 2ch → 1ch | **不能简单取第一个声道**（会丢一半信息）。用平均 |
| 采样率不匹配 | 返回 `RateMismatch`，**不悄悄变调** |

第三点是刻意的：重采样的 bug 形态是「渐进音调漂移」，
几分钟后才听得出来，而那时已经无从定位。宁可明确报错。

## 延迟怎么算

```text
latency_ms = max(GetStreamLatency(), 设备周期) + 积压帧 / 采样率
```

`GetStreamLatency()` 在本机返回 **0**（与 `audio-out` 实测一致），
所以下限必须取 `max(设备周期)`，否则队列空时报 0ms 是假的。

实测：设备周期默认 10ms / 最小 3ms，采集延迟 10.00ms。

## 真实回环验证（最硬的证据）

方法：用 `sidelens-audio-out` 的 `play_tone` 播 440Hz（振幅 0.25），
同时用本 crate 从 loopback 采集，写 WAV。

```text
=== wasapi-loopback ===
设备格式 48000Hz/2ch/32f，缓冲 1056 帧
采集延迟 10.000ms
采到 62880 采样（音频时长 1.31s / 墙上时间 3.50s），RMS 4582.8，peak 8191，削顶 0
read 2284 次，返回 0 的有 2153 次，丢弃 0 采样
已写入 capture_loopback.wav（125804 字节）
```

**`peak = 8191 = 0.25 × 32767`** —— 与注入信号的振幅**精确一致**。
这一个数字同时证明了：

1. f32→i16 的缩放系数正确（没有增益失真）
2. 双声道降单声道正确（若降混有误，峰值会变）
3. 整条链没有削顶（`削顶 0`）

WAV 头逐字节校验通过（`RIFF` / size 125796 / `WAVE` / `fmt ` / 16 / format=1 /
1ch / 48000 / 96000 / blockalign=2 / 16bit / `data` / 125760），
`.NET SoundPlayer` 能解析。

## 发送端怎么接（第四十四轮）

`--audio-in <synth|loopback|mic>`，默认 `synth`。

采集放在 `AudioOutbound::send_next` 里，**不是**独立 tick：

```text
audio ticker(20ms) -> send_next()
  ├─ pacer.next_frame() -> (pts, duration)   ← PTS 严格等距
  ├─ capture.read(&mut buf)                  ← 有多少拿多少
  ├─ 不足补零 / 超出不截断                    ← 保证 pcm.len() == 960
  └─ Opus 编码 -> 发送
```

**为什么必须这样：** 如果让采集侧的抖动传到 PTS 上，接收端 `AvSync` 的主时钟
就再也稳不住。采集给多给少只影响**载荷内容**，不影响**这一帧声称自己是什么时候的**。

打不开时降级成 synth 而不是退出 —— 没有麦克风的机器照样该能发屏幕。

实测（10 秒，真实 loopback）：

```text
audio_sent=500  audio_frames_expected=500   ← 一帧不差
audio_pts_span_ms=10000.0                   ← 时间线严格等距
audio_kbps=45.3  （原始 PCM 是 768kbps，压了 17 倍）
接收端：audio_decoded=448  audio_decode_failed=0  audio_write_stalls=0
```

## 已知缺口

| 缺口 | 影响 | 修法 |
| --- | --- | --- |
| 不支持重采样 | 44.1kHz 设备直接 `RateMismatch` | 加线性插值路径（注意它的 bug 是渐进漂移，难测） |
| 多声道降混只有「平均」和「直通」 | 5.1 设备请求立体声时中置人声会丢 | 加 Dolby 矩阵 `L + 0.707R` |
| `samples_dropped` 依赖驱动标志 | 实测恒 0，未被真实溢出触发过 | 需要人为制造溢出才能验证 |
| `!Send` | COM 对象不能跨线程 | 要 `tokio::spawn` 得先挪专属线程 |
