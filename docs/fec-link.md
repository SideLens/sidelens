# FEC 在真实链路里的接入

> 代码：[crates/sidelens-core/src/fec_link.rs](../../crates/sidelens-core/src/fec_link.rs)
> 相关：[protocol.md](protocol.md)、[udp-nack-report.md](udp-nack-report.md)、[congestion.md](congestion.md)

这一层要解决的问题：**纯 XOR 的 FEC 与 UDP 分片链路怎么对接**。

`fec.rs` 只做数学运算，它不知道「包」在线上长什么样；`fragment.rs` 只管切分片，
不管丢包恢复。两者之间缺一层，负责：

1. 决定哪些分片进同一个 FEC 组
2. 冗余包怎么编号、怎么在线上表示
3. 恢复出来的分片怎么送回重组器

---

## 1. 分组策略：按帧分组，不跨帧

| 策略 | 优点 | 缺点 |
| --- | --- | --- |
| **按帧分组** | 一帧的所有分片同时可恢复，恢复即完整 | 帧小则组小，恢复能力弱 |
| 跨帧分组 | 组大、恢复能力强 | 一组的最后几个分片要等后续帧，**引入额外延迟** |

选按帧分组。实时视频最怕延迟，跨帧分组会让「本帧的冗余包」依赖
「下一帧的数据包」，等于给每一帧都加一个帧间隔的延迟。

代价是组大小 = 帧的分片数。720p 8KB 帧约 7 个分片，`redundant = 2` 时
能救任意 2 个不同余类的丢失。关键帧动辄 30-50 个分片，恢复能力显著更强 ——
这正好符合「关键帧更重要」的直觉。

## 2. 冗余包的线格式（10 字节头）

```text
偏移 0  2B  frame_seq       帧序号（回绕）
偏移 2  2B  group_in_frame  帧内第几个 FEC 组
偏移 4  2B  k               第几个冗余包（0..redundant）
偏移 6  2B  real_count      本组真实包数（不含补齐的空包）
偏移 8  2B  reserved        保留
偏移 10 ..  冗余数据
```

`real_count` 这个字段是被逼出来的，见 §4 坑 6。

## 3. 六个坑（全是设计层面，不是笔误）

### 坑 1：补空包死循环，34GB 内存分配失败

第一版写：

```rust
while !self.encoder.is_full() {
    self.encoder.push(&[]);
}
```

看起来合理，但循环体内还调了 `take_redundant()` —— 它清空组后条件又变 false，
无限补空包，`Vec` 一路涨到 34GB 然后 OOM。

**修法**：循环条件显式写上界，且只在循环外取一次冗余包。

### 坑 2：独立计数器与 encoder 脱节

为了知道「这一帧有没有推过分片」，加了 `self.pending += 1`。
但组满自动 flush 后 encoder 已清空，计数器还留着上一帧的值 ——
`finish_frame` 以为还有半组要补。

**修法**：只用 `encoder.pending()` 作唯一真相，不留第二个计数器。

### 坑 3：组号只用帧序号

一帧的分片数超过 `group_size` 时，同一帧会产出**多个组**的冗余包，
而它们的组号完全相同、`k` 都从 0 开始。接收端无法区分。

实测：`group_size=4` 的 5 分片帧产出 2 个冗余包，`k = [0, 0]`。

**修法**：组号拆成「帧序号 + 帧内组下标」，全局标识 `frame_seq << 16 | group_in_frame`。

### 坑 4：帧序号在 `finish_frame` 才赋值

组满时 `push_fragment` 就已经产出冗余包了 —— 那时 `frame_seq` 还是上一帧的。

实测：push 期产出组 `(0, 0)`，finish 期产出组 `(1, 1)`，同一帧两个组号。

**修法**：加 `begin_frame()`，帧序号在推第一个分片**之前**确定。

### 坑 5：组内下标折算撞车（最深的一个）

`FecDecoder` 的 `index` 是**组内下标**（0..group_size）。一帧跨多组时，
帧内分片下标会超过 group_size，直接传进去越界。

用 `index % group_size` 折算，不同组的下标又会**撞车** ——
组 1 的分片 4 折算成组内 0，把组 0 的分片 0 覆盖了。

实测现象：接收端 `received=4 missing=0`（组 0 看起来齐了），但恢复全失败。

**修法**：每组一个独立的 `FecDecoder`，用 `HashMap<组号, decoder>` 存。
组的生命周期很短（一帧内几个组），收到该组所有冗余包后就可以移除。

### 坑 6：补空包让接收端误判「缺了很多」

