# 进程间帧传输（IPC）

> 代码：[crates/sidelens-ipc](../../crates/sidelens-ipc)
> 相关：[vcam-pitfalls.md](vcam-pitfalls.md)、[latency-budget.md](latency-budget.md)

## 为什么必须有这一层

虚拟摄像头不是「我们的进程里有个摄像头」。真实架构是：

```text
手机 ──UDP/WebSocket──> sidelensd.exe ──IPC──> FrameServer (svchost.exe -k Camera)
                            │                        │
                            │                        └─> 加载 sidelens_vcam_dll.dll
                            │                            调 IMFMediaStream::RequestSample
                            └─ 解出 NV12 像素，写进共享内存
```

关键事实：**消费端（FrameServer）不在我们的进程里**。Windows 的摄像头框架规定，
任何要出现在「相机」列表里的设备，都必须由 FrameServer 这个系统服务统一代理。
它加载我们的 DLL，通过 COM 接口向我们要帧。

于是帧必须跨进程。这是架构必需，不是可选优化。

## 为什么是「管道 + 共享内存」而不是「管道传帧数据」

一帧 1080p NV12 = 1920 × 1080 × 1.5 = **3.1 MB**。60fps 就是 **187 MB/s**。

走命名管道传像素，内核要拷贝两次（写端→内核缓冲，内核缓冲→读端）。
374 MB/s 的内存带宽全烧在搬运上，而且每次拷贝都要经过内核态切换。

实测对比（同一台机器）：

| 方案 | 单帧开销 | 60fps 总开销 |
| --- | --- | --- |
| 管道传 8KB 帧 | ~18 µs | 1.1 ms/s |
| 管道传 3.1MB 帧 | ~4.2 ms | 250 ms/s ← 不可能 |
| 共享内存 3.1MB（一次 memcpy） | ~0.31 ms | 18.6 ms/s |

所以分工是：

- **共享内存**放像素，写端直接写、读端直接读，零内核拷贝
- **命名管道**只传 44 字节的帧信号（slot 下标 + 元数据）

## 内存布局

```text
┌───────────────────┬───────────────────┬─────────────────┬──────────────────┐
│ RingHeader (64 B) │ slot_seq[N] × 4 B │ meta[N] × 40 B  │ pixels[N] × cap  │
└───────────────────┴───────────────────┴─────────────────┴──────────────────┘
offset 0            64                  64+4N             64+4N+40N
```

三段分开的理由：

1. **meta 独立**：读者想「看看有没有新帧」时只碰 40 字节，不用把 3MB 的像素区拉进缓存
2. **seqlock 独立**：每个 slot 一把顺序锁，粒度细到单帧，读者之间不互相阻塞
3. **像素对齐**：像素区起点按 64 字节对齐，方便将来接 SIMD 色彩转换

### RingHeader 字段

| 偏移 | 大小 | 字段 | 说明 |
| --- | --- | --- | --- |
| 0 | 4 | magic | `0x534C4E52`（「SLNR」小端） |
| 4 | 4 | version | 当前 2（v1 无 seqlock，见下） |
| 8 | 4 | slot_count | 必须是 2 的幂 |
| 12 | 4 | slot_capacity | 每 slot 字节容量 |
| 16 | 4 | write_seq | 写者已写帧数（原子，回绕） |
| 20 | 4 | read_seq | 读者已读帧数（原子，回绕） |
| 24 | 4 | dropped | 因覆盖丢失的帧数 |
| 32 | 8 | total_written | 累计写入（u64，不回绕） |
| 40 | 8 | total_read | 累计读取（u64，不回绕） |

### FrameMeta（40 字节，跨进程 ABI）

| 偏移 | 大小 | 字段 |
| --- | --- | --- |
| 0 | 4 | version |
| 4 | 4 | seq |
| 8 | 4 | width |
| 12 | 4 | height |
| 16 | 4 | format（0=NV12, 1=BGRA, 2=YUY2） |
| 20 | 4 | keyframe |
| 24 | 8 | pts_us |
| 32 | 4 | payload_len |
| 36 | 4 | reserved |

`SIZE = 40` 是**手算常量**而不是 `size_of::<FrameMeta>()`。这样改字段忘了同步
读端时测试会立刻红，而不是悄悄错位。

## 无锁协议：为什么必须加 seqlock

### 第一版（错的）

```text
写者：                          读者：
seq = write_seq.load()          w = write_seq.load(Acquire)
slot = seq & mask               seq = w - 1; slot = seq & mask
写 pixels[slot]                 读 meta[slot]
写 meta[slot]                   读 pixels[slot]
write_seq.store(seq+1,Release)  read_seq.store(w)
```

看起来没问题：写者用 `Release` 发布，读者用 `Acquire` 获取，标准的消息传递模式。

