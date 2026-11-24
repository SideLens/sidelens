# ADR 0004：Rust 核心重写完成，JS 版转为基础参照

- 状态：已采纳
- 日期：2026-11-24
- 取代：[ADR 0002](0002-js-demo-first.md) 的「临时决策」部分

## 决策

`crates/sidelens-proto` 与 `crates/sidelens-core` 已完成 JS 版的等价重写，
全部测试通过。JS 版（`packages/`）**保留但不再演进**，作为行为参照与 golden 向量来源。

## 对应关系

| JS 文件 | Rust 文件 | 测试 |
| --- | --- | --- |
| `packages/proto/src/index.js` | `crates/sidelens-proto/src/{frame,decoder,clock}.rs` | 7 + 3 |
| `packages/core/src/jitter.js` | `crates/sidelens-core/src/jitter.rs` | 6 |
| `packages/core/src/clock.js` | `crates/sidelens-core/src/clock.rs` | 6 |

## 关键设计差异（不是逐行翻译）

### 1. 解码器：从「返回切片」改成「元数据 + 区间」

JS 里 `FrameDecoder.push()` 直接返回 `{payload: Buffer}`。Rust 里如果返回 `&[u8]`，
解码器会在帧的生命周期内被永久借用，后续 `push` 全都编译不过。

改法是拆两阶段：

```rust
dec.push(&bytes);              // 只解析元数据
for m in dec.frames() {        // 不可变借用
    let payload = dec.payload(*m);  // 零拷贝取载荷
}
```

全程仍然零拷贝，只是把「借用」摊开成两次短的。

### 2. 队列：`VecDeque` + 线性插入，而不是「push 后整体排序」

JS 版每帧都 `queue.sort()`，O(n log n)。Rust 版按 PTS 找插入点，O(n)。
队列通常只有 1–3 帧，线性插入实测更快。

### 3. 缓冲区压缩：延迟到 64KB 才搬移

JS 版用 `subarray` 天然零拷贝。Rust 的 `Vec` 需要显式管理消费偏移，
否则每帧 `drain` 都是 O(n) 搬移。阈值设在 64KB，压缩时同步平移已解析帧的区间。

### 4. 时间戳：`u64` 微秒，不是 `Duration`

保留与 JS 的 `BigInt` 一致的语义（Unix 纪元微秒），便于做 golden 向量比对。

## 行为对齐的保障：golden 向量

不是靠文档互相对照，而是**字节级对齐**。
`crates/sidelens-proto/tests/golden.rs` 里的 7 组 hex 由 JS 的 `encodeFrame()` 生成
（见 `tools/gen-golden.mjs`）。任何一方改了协议布局，测试立刻失败。

## 踩到的坑

| 坑 | 现象 | 根因 | 修法 |
| --- | --- | --- | --- |
| 借用冲突 | `cannot assign to self.resync_skipped because it is borrowed` | `push_into` 返回 `&'a [u8]` 让 `self` 被永久借用 | 改为「元数据 + 区间」两阶段 |
| 空集合 `min()` | 返回 `f64::MAX` | `fold(INFINITY, min).min(f64::MAX)` 的链式写法在空集合上返回 MAX | 先判空 |
| 文档示例不编译 | `cannot find value bytes` | doc-test 里用了未定义的变量 | 补全 `use` 与构造 |
| 基准测出 138 GB/s | 超过物理内存带宽 | LTO 把 memcpy 消除了 | 每轮真实读取产出并累加 |
| 8KB 全进 L2 | 同样的虚高数字 | 固定尺寸只测到缓存带宽 | 改为按载荷大小扫描 |

## 性能（`cargo run --release --example bench`）

| 载荷 | 编码 ns/帧 | 解码 ns/帧 |
| --- | --- | --- |
| 0 B（纯头） | 5 | 7 |
| 1 KB | 11 | 20 |
| 8 KB（真实帧） | 56 | 94 |

按 8KB / 60fps 算，协议开销约占单帧预算的 **0.34%**（94ns / 16.7ms），可以忽略。