发送端把尾组补空包凑满再算冗余，但接收端不知道「这组原本只有 1 个真实分片」。
它看到 `received=1`，算出 `missing=3` —— 超过 `redundant=1` 就放弃恢复。

**修法**：冗余包头加 `real_count` 字段。接收端据此把「补齐的空包」标记为已收到
（它们本来就不需要传输），只关心前 `real_count` 个下标。

## 4. 恢复出的分片怎么送回重组器

这是接入时最容易漏的一步。FEC 恢复出分片后，如果只统计不送回重组器，
重组器不知道这些分片已可用，会一直等，最后还是靠 NACK 补 ——
**白白浪费了 FEC 的 0 延迟优势**。

但有个接口障碍：`DatagramPiece` 要求载荷开头是 8 字节分片头，
其中 `count`（本帧总片数）与 `total_size`（帧载荷总长）是重组器必需的。
而 FEC 只保护分片的**数据部分** —— 恢复时这两个值在丢失的分片头里。

解法：`FecReceiver::push_fragment_with_meta()` 在收到数据分片时顺便记下
`(count, total_size)`，恢复时用 `group_meta(gid)` 取出来，重建完整分片。

```rust
// 收到分片时
rx.push_fragment_with_meta(seq, idx, data, hdr.count, hdr.total_size);

// 恢复时
let meta = rx.group_meta(pkt.group_id());
let mut full = Vec::new();
FragmentHeader { index, count, total_size }.encode(&mut full);
full.extend_from_slice(&recovered_data);
asm.push(DatagramPiece { payload: &full, .. }, now);
```

## 5. FEC 配置自适应

`suggest_fec(loss_ratio, group_size)` 按丢包率选冗余度：

| 包级丢包率 | 建议 |
| --- | --- |
| < 1% | 不启用（冗余包 99% 用不上） |
| 1-3% | `redundant = 1` |
| 3-8% | `redundant = 2` |
| 8-15% | `redundant = 3`（但预期部分恢复） |
| > 15% | 不启用，改为降码率 |

最后一条的依据见 [congestion.md](congestion.md) §4：高丢包时 FEC 开销
随丢包率线性增长，而组内丢失超出可解范围时 FEC 完全失效。

## 6. 与 NACK 的分工

两者互补而非二选一：

```text
收齐本组数据包与冗余包
    ↓
FecReceiver::push_redundant → 恢复可解的分片（0 延迟）
    ↓
仍缺失的下标 → NackGenerator::mark_missing → 走 1 RTT
```

FEC 先手救回可解范围内的丢失，救不回来的交给 NACK。

## 7. 测试

`cargo test -p sidelens-core --test fec_link` → **16 个测试**

| 覆盖 | 测试 |
| --- | --- |
| 包编解码往返 | `packet_roundtrip`、`short_buffer_is_rejected` |
| 无丢包 | `no_loss_recovers_nothing` |
| 单丢恢复 | `single_loss_is_recovered` |
| 双丢不同余类 | `two_losses_in_different_residue_classes` |
| 跨组尾组 | `cross_group_frame_recovers_tail_group` |
| 多组同时丢 | `cross_group_losses_in_both_groups` |
| 帧间隔离 | `groups_are_isolated_between_frames` |
| 配置自适应 | `suggest_fec_follows_loss_table`、`suggest_fec_clamps_redundant_to_group_size` |
| 组管理 | `receiver_tracks_and_drops_groups`、`drop_before_frame_cleans_old_groups` |
| 统计 | `sender_counts_are_tracked` |
| 非法配置 | `invalid_config_is_rejected` |

其中 `cross_group_*` 与 `groups_are_isolated_between_frames` 是坑 3/4/5 的回归测试，
在修好之前全部失败。
## 9. 接入真实链路时发现的两个连环 bug（本轮最重要）

FEC 模块的单元测试全绿、`fec_link` 的 16 个集成测试也全绿，
但接到真实 daemon 上时 **`fec_recovered` 一直是 0**。

实测日志（发送端明明发了 347 个冗余包）：

```text
发送端: fec_reds=347 fec_overhead="27.1%"
接收端: fec_reds=0   fec_recovered=0     ← 一个都没收到
```

### Bug 1：控制帧被重组器吞掉

`UdpReceiver::recv_frame` 无条件调用 `asm.push(piece, now)?` ——
它对**未完成重组**的帧返回 `None`，于是整个函数返回 `None`。

`FrameType::Fec` 的包被喂进重组器（它们不是分片帧，永远凑不齐），
`asm.push` 返回 `None`，函数返回 `None`，**FEC 分支在调用方永远走不到**。

修法：非媒体帧直接包成 `ReassembledFrame` 返回，不经过重组器。

