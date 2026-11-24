# 编解码层

## 分层理由

编码器各平台不同，但**码流字节结构统一**。把纯字节操作抽出来，
各平台实现只负责「喂帧给硬件、取出码流」。

| 平台 | 硬件编码 | 硬件解码 |
| --- | --- | --- |
| Windows | Media Foundation / NVENC / QSV / AMF | 同左 |
| macOS / iOS | VideoToolbox | VideoToolbox |
| Android | MediaCodec | MediaCodec |
| Linux | VA-API / NVENC | 同左 |
| HarmonyOS | AVCodec Kit | AVCodec Kit |

## NALU 处理

### Annex-B 起始码有两种

```text
00 00 01     3 字节，短
00 00 00 01  4 字节，长（关键帧/SPS/PPS 前常用）
```

**两者都要支持。** 只认 4 字节的实现会在真实码流上丢掉一半 NALU ——
这是最容易踩的坑，有专门测试覆盖。

### NALU 头位布局

```text
bit 7    forbidden_zero_bit（必须 0）
bit 6-5  nal_ref_idc
bit 4-0  nal_unit_type
```

关键类型：

| 值 | 类型 | 说明 |
| --- | --- | --- |
| 1-4 | 非 IDR 切片 | P/B 帧 |
| 5 | IDR 切片 | **关键帧** |
| 7 | SPS | 序列参数集 |
| 8 | PPS | 图像参数集 |

### 关键帧判断必须扫全部 NALU

一帧通常是 `[AUD][SPS][PPS][IDR]` 或 `[SEI][IDR]`，
**IDR 不一定在最前面**。只看第一个 NALU 会漏判，
而关键帧判断错了会让抖动缓冲在丢包后无法恢复。

## 参数集缓存

SPS/PPS 只在码流开头或关键帧前出现一次。接收端如果中途加入
（或丢包后请求关键帧），必须能拿到最近一次的参数集。

缓存策略：

- 保存**最新**的一份（分辨率变化时参数集会更新）
- `update_from()` 返回是否变化，供上层决定是否要重发关键帧
- `to_annexb()` 把缓存的参数集拼回去，前置在关键帧前发送

## AVCC 互转

AVCC 用 4 字节大端长度代替起始码，是 Media Foundation 的
`MF_MT_MPEG_SEQUENCE_HEADER` 格式。

```text
Annex-B: 00 00 00 01 [NALU] 00 00 00 01 [NALU]
AVCC:    [len:4][NALU] [len:4][NALU]
```

## 码率统计

### 三个容易做错的地方

| 错误做法 | 后果 | 正确做法 |
| --- | --- | --- |
| 只统计最近 1 秒 | 抖动大，误判拥塞 | 滑动窗口 + 指数平滑 |
| 算术平均 | 被单个大 I 帧拉偏 | 指数平滑（默认 0.2） |
| 忘算协议头 | 低估带宽占用 | 提供含头/不含头两种口径 |

### 实测数据

3% 丢包、8192 字节/帧、60fps：

```text
kbps="4301"  rx_fps="64.8"  kf_ratio="0.000"
```

理论值：8192 字节 × 8 × 60 = 3.93 Mbps，加上 32 字节协议头约 4.08 Mbps。
实测 4.3 Mbps 略高，因为含重传帧。

`kf_ratio=0.000` 是正确的：60fps 下每 120 帧一个关键帧，
1 秒窗口内通常不含关键帧。

### 码率自适应建议

```rust
// 超预算 15% → 建议降到 85%
// 低于预算 60% → 建议升到 75%
// 其余 → 不变（避免抖动）
t.suggest_target(budget_bps)
```

## 测试

| 文件 | 数量 | 覆盖 |
| --- | --- | --- |
| `tests/nalu.rs` | 14 | 切分、起始码、关键帧、参数集、AVCC |
| `tests/bitrate.rs` | 15 | 码率、帧率、开销、平滑、建议、抖动 |