**并发测试立刻抓到了撕裂帧**：

```text
assertion `left == right` failed: 内容与序号不一致
  left: 100
 right: 108
```

读端拿到的 meta 说 `seq=100`，但像素内容是 `seq=108` 的。

原因：读端在「读完 meta」到「读像素」之间，写端又写了 8 帧（slot_count=8），
转一圈回到同一个 slot 并把它覆盖了。**读者没有任何手段发现这件事** ——
它读 meta 时看到的是有效数据，读像素时那个 slot 已经属于另一帧。

这不是内存序问题（内存序是对的），是**协议缺失**。`Acquire`/`Release` 只能保证
「我读到的 write_seq 之前的数据是完整的」，不能保证「我读数据的这段时间里没人写」。

### 第二版（正确）：per-slot seqlock

```text
写者：                             读者：
s = slot_seq[slot].load()          s1 = slot_seq[slot].load(Acquire)
slot_seq[slot] = s+1  (奇数)       if s1 & 1 == 1 → 正在写，重试
写 pixels + meta                   拷贝 meta + pixels
slot_seq[slot] = s+2  (偶数)       s2 = slot_seq[slot].load(Acquire)
write_seq.store(seq+1, Release)    if s1 != s2 → 被覆盖，重试
                                   read_seq.store(w, Release)
```

- **奇偶是「正在写」的标志位**：奇数说明写者手上有这个 slot
- **s1 == s2 保证拷贝期间没有写者进来**：写者每次进出都改变序号，只要序号没变，中间就没有发生过写入
- **读者不加锁、不阻塞写端**：这正是实时视频需要的。宁可重试或丢帧，也绝不能让写端等 —— 写端一卡，整条链路都卡

重试上限 `MAX_READ_RETRIES = 4`：给 `read_latest` 一个确定的执行时间上界。
连续 4 次都撞上「正在写」说明写端跑得比读端快得多，这时返回 `None` 让调用方
下个节拍再来，比在这里空转更好。

### 为什么不用信号量 / 互斥量

信号量要陷入内核，一次约 1-2 µs。60fps 下每帧两次就是 120-240 µs/s 的内核时间。
听着不多，但它引入**不可预测的调度抖动** —— 内核可能把等待的线程放到就绪队列末尾。

原子操作是纯用户态，x86 上是单条 `lock` 前缀指令，亚纳秒级，且不会引起线程切换。

### 为什么读者只取最新帧

写端 60fps，读端（FrameServer）可能只以 15fps 取。这时环形缓冲会积压。
两种选择：

- 补播旧帧 → 延迟越滚越大，用户看到的是几秒前的画面
- 跳过旧帧只取最新 → 延迟恒定，画面偶尔跳一下

实时视频里**延迟恒定**远比**帧率完整**重要。所以 `read_latest` 总是取
`write_seq - 1` 那一帧，中间的统统跳过，并把 `read_seq` 一次推到 `write_seq`。

## 命名管道

### 消息模式 vs 字节模式

- **字节模式**：`ReadFile` 读到什么边界由缓冲区大小决定，可能半条消息、可能两条半
- **消息模式**（`PIPE_TYPE_MESSAGE`）：内核保留写入边界，一次 `WriteFile` = 一次 `ReadFile`

帧信号是固定 44 字节，用消息模式可以彻底免掉「攒够 44 字节再解析」的粘包状态机。
少一个状态机就少一类 bug。

客户端连上后**必须显式调 `SetNamedPipeHandleState` 切到消息读模式** ——
服务端建的是消息模式，但客户端默认是字节读模式。

### 重叠 I/O 与超时

`ReadFile` 在没有数据时会**无限阻塞**。虚拟摄像头跑在 FrameServer 的线程池里，
一个线程被卡住会拖垮整个取帧管线。

所以每次读写都带 `OVERLAPPED` + 事件 + `WaitForSingleObject` 超时。

#### 坑 1：hEvent 必须自己创建

`OVERLAPPED.hEvent = NULL` 时，内核用**文件句柄本身**当信号。
多个并发 I/O 会互相唤醒错对象 —— 表现为随机读到别人的数据。

#### 坑 2：HRESULT 不等于 Win32 错误码

`windows` crate 把 Win32 的 DWORD 错误码包成 HRESULT：

```text
HRESULT_FROM_WIN32(997) = 0x800703E5
ERROR_IO_PENDING.0      = 997
```

直接比较永远为假。这会让「挂起的重叠 I/O」被当成真失败，`accept` 直接返回错误。

```rust
fn win32_code(e: &windows::core::Error) -> u32 {
    (e.code().0 & 0xFFFF) as u32
}
```

`HRESULT_FROM_WIN32` 的构造方式保证了低 16 位就是原始码。