修后 `fec_reds` 从 0 变成 345 —— 但 `fec_recovered` 仍是 0。

### Bug 2：FEC 的输入点错了（更深）

喂 FEC 的代码放在了「重组完成后」的分支里。但那时 `f.payload`
已经是**拼好的整帧**，分片头早没了。`push_fragment` 拿到的是错误的数据段，
XOR 对不上，恢复必然失败。

而 FEC 需要的是**每个分片到达时**的数据段 —— 冗余包保护的是分片级数据。

修法：给 `UdpReceiver` 加 `recv_datagram()`，它对**每个到达的分片**都返回，
附带 `fragment_index` / `fragment_count` / `total_size` / `data`（已去掉帧头与分片头）。

### 修后实测（5% 丢包，6 秒，30fps，8KB/帧）

| 指标 | 修复前 | 修复后 |
| --- | --- | --- |
| `fec_reds` | 0 | **340** |
| `fec_recovered` | 0 | **60** |
| `nack_seqs`（需 NACK 补） | 175 | **21** |
| `loss`（链路丢包率） | 0.051 | **0.020** |

**NACK 请求量下降 88%。** 这正是 FEC 的价值：大部分丢包在本地就被恢复，
不用等 1 个 RTT 的重传。

### 教训

单测全绿不代表集成能跑。这两个 bug 都只在「真实数据流 + 真实重组器」
的组合下才暴露 —— 集成测试（`e2e_pipeline.rs`）用的是直接构造的分片，
绕过了 `recv_frame` 的 `asm.push` 路径，所以也测不出来。

**唯一能抓到它们的是端到端实测。**
## 10. 五档端到端实测（最终数据）

同一环境（本机回环，30fps，8192 字节/帧，5 秒）：

| 场景 | 冗余度 | 发送开销 | 收到冗余 | 恢复分片 | NACK 请求 | 链路丢包率 |
| --- | --- | --- | --- | --- | --- | --- |
| baseline（无 FEC） | — | 0% | 0 | 0 | **128** | 0.045 |
| `--fec-auto`（5% 丢包） | 2 | 26.8% | 286 | 53 | **19** | 0.017 |
| `--fec 1`（2% 丢包） | 1 | 13.7% | 146 | 13 | 27 | 0.016 |
| `--fec 2`（5% 丢包） | 2 | 27.0% | 287 | 52 | 21 | 0.024 |
| `--fec 3`（10% 丢包） | 3 | 38.6% | 410 | 71 | 85 | 0.051 |

### 关键观察

1. **NACK 请求量下降 78-85%**（128 → 19-27）。这是 FEC 的核心价值：
   大部分丢包在本地恢复，不用等 1 个 RTT 的重传。
2. **链路丢包率读数也下降**（0.045 → 0.017）。因为「曾缺失的帧」少了 ——
   很多帧被 FEC 补齐，重组器不再认为它缺失。
3. **开销比理论值高 7 个百分点**（`--fec 2` 理论 20%，实测 27%）。
   原因：帧的分片数（约 7）不是 `group_size`（10）的整数倍，尾组补空包后
   仍要发 2 个冗余包，实际冗余占比 = 2/7 ≈ 29%。
   **`group_size` 应与典型分片数匹配**，否则尾组开销被放大。
4. **`--fec 3` 在 10% 丢包下 NACK 反而更多**（85 vs 21）。
   因为 38.6% 的开销吃掉了带宽，实际送达的数据分片变少。
   这印证了 `suggest_fec` 的决策表：> 8% 丢包时加冗余不划算，该降码率。

### 已知的调优方向

`group_size` 固定为 10 是个粗糙选择。更合理的做法是按帧的实际分片数动态设：

```text
720p 8KB 帧  → 约 7 片   → group_size = 8（尾组只剩 1 片，开销 1/8 = 12.5%）
1080p 30KB 帧 → 约 26 片 → group_size = 32（开销 2/32 = 6.25%）
```

代价是两端必须协商 `group_size`（当前靠约定固定值）。这是下一步的改进项。
## 11. 第三个致命缺口：恢复出的帧从未被播放

FEC 端到端打通后（`fec_recovered=53`），发现一个更隐蔽的问题：

**恢复出的帧只被计数，没有进入抖动缓冲。**

`asm.push()` 返回的完整帧被 `stats.frames_in` 记了一笔就丢掉了 ——
用户看到的画面完全没有改善。FEC 的价值停留在统计数字上。

### 连带问题：时间戳填了 0

重建 `DatagramPiece` 时写的是 `pts_us: 0, send_ts_us: 0`。
而这两个值是延迟统计与音画同步的依据 —— 填 0 会让重组出的帧
被判为「延迟几百万毫秒」，抖动缓冲立刻丢弃它。

