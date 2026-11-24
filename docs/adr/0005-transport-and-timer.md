# ADR 0005：Rust 传输层与定时器分辨率

- 状态：已采纳
- 日期：2026-11-24

## 1. 传输层

### 决策

`crates/sidelens-transport` 用 **WebSocket over TCP**，但接口按 QUIC 语义设计。

### 为什么不是直接用 QUIC

阶段 0 的目标是「拿到 Rust 版的真实端到端延迟」。QUIC 需要
QUIC 库 + UDP 打洞 + 自签证书 + 多路复用调度，这些会淹没核心问题。

### 接口设计（换 QUIC 时上层不用改）

| 语义 | 现状 | QUIC 后的行为 |
| --- | --- | --- |
| `send()` | 有界队列，满则丢 | 同样丢（数据报语义） |
| 有序性 | TCP 保证 | 不保证（上层抖动缓冲已能处理） |
| 背压 | try_send 失败即丢帧 | 同 |

关键在 `client.rs` 的 `send()`：用 `try_send` 而不是 `send`，
队列满时**丢帧而非阻塞采集**。这是 QUIC 数据报的自然语义，
提前在 TCP 上模拟出来，将来切换时行为一致。

```rust
match self.tx.try_send(bytes) {
    Ok(()) => true,
    Err(TrySendError::Full(bytes)) => {
        self.stats.dropped_backpressure.fetch_add(1, Relaxed);
        false   // 通知调用方可丢弃
    }
    ...
}
```

## 2. 定时器分辨率（本项目最隐蔽的坑）

### 问题

Windows 上的 `tokio::time::interval(Duration::from_millis(1))`
**不是 1ms 触发一次，而是 15.625ms**。实测：

```text
interval( 1ms)  p50=15.619ms
interval( 2ms)  p50=15.619ms
interval( 4ms)  p50=15.623ms
interval(16ms)  p50=15.656ms
```

四个不同间隔值，实测全是 15.6ms —— 这是 Windows 的默认系统定时器分辨率（64Hz）。

### 影响量化

帧在抖动缓冲里到期后，要等下一次 tick 才能被取走，平均白等 7.8ms。
表现为：抖动缓冲报告 20ms，实际可播放 31ms。

**这个 bug 之所以难发现，是因为它只影响"出队"这一侧，
而抖动缓冲自身的队列深度统计完全正常。**

### 修复

```rust
// crates/sidelens-core/src/timer.rs
let _guard = HighResTimer::new();  // timeBeginPeriod(1)
```

效果：

```text
修复前  playable_ms = 31.16ms
修复后  playable_ms = 20.83ms   （降低 10ms）
```

### 为什么做成 RAII 守卫而不是全局调用一次

`timeBeginPeriod` 是**全系统**设置：它会让其他进程的 sleep 也变精确，
整体功耗略升。做成守卫可以在本进程退出时自动还原，不影响用户后续使用。

### 为什么这个 API 不应该有 unsafe

`timeBeginPeriod` / `timeEndPeriod` 是幂等、无副作用、失败也无害的 winmm 调用。
理论上应有安全封装，但 Windows crate 生态没有提供。
因此本 crate 的 `#![deny(unsafe_code)]` 在 timer 模块内开了唯一一处例外：

```rust
// crates/sidelens-core/src/timer.rs
#![allow(unsafe_code)]
```

## 3. 同时修复的两个 bug

### 端口 0 的语义

`WsServer::bind(0)` 让 OS 分配端口，但代码原本记录的是 `port` 变量（0），
不是 `listener.local_addr()`。测试里连接 `127.0.0.1:0` 全部失败（EADDRINUSE）。

**修法**：`let actual_port = listener.local_addr()?.port();`

### 错误类型不兼容

`Box<dyn std::error::Error + Send + Sync>` 无法通过 `?` 转换成 `anyhow::Error`
（缺 `Sized`）。统一改为返回 `anyhow::Result`，这是应用级集成 crate 的合理选择。

## 4. 实测结果

详见 [rust-latency-report.md](../rust-latency-report.md)。

| 指标 | p50 | p95 |
| --- | --- | --- |
| 网络单程 | 0.22 ms | 0.32 ms |
| 缓冲滞留 | 20.61 ms | 21.61 ms |
| 可播放 | 20.83 ms | 21.82 ms |
