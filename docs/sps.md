# H.264 参数集解析（SPS）

> 代码：`crates/sidelens-codec/src/sps.rs`（约 990 行）+ `tests/sps.rs`（29 测试）
> 实现者：`sps-parse` 子代理
> 状态：**已完成，29 项测试全绿**

## 为什么需要它

接收端在**不解码**的前提下要知道三件事：

1. 这一路视频的**真实显示分辨率**（决定虚拟摄像头输出什么尺寸）
2. **帧率**（决定抖动缓冲的目标深度）
3. 分辨率**有没有变**（手机旋转、切换前后摄像头 → 解码器必须重建）

这三件事全部躺在 SPS 里，而 SPS 是**比特级**的指数哥伦布编码，
不能按字节读。所以自带一个比特读取器，零外部依赖。

## 两个最常见的错误（都做了专门测试）

### 1. 忘了 cropping

H.264 的编码尺寸必须是 16 的整数倍。1920x1080 的视频实际编码成
**1920x1088**，多出来的 8 行靠 `frame_cropping_flag` 里的
`frame_crop_bottom_offset = 4` 裁掉（单位是 CropUnitY = 2）。

```text
不算 cropping → 报 1088 → 虚拟摄像头输出 1088 行 → 画面底部多一条绿边
```

实现同时暴露两个值，测试里显式断言：

```text
coded_resolution()  = (1920, 1088)
resolution()        = (1920, 1080)   ← 已应用 cropping
```

还有一个**双向裁剪**的测试（1400x1050）：crop 是 `(0, 4, 0, 3)` ——
右边和下边都裁。专门抓「只算 bottom」的实现。

### 2. 忘了 frame_mbs_only_flag

`frame_mbs_only_flag = 0` 表示隔行（场编码），此时
`pic_height_in_map_units_minus1` 数的是**宏块对行**，真实高度要 **×2**。
同时 CropUnitY 也会再乘一次 `(2 - frame_mbs_only_flag)`。

`CropUnit` 严格按 7.4.2.1.1 分两支：

```text
ChromaArrayType == 0  → (1, 2 - frame_mbs_only)
否则                   → (SubWidthC, SubHeightC × (2 - frame_mbs_only))
```

有专门测试 `单色与_4_4_4_的裁剪单位是_1`：`separate_colour_plane_flag=1` 时
若错用 4:2:0 的 2×2，1084 会被算成 1080。

## 不可信输入的三条铁律

SPS 来自网络，随时可能是垃圾：

| 铁律 | 实现 |
| --- | --- |
| **绝不 panic** | 所有越界返回 `SpsError` |
| **绝不死循环** | `ue(v)` 前导零超过 `MAX_UE_LEADING_ZEROS = 32` 立即报错 |
| **绝不无界分配** | `num_ref_frames_in_pic_order_cnt_cycle ≤ 255`、`cpb_cnt_minus1 ≤ 31` |

### 畸形输入测试（6 组，全部断言 `is_err()`）

```text
全 0（1/2/8/64/512 字节）
全 0xFF（1/2/8/64 字节）
逐字节截断（真实 1080p SPS 从 0 截到 26，每段都必须 Err）
前导零爆炸（手工造 40 个前导零）
空载荷 + 空 NALU
cropping 越界
```

## 真实码流样本（6 条，全部来自公开论坛/邮件列表的抓包）

| # | 来源 | 解析结果 |
| --- | --- | --- |
| 1 | Sricam/ONVIF 摄像头 SDP | Baseline 66 / L3.1 / **1280x720** / 无 VUI → 帧率 `None` |
| 2 | StackOverflow 42300009 的 RTMP/FLV 首帧 | High 100 / L4.0 / coded **1920x1088** → **1920x1080** / crop bottom=4 / **30.0 fps** |
| 3 | doom9 论坛的 HD DVD 帧率讨论 | Main 77 / **隔行** `frame_mbs_only=0` / **1920x1088** / **25.0 fps** |
| 4 | libav-user 邮件列表的 fMP4 init segment | Main 77 / 1920x1080 / **29.97002997002997 fps**（nut=1001, ts=60000） |
| 5 | StackOverflow 38094302 的原始 NAL 转储 | Baseline 66 / 1280x720 / 30.0 fps / VUI 含 **NAL+VCL HRD 两段** |
| 6 | 1400x1050 双向裁剪样本 | High 100 / coded **1408x1056** → **1400x1050** |

第 5 条是特意加的：它的 VUI 里 HRD 的 `cpb_cnt_minus1` 表必须逐项读完，
少读一个 `se(v)` 后面就全错位 —— 这是「跳过 ≠ 少读」的直接验证。

**交叉验证**：实现者另写了一个 Python 参考解析器（独立实现，含完整 VUI/HRD），
6 条结果与 Rust 实现逐字段一致。

## 「规范要求但实际常缺失」的字段

| 字段 | 实际情况 | 处理 |
| --- | --- | --- |
| **VUI 整块** | 最常见缺失。摄像头/低延迟编码器基本不带 | `vui: Option<VuiInfo>`，`frame_rate()` 返回 `None`，**不猜默认值** |
| **VUI timing info** | VUI 存在但 `timing_info_present_flag=0` 也很多 | 检查标志位，为假返回 `None` |
| **`fixed_frame_rate_flag`** | 绝大多数码流是 0（可变帧率） | 只作为提示字段暴露，不参与计算 |
| **cropping** | 16 对齐的分辨率通常整块省略 | 为假时四个偏移全置 0 |
| **high profile 扩展块** | Baseline/Main 码流里**整个不存在** | 按 13 个 profile 值条件进入，否则取规范默认值 |
| **`seq_scaling_matrix_present_flag`** | 存在但内部矩阵常为 0 | 逐位读 present 标志，只在为 1 时消费 scaling list |
| **HRD 参数** | 存在但 `cpb_cnt_minus1=0` 居多 | 逐项消费，`> 31` 直接拒绝 |
| **`pic_order_cnt_type=3`** | 规范标「reserved」，真实码流偶有 | 不报错 —— 语法结构与 2 完全相同 |

**统一原则**：缺失 = 该字段在码流里**不存在**（不是「值为 0」），
所以用 `Option` 表达，**绝不编造默认值**。

对帧率尤其重要：把缺失当 30fps 会让抖动缓冲时钟慢慢漂移。

## 分辨率变更检测

`ParameterSetTracker` 喂 NALU 进去，检测「分辨率变了」并返回
`ResolutionChange { from, to }`。

这是真实场景必需：手机旋转屏幕、切换前后摄像头都会触发 SPS 变化，
接收端的解码器必须重建，否则花屏或崩溃。

6 个测试覆盖：首次只建基准 / **A→A→B 第三次报变更** / 横竖互换 /
畸形不污染状态 / 忽略非 SPS / 一段码流两份 SPS / reset 后重建基准。

## 顺带发现的 5 个 nalu.rs 问题

见 [nalu-bugs.md](nalu-bugs.md)。其中 2 个是真 bug（已修）：

1. **无起始码的碎片被当 NALU** → 垃圾被缓存成 SPS → 整路黑屏
2. **尾随 0 裁剪吃掉 `cabac_zero_word`** → NALU 长度与 slice 不一致 → 花屏

另外 3 个是设计缺陷（记录未修）：不校验 `forbidden_zero_bit`、
`ParameterSets` 只存最后一份且每帧克隆、`is_keyframe` 不认 SEI recovery point。

## 测试数

| 项目 | 数量 |
| --- | --- |
| SPS 解析 | 29 |
| nalu 回归（本轮新增） | 9 |
| 全 workspace | **505** |