即使把帧送进缓冲，也会因为时间戳错误而全部被丢掉。

### 修法

1. `FecReceiver` 增加 `frame_meta: HashMap<组号, (pts_us, send_ts_us)>`，
   收到分片时顺便记下时间戳
2. 恢复时用 `frame_meta(gid)` 取出来填进 `DatagramPiece`
3. 重组出的帧**推进抖动缓冲**（`jb.push(...)`）

### 修后实测（5% 丢包，6 秒）

| 指标 | 修复前 | 修复后 |
| --- | --- | --- |
| `fec_recovered` | 53 | 67 |
| `nack_seqs` | 19 | 24 |
| `expired`（缺片帧超时） | 45 | **9** |
| `loss` | 0.017 | 0.012 |

`expired` 从 45 降到 9 是最有说服力的数字：**缺片帧真的被 FEC 补齐了**，
不再需要等 NACK 或超时丢弃。

### 顺带修的一处 clippy

`push_fragment_with_meta` 加到 8 个参数时 clippy 报 `too many arguments`。
改成 `FragmentInfo` 结构体。这不仅是满足 lint —— 8 个值来源不同
（帧头 3 个、分片头 2 个、载荷 1 个、组内推导 2 个），散传极易顺序传错，
而传错的表现是「恢复出静默的垃圾数据」，没有任何报错。
## 12. 一次失败：解析模型预测 FEC 收益，三次都错

实测发现 `--fec 3` 在 10% 丢包下反而更差（NACK 从 21 涨到 85）。
我想做一个「净收益评估」函数来自动判断 FEC 值不值得开。**三次建模全部失败。**

### 尝试 1：`savings = p * (1-p)^(n-1) * (1-p^r)`

```text
所有丢包率下 savings 都是 6-7%，全部判为负收益
```

错因：把「恰好单丢」的概率当成「可恢复的丢包比例」，
又乘了一次「冗余包活着」的衰减 —— 两次衰减把数字压扁。

### 尝试 2：二项分布 `P(K <= redundant)` 递推

```rust
binom *= ((n - k + 1) / k) * (p / (1.0 - p))  // 数值不稳
```

```text
p=0.10 时 savings 恒等于 1.0，所有配置都判为「值得」
```

错因：`p/q` 递推在 p 较大时放大失控。改成直接算 `C(n,k) p^k q^(n-k)` 后数字正常了
（10% 丢包 / redundant=2 → 救回 96.2%），但见尝试 3。

### 尝试 3：加「带宽压力」二阶项

```rust
let effective_loss = 1.0 - (1.0 - p) / (1.0 + overhead);
let delivery_with_fec = (1.0 - effective_loss) * eff_recoverable;
let delivery_without = 1.0 - p;
let net = delivery_with_fec - delivery_without;
```

```text
所有情况都判负收益，包括实测明确划算的 5% / redundant=2
```

错因：基准不一致。`delivery_without = 1-p` 是「送达率」，
而 `delivery_with_fec` 是「送达率 × 可恢复率」，两者量纲不同。

### 结论：改用实测决策表

三次失败的共同根因：**FEC 的实际收益不是一个丢包率的函数**。它取决于
分片大小分布、组边界对齐、拥塞控制响应、NACK 时序等多个交互因素。

最终实现改成查表（`evaluate_fec`），数据直接来自五档实测：

| 丢包率 | 冗余度 | 实测开销 | 实测 NACK | 判定 |
| --- | --- | --- | --- | --- |
| < 1% | — | 0% | — | 不需要 |
| 1-3.5% | 1 | 13.7% | 27 | 划算 |
| 3.5-7.5% | 2 | 27.0% | 21 | 划算 |
| 7.5-12% | 3 | 38.6% | 85 | 边际 |
| > 12% | — | — | — | 不划算，该降码率 |

**教训**：当解析模型连续多次与实测不符时，应该停止调参、改用实测数据。
继续拟合只会得到一个「看起来有理论依据、实际全错」的函数。


## 13. H.264 接入后暴露的设计边界：按帧分组在低码率视频上几乎失效

### 现象

`--h264 --fec 2 --loss 0.05` 实测（6 秒 / 30fps / 181 帧）：

```text
fec_skipped_frames = 166 / 181   ← 92% 的帧被跳过
fec_reds = 26
fec_recovered = 2
avg_frags = 1.4
```

FEC 几乎不起作用。

### 原因

`should_fec(fragment_count, redundant) = fragment_count > redundant`。

H.264 的帧大小分布是「大量 1.3KB 的 P 帧 + 偶尔 16KB 的 IDR」：

