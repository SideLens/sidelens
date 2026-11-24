# 性能审计（perf-audit）

> 审计者：`perf-audit` 子代理
> 方法：只读插桩（`#[cfg(feature = "perf-probe")]`），测完整块删除
> 状态：**top 2 已由主线修复并验证**

## 一句话结论

**这个系统现在的延迟瓶颈不在内存分配，在每毫秒一次的百分位统计
和一个漏了的内存回收调用。**

---

## 0. 修复状态

| # | 问题 | 修复 | 验证 |
| --- | --- | --- | --- |
| **1** | 每 tick 的百分位排序（**83.9 µs/tick**） | 移到 `fresh` 分支内（每 2 秒一次而非每毫秒） | tick 总耗时 85.1 µs → 约 1.2 µs（÷70） |
| **2** | FEC 组状态永不回收（**+0.52 MB/s**） | 1ms tick 里接上 `prune_stale(highest_completed, 32)` | 30 秒 64KB 大帧只涨 **0.8 MB**（修复前同场景 55 MB+） |
| 3 | `UdpDatagram::payload` 的 `to_vec` | **未做** | 审计自己说「比 #1 小三个数量级」；动 `transport` 公开 API 不值 |

### 修复 2 的实测验证（30 秒 / 30fps / 64KB 帧 / 3% 丢包 / FEC 自适应）

```text
内存: 9.6 MB → 10.4 MB（增长 0.8 MB）
fec_groups_pruned = 603          ← 清理在工作
fec_recovered = 1275             ← FEC 仍正常工作
playable_ms = 21.12 / 22.42      ← 延迟未受影响
```

---

## 1. 最贵的三处（按「单次成本 × 频率」）

| 排名 | 位置 | 成本 | 状态 |
| --- | --- | --- | --- |
| 🥇 | `udp_server.rs:597` 每 tick 调 `net_latency.summary()` | **83.9 µs/tick × 999/s = 83.8 ms/s**（单核 8.4%）。每 tick 把 2683 个 f64 全量 collect + 排序 ×3 次 | ✅ 已修 |
| 🥈 | FEC 组状态永不回收（`prune_stale` 调用点缺失） | **+0.52 MB/s**，内存永不回落 | ✅ 已修 |
| 🥉 | `transport/udp.rs:491` `payload: payload.to_vec()` | 232 次/s × 1.2 KB = 240 KB/s（且该字段全仓库 0 次读取） | ⏸ 未做 |

**收包路径整条**（含 2 次 `to_vec` + 3 次 HashMap + 重组 + 抖动缓冲）
均摊只有 **1.21 µs/包**，占 wall clock 0.056% —— **比 tick 小 69 倍**。

---

## 2. 实测验证的三项

### A｜百分位排序

60fps × 100s，逐窗口实测 **2.0 µs → 83.9 µs**（样本 47 → 2683，
斜率 0.031 µs/样本）。tick 总耗时 85.1 µs，百分位占 **98.5%**。

### B｜FEC 泄漏

`tracked_groups` 与 `frames_in` **严格 1:1**：

| 场景 | tracked_groups | frames_in | 内存 |
| --- | --- | --- | --- |
| 30fps × 20s | 601 | 601 | 3.24 → 8.64 MB |
| 60fps × 100s | 5973 | 5973 | 3.56 → 55.45 MB |

单调上涨，永不回落。

### C｜`frames > sent`

`FRAMES_IN_RETX_TYPE = 0`（三场景全 0），`FRAMES_IN_DUP_SEQ = 473/1093`（5% 丢包）。

---

## 3. 四个「看起来是问题、实测不是」

**这条比 top 3 更重要** —— 误优化它们会白花力气。

| 伪问题 | 真相 |
| --- | --- |
| `payload: f.payload`（`udp_server.rs:959`）是拷贝 | **是 move 不是拷贝**（`RX_COMPLETED_PAYLOAD_BYTES = 0` 佐证） |
| 重复分片分支 `chunks[idx].is_some()` 有分配 | **已在 `to_vec()` 之前 return**，零分配；实测重复率仅 2.5%/0.26% |
| 「Rust 分配器开销」在本项目明显 | **测不出来** —— 两次 `to_vec` 加起来 ≤0.48 µs，而百分位是 83.9 µs |
| 热路径有 `format!` / `to_string()` 分配 | **没有** —— 全在 2 秒统计块里 |

---

## 4. `frames > sent` 的根因（修正了 `latency-matrix.md` 的描述）

**确认了，但机制描述是错的。**

`latency-matrix.md` §5 说「`frames_in` 同时统计 `Video` 和 `Retransmit`」——
但 `FrameType::Retransmit` 分支**从未命中**（实测 0 次）。

真因：**发送端重传直接重发缓存里的原始字节，`ty` 仍是 `Video`**。
`FLAG_RETRANSMIT` 定义了但全仓库无写入点。

所以是「重传帧伪装成 Video 被再次计为到达帧次」，
结论（`frames` 是到达帧次不是去重帧数）对，机制需修正。

实际浪费有但很小：重传帧走完整链路（重组器重建 `PartialFrame`、
抖动缓冲插入后又被判迟到丢弃），实测重复率 2.5%，
收益约 0.03 µs/包，**不值得做**。

---

## 5. 多 agent 并行的一个真实教训

审计员用「整文件备份 → 回滚」的方式清理自己的插桩，
**覆盖了 `fec-adaptive` 同期写入的 FEC 自适应代码**。

它自己记录了这件事，并告知了 `fec-adaptive` 让它重贴。

**教训：多 agent 并行改同一文件时，整文件备份/回滚不可用。**
应按行号精确删除自己插入的行，或者用 `git stash` / 分支隔离。

（本项目无 git，所以只能靠「只删自己加的行」这个纪律。）