#### 坑 3：超时后必须取消挂起的 I/O

重叠 I/O 超时后，那个 `OVERLAPPED` 和它的事件句柄**仍然被内核引用**。
直接返回并 Drop（关闭事件句柄）的话，内核稍后完成这个 I/O 时会往已关闭的
句柄上 `SetEvent` —— 句柄被复用给别的对象就会误唤醒。典型的定时炸弹。

正确顺序：

```text
WaitForSingleObject(hEvent, ms)   → 超时
CancelIoEx(handle, &ov)           → 通知内核放弃
WaitForSingleObject(hEvent, 1000) → 等它真的放手
返回 TimedOut
```

#### 坑 4：FILE_FLAG_FIRST_PIPE_INSTANCE

同名管道已存在时，加这个标志会**立刻失败**而不是悄悄共享。
排查「连到了别人的管道」这类问题非常有用 —— 尤其当多个版本的 sidelensd
同时跑在机器上时。

## 命名空间：Local 与 Global

Windows 内核对象命名空间：

| 前缀 | 可见范围 | 谁能创建 |
| --- | --- | --- |
| （无） | 当前会话 | 任何人 |
| `Local\` | 当前会话 | 任何人 |
| `Global\` | 全机器所有会话 | 需要 SeCreateGlobalPrivilege |

麻烦在于：**FrameServer 跑在 session 0**（`svchost.exe -k Camera`），
而 `sidelensd.exe` 跑在用户会话（session 1+）。`Local\` 命名的东西两边互相看不见。

策略：**先试 `Global\`，失败退回 `Local\`**，并把实际用到的名字记下来交给对端。

- 同会话场景（用户直接跑桌面程序取流）：`Local\` 够用
- 跨会话场景：`sidelensd` 必须以服务身份运行，那时 `Global\` 自然可用

## 实测数字

| 项目 | 值 | 条件 |
| --- | --- | --- |
| 共享内存 1080p 单帧写 | ~0.31 ms | 3.1 MB memcpy |
| 管道发一条信号 | ~18 µs | 44 字节消息模式 |
| 共享内存创建（4 slot 720p） | ~0.4 ms | CreateFileMapping + MapView |
| seqlock 读重试率 | < 0.1% | 60fps 写 / 15fps 读 |
| 撕裂帧 | 0 | 300ms 压测，2000+ 帧 |

## 测试覆盖

`cargo test -p sidelens-ipc` → **50 个测试**

| 文件 | 数量 | 覆盖 |
| --- | --- | --- |
| `src/wire.rs` | 3 | ABI 大小、编解码往返、缓冲区计算 |
| `src/ring.rs` | 8 | 2 的幂取整、slot 映射、回绕、丢帧检测 |
| `src/shm.rs` | 5 | 创建/打开/越界/零长/不存在 |
| `src/pipe.rs` | 8 | 往返、消息边界、超时、首实例、重连 |
| `src/ring_shm.rs` | 13 | 布局、往返、跳过积压、seqlock 奇偶、撕裂检测 |
| `tests/concurrency.rs` | 15 | 真双线程撕裂检测、计数器一致性、突发回绕、管道+共享内存端到端 |

其中 `producer_consumer_never_tears_a_frame` 是最关键的一条：
写端把整帧填成同一个字节，读端检查「所有字节是否都等于该值」。
没有 seqlock 时它 100% 失败 —— 这条测试就是 seqlock 存在的理由。

## 与虚拟摄像头的对接

下一步（B3 卡点解决后）：

```text
sidelensd 收到帧
  → 解码出 NV12
  → ShmRing::write_frame(pixels, meta)
  → PipeServer::write_message(FrameSignal::new(slot, meta))

FrameServer 调 IMFMediaStream::RequestSample
  → PipeClient::read_message() 拿到 signal
  → 用 signal.slot 直接定位像素（不用重算，这正是信号里带 slot 的意义）
  → ShmRing::read_latest(buf)
  → 包成 IMFSample（MFCreateSample + MFCreateMemoryBuffer + CopyTo）
  → 投递 MEMediaSample 事件
```

注意最后一步的 `CopyTo`：Media Foundation 的 `IMFSample` 不认外部内存，
必须把像素拷进它自己的 `IMFMediaBuffer`。这一次拷贝省不掉 ——
但它是从共享内存拷到 MF 缓冲，比「管道→用户态→MF 缓冲」少一次。

## 版本演进策略

`HEADER_VERSION` 不匹配时 `ShmRing::open` 直接失败，不尝试兼容。

理由：共享内存布局错了会读到**别的字段的内存**，表现为随机的诡异行为
（帧尺寸错乱、像素花屏），极难排查。宁可让 DLL 明确报「请重启 sidelensd」。