| 帧类型 | 大小 | 分片数 | 是否 FEC |
| --- | --- | --- | --- |
| P 帧 | ~1.3 KB | 2 | ❌ 跳过（2 ≤ redundant=2） |
| IDR 帧 | ~16 KB | 14 | ✅ 用 FEC |

**92% 的帧是 2 分片**，而 `redundant=2` 时判据是「分片数必须 > 2」。
所以只有 IDR 帧走了 FEC。

### 这个跳过判据本身是对的

2 个数据包 + 2 个冗余包 = 100% 开销，而能救回的信息量不超过 2 个包。
用 NACK 补 2 个包的代价低两个数量级。

**问题不在判据，在分组策略**。

### 根本矛盾：按帧分组要求「帧内有足够分片」

`fec_link.rs` 第 10-24 行记录的取舍：

> | 策略 | 优点 | 缺点 |
> | --- | --- | --- |
> | 按帧分组 | 一帧的所有分片同时可恢复，恢复即完整 | **帧小则组小，恢复能力弱** |
> | 跨帧分组 | 组大、恢复能力强 | 一组的最后几个分片要等后续帧，**引入额外延迟** |
>
> 选按帧分组。理由：实时视频最怕延迟。

当时的分片数是「720p 8KB 帧约 7 个分片」，按帧分组够用。
**H.264 把这个前提打破了** —— 真实 720p 30fps 的 P 帧只有 2 个分片。

### 三个可选方向（都需要单独一轮）

| 方案 | 做法 | 代价 |
| --- | --- | --- |
| **A 跨帧分组（固定延迟）** | 攒 N 帧（比如 3 帧 = 100ms）成一组 | 引入 100ms 延迟，对实时视频是灾难 |
| **B 跨帧分组（滑窗）** | 每收到 M 个分片就结一组，不等整帧 | 组的边界与帧不对齐，恢复出的分片要缓存到帧齐 |
| **C 降低 redundant 到 1** | 判据变成「分片数 > 1」，2 分片帧也能用 | 只能救 1 个包，且 50% 开销 |

**B 是唯一可行的**：组的边界按「分片数」而不是「帧」划，
这样 2 分片帧也能凑进一个 10 分片的组（跨 5 帧），
但恢复出的分片必须缓存到对应帧的分片头信息齐了才能送回重组器 ——
这一步 `FecReceiver::group_meta` 已经在做了。

### 另一侧同样失效：帧太大

`latency-matrix` 的 `stress` 配置（10% 丢包 + 64KB 帧 + 音频）暴露了反面：

```text
avg_frags = 55.0          ← 64KB 帧切 55 片
loss      = 0.033         ← 链路丢包只有 3.3%，比 loss-5 的 5.4% 还低
nack_seqs = 1104          ← 是 fec-off 的 6 倍
fec_recovered = 12        ← FEC 几乎无效
playable p50 = 84.97 ms   ← 是基线 21ms 的 4 倍
```

根因：55 片里**至少丢一片的概率是 `1 - 0.9^55 ≈ 99.7%`**，
所以几乎每一帧都在等 NACK。而 `redundant=2` 挡不住 55 片里的多处随机丢失
（要挡住 3% 的包级丢包在 55 片里的多点分布，需要 `redundant` 远大于 2）。

### 合起来看：按帧分组在两侧都失效

| 场景 | 分片数 | FEC 表现 | 根因 |
| --- | --- | --- | --- |
| H.264 720p P 帧 | 2 | `fec_skipped=92%`，几乎不工作 | 组太小（2 片），`should_fec` 直接跳过 |
| 合成 8KB | 8 | 开销 27%，工作正常 | 恰好在「够大但不太大」的甜区 |
| 64KB 大帧 | 55 | `fec_recovered=12`，几乎不工作 | 组太大，`redundant=2` 挡不住多点丢失 |

**按帧分组的有效区间很窄**：分片数要在「`> redundant`」和
「`redundant` 能覆盖的随机丢失数」之间。当前 `redundant=2` 时，
有效区间大约是 **3 ~ 10 个分片**。

而真实视频（H.264）落在 2，大帧落在 55 —— **两侧都在区间外**。

### 当前状态

**未改**。本轮只做了「组大小不抖」的修复（`fec_cfg_updates` 11 → 3），
分组策略的变更留到单独一轮 —— 它要改 `fec_link.rs` 的核心数据结构，
不是调参能解决的。

如实记录：**H.264 与 64KB 大帧两个场景下当前 FEC 配置基本无效**，
这是已知边界，不是 bug。

### 修法方向（下一轮）

按帧分组的根本问题是「组的大小被帧的大小绑死」。解法是**让组的大小独立于帧**：

```text
方案：滑窗分组
  每收到 M 个分片就结一组，不等整帧
  一帧的 2 个分片可以和后面 4 帧的分片凑成 10 分片的组
  恢复出的分片缓存到「对应帧的分片头信息齐了」再送回重组器
```

代价是恢复延迟：一个跨 5 帧的组，最后一片要等到第 5 帧才发得出去。
按 30fps 算，组尾的分片要等 ~130ms。这**不能接受**（超过抖动缓冲深度）。

所以真正的解法可能是**按时间窗分组**而不是按分片数：

```text
每 20ms（一帧间隔）结一组，不管组里有多少分片
→ 组的延迟上界 = 20ms（一帧）
→ 组的大小随码率自适应：低码率时组小（2-3 片），高码率时组大（50+ 片）
```

但这又回到「组太小挡不住丢包」的问题。**需要 `redundant` 也自适应**：

```text
redundant = ceil(组内分片数 × 目标丢包率 × 安全系数)
```

这才是完整的解法 —— 但它是「FEC 自适应」的完整设计，
需要独立的建模与实测，不是一轮能做完的。

## 14. 组大小抖动：H.264 带来的第二个问题（已修）

### 现象

接入 H.264 后实测：

```text
fec_group_usage = "4:5,8:1,16:6,32:2,64:1"   ← 5 个不同组大小
fec_cfg_updates = 11                          ← 6 秒内重建 11 次
```

每次重建都丢掉未完成的组 —— FEC 等于没接。

### 原因

`pick_group_size` 只看当前帧的分片数。合成数据每帧 8192 字节固定，
分片数恒为 7-8，组大小稳定在 8，从来没抖过。

真实 H.264 的帧大小在 **918 ~ 16199 字节**之间剧烈变化，
分片数在 2 ~ 14 之间跳 —— 组大小跟着跳。

### 试过两个方案

| 方案 | 结果 |
| --- | --- |
| 2 倍迟滞（`want >= cur*2 \|\| want*2 <= cur` 才切） | ❌ 仍抖：5 个组大小，11 次切换 |
| **只增不减**（`want.max(cur)`） | ✅ 3 个组大小，3 次切换 |

**迟滞挡不住锯齿**：它只挡「同档位抖动」，挡不住「大帧来了就涨、
下一帧小了就跌」。而 H.264 的帧大小分布（大量 P 帧 + 偶尔 IDR）
让锯齿成为**结构性**的，不是噪声。

「只增不减」把切换次数从 11 降到 3，且全部发生在开头几秒。

### 为什么不浪费带宽

组大小 64 保护 2 分片帧时，理论上 2 个冗余包保护 2 个数据包（100% 开销）。
但 `should_fec` 会直接跳过这种帧 —— 浪费的冗余包根本不会发出去。

实测佐证：`fec_skipped_frames = 166 / 181`，只有 15 个真 IDR 帧用了 FEC。

### 代价

一次会话内组大小只增不减，意味着**长时间运行后组大小会固定在上限 64**。
对当前场景（组大小只影响「一帧内几个组」，而 H.264 帧最多 14 分片）
没有实际影响 —— 14 < 64，永远只有一个组。

但如果将来接入 4K（帧可能 100+ 分片），需要重新评估上限。


## 15. 自适应冗余度（`fec-adaptive` 子代理）

### 核心思路

固定 `redundant=2` 意味着「能救 2 个包」。但：

| 组内分片数 | 3% 丢包下期望丢 | `redundant=2` 够吗 |
| --- | --- | --- |
| 2 | 0.06 | 够（但 100% 开销，`should_fec` 已跳过） |
| 8 | 0.24 | 够（P(≥3 丢) 极小） |
| 55 | 1.65 | **经常不够** |

正确的 `redundant` 应该是「让 P(可恢复) ≥ target」的最小值。

### 实现

```rust
pub fn suggest_redundant(group_size: usize, loss_rate: f64, target: f64) -> usize
```

`P(可恢复) = Σ_{i=0}^{r} C(k,i) p^i (1-p)^(k-i)`，求最小 `r` 使其 ≥ target（默认 0.99）。

**两个近似**（都在代码注释里说明）：

1. 忽略冗余包自身的丢失 —— 代价是实际恢复率略低于目标
2. 丢包率来自 NACK 反解，需要先反解回包级（`unresolve_loss`）

### 迟滞（`RedundantHysteresis`）

`redundant` 变化必须发 `FrameType::Config`，抖动会导致「每帧两个 Config 帧，
接收端 FEC 状态永远刚被清空」。

默认参数：降幅度 ≥ 2、升驻留 8 帧、降驻留 30 帧。

### 实测（四档场景）

| 场景 | overhead | hyst_switches | recovered | nack_seqs | playable p50/p95 |
| --- | --- | --- | --- | --- | --- |
| 8KB / 3% 丢包 | 27.5% | 0 | 51 | 6 | 20.93 / 21.97 |
| 8KB / 10% 丢包 | 25.5% | 1 | 112 | 128 | 20.96 / 21.98 |
| 64KB / 3% 丢包 | 22.8% | 10 | 346 | 147 | 21.06 / 22.41 |
| 8KB / 无丢包 | 28.2% → **15.9%** | 0 → 1 | 0 | 0 | 20.99 / 22.03 |

### 主线发现并修掉的一个真 bug：降到下限被挡住

**症状**：无丢包时 `fec_recovered = 0` 但 `fec_overhead = 28.2%` ——
发了 100% 用不上的冗余包，10 秒白白多发 **723 KB**。

**根因**：链路干净时 `suggest_redundant` 返回 0，调用方钳到 1。
而 `observe` 的降幅度闸门是：

```rust
if !up && want.abs_diff(self.current) < self.min_delta_down {
    return None;   // 2 → 1：abs_diff = 1 < 2，被挡住
}
```

**永久卡在 2**。

**修法**：降到下限（`want <= 1`）时不受幅度限制。

```rust
let at_floor = want <= 1;
if !up && !at_floor && want.abs_diff(self.current) < self.min_delta_down {
    return None;
}
```

语义上也该放行：`min_delta_down` 的目的是「挡住 2↔3 这种 ±1 抖动」，
而降到 1 是**触底**，下面没有更小的值可以抖回来 —— 不构成抖动。

**修复后实测**：

```text
fec_redundant: 2 → 1
fec_overhead:  28.2% → 15.9%
fec_red_bytes: 723,604 → 407,478（省 316 KB / 10 秒）
fec_hyst_switches: 0 → 1
```

### 仍存在的结构下限

理想值是 `redundant = 0`（完全不发冗余），但 `FecConfig` 要求 `redundant >= 1`
—— 这是类型层面的约束，改它要动 `fec.rs` 的 XOR 引擎（已稳定，不动）。

所以当前 15.9% 的 overhead 是**结构下限**，不是调参空间。

## 16. 关键帧保护：按帧类型分两条策略

### 问题的量级

第 15 节的自适应冗余度对**所有帧一视同仁**。但关键帧和 P 帧丢一个的代价差 30 倍：

| 帧类型 | 丢了会怎样 | 代价 |
| --- | --- | --- |
| P 帧 | NACK 补一个包 | 1 个 RTT（10-30ms） |
| 关键帧 | 后面整个 GOP 都解不出来 | 等下一个 IDR = 60 帧 = **1 秒** |

而关键帧恰好是最大的帧（720p 下 30+ 分片，P 帧 1-2 分片），
分片越多丢一个的概率越高。**最脆弱的帧拿到同样的保护** —— 这是个结构性错配。

### 二项模型对关键帧系统性欠保护

`binomial_cdf(k, p, r)` = `P(丢失数 <= r)`，看起来正是「能救回来」的概率。
但 FEC 高斯消元能解出全部缺失，还要求「丢失包的系数向量线性无关」，后者严格更严：

```text
k=35 片，5% 丢包
  二项模型说 r=9 够 99.9%
  真实 FecDecoder 只有 89.5%     ← 差 4 倍
```

所以关键帧必须换**精确模型** `exact_recovery_probability(k, r, p)`。
`tests/fec_keyframe.rs::exact_model_matches_real_decoder` 用真实 `FecSender`/`FecReceiver`
跑 6 组对照，偏差 < 0.2pp（测试容差 3pp）。

### 两个目标值的实测依据

**`KEYFRAME_RECOVERY_TARGET = 0.999`**：

| 丢包率 | target=0.99 | target=0.999 | 倍数 |
| --- | --- | --- | --- |
| 0.5% | r=2 (6%) | r=11 (31%) | 5.5x |
| 1% | r=6 (17%) | r=26 (74%) | 4.3x |
| 2% | r=15 (43%) | r=33 (94%) | 2.2x |
| 3% | r=25 (71%) | r=35 (100%) | 1.4x |

- 不选 0.995：1% 丢包只要 r=10，等于每 200 秒卡一次，用户能感知。
- 不选 0.9999：k=35/p=5% 时即使 100% 开销的天花板也只有 99.65%，物理不可达。

**`KEYFRAME_MAX_OVERHEAD_RATIO = 0.5`**：

| 上限 | r | @5% | @2% | 码率增量 |
| --- | --- | --- | --- | --- |
| 0% | 0 | 16.61% | 49.31% | +0.0% |
| 10% | 3 | 69.05% | 93.51% | +3.6% |
| 30% | 10 | 89.62% | 98.25% | +10.7% |
| **50%** | **17** | **95.08%** | **99.24%** | **+17.9%** |
| 100% | 35 | 99.65% | 99.99% | +35.8% |

拐点在 40–50%：0→50% 换 +78pp，50%→100% 只再换 +4.6pp 却多付一倍带宽。

### API

```rust
pub struct FrameClass { pub is_keyframe: bool, pub fragments: usize }
impl FrameClass {
    pub fn keyframe(fragments: usize) -> Self;
    pub fn delta(fragments: usize) -> Self;
}

/// 发送端该调这个
pub fn suggest_redundant_for_frame(class: FrameClass, loss_rate: f64, target: f64) -> usize;
pub fn exact_recovery_probability(k: usize, r: usize, p: f64) -> f64;
pub fn suggest_redundant_exact(k: usize, loss_rate: f64, target: f64) -> usize;
pub fn clamp_keyframe_redundant(fragments: usize, want: usize) -> usize;
pub const KEYFRAME_RECOVERY_TARGET: f64 = 0.999;
pub const KEYFRAME_MAX_OVERHEAD_RATIO: f64 = 0.5;
```

P 帧分支**逐点等同**原来的 `suggest_redundant`（有测试守着），
所以接入不改 P 帧行为，只改关键帧。

### 一个非单调 bug（概率模型专属）

第一版在 `r >= k` 时短路成下界 `q^k(1+p)^k`：

```text
k=4, p=1%:  r=3 -> 99.99%    r=4 -> 99.96%   ← 冗余越多概率越低
```

真实值 r=4 是 100.0000%（丢 1 个包，3 个冗余包任一个都能救）。下界把多重冗余丢了。
**概率模型的短路优化必须先证明它保持单调。**

### 接入时的真 bug：用错了分片数

第一版 `decide_redundant(is_keyframe)` 内部仍用 `self.last_fragments`（上一帧）：

```text
P 帧 P 帧 P 帧 ... P 帧  [关键帧]
 2片  2片  2片      2片   30+片
                          ↑ 此刻 last_fragments 还是 2

affordable_redundant(2, want) = floor(2 * 0.35) = 0
→ 关键帧拿到 0 个冗余
```

实测：`fec_skipped_frames=344`、`fec_skip_overhead=344`、`fec_loss_avg_frags=1.9`、`fec_redundant=1`。
修法：`send_frame` 有 `datagrams.len()`，直接传。删掉 `last_fragments` 字段（它本身就是 bug 温床）。

### A/B 实测（720p @15% 丢包，各 20 秒）

| 指标 | 固定 r=2 | 关键帧感知 | 变化 |
| --- | --- | --- | --- |
| `fec_redundant` | 2 | **11** | 5.5x |
| `fec_recovered` | 2 | **11** | 5.5x |
| `fec_reds` | 2 | 18 | 9x |
| `nack_sent` | 316 | 299 | -5% |
| `healed` | 152 | 165 | +9% |
| `dropped` | 229 | 226 | -1.3% |
| `video_decoded` | 875 | 888 | +1.5% |

**如实记录：整体 `dropped` 只降 1.3%。** 关键帧只占 1.8%（`kf_ratio=0.018`），
剩下 98% 的丢包都在 P 帧上，而 P 帧策略刻意没改。
能观测到的实质改善是 `fec_recovered` 2 → 11：关键帧真的被救回来了。
换算成用户感知：20 秒里从「卡 2 秒」变成「卡 0.2 秒」。

### 测试

| 位置 | 数量 | 内容 |
| --- | --- | --- |
| `crates/sidelens-core/tests/fec_keyframe.rs` | 21 | 单调性、精确 vs 二项、端到端一致性、真实 1160B 载荷逐字节 |
| `apps/sidelenssend/src/main.rs` | 3 | 关键帧 >= P 帧、预算仍生效、分片数回归 |

### 已知缺口

- P 帧仍用二项模型。P 帧只有 2 片时两者差距很小（k=2/p=5%：二项 r=1 够，真实 99.28%），
  没有改的必要；但**大 P 帧**（H.264 I 帧之外的场景）值得重新评估。
- `suggest_redundant_for_frame` 的 `target` 参数对关键帧被 `max(0.999)` 吃掉，
  传什么都不影响关键帧。这是刻意的（关键帧目标不该被调用方降低），但接口上有点意外。
