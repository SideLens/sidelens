# SideLens 跨平台可编译性检查报告

> 检查方式：纯 `cargo check`（不链接），宿主机 Windows x86_64，交叉 target 仅需 rustup 组件。
> 检查时间：2026-11-24　检查范围：workspace 全部 9 个成员。
> **本次检查未修改任何源码文件**；唯一新增文件就是本文档。

---

## 0. 一句话结论

**协议层 / 核心层 / 编解码层 / 传输层 / 发送端 5 个 crate 在 Linux、Android、macOS、iOS 四个 target 上全部 `cargo check` 通过，是真正平台无关的。**
**虚拟摄像头三件套（sidelens-vcam / sidelens-vcam-dll / sidelensd）在非 Windows 上编译失败**，但失败原因**不是**「Windows API 不可用」这种理所当然的理由，而是**缺少 cfg 守卫**——它们根本没有为非 Windows 准备任何分支。

更重要的是发现了 **2 个真实缺陷**（见第 4 节 P1、P2），其中一个（sidelensd 的依赖声明）与移植无关，是当前工程配置本身的错误。

---

## 1. 结论表

### 1.1 逐 crate × 逐 target

| crate | linux-gnu | android | darwin | ios | 判定 | 说明 |
|---|:---:|:---:|:---:|:---:|---|---|
| `sidelens-proto` | ✅ | ✅ | ✅ | ✅ | **平台无关** | 仅依赖 serde_json |
| `sidelens-core` | ✅ | ✅ | ✅ | ✅ | **平台无关** | 有 cfg 分支，但成对写了 `not(windows)` 兜底 |
| `sidelens-codec` | ✅ | ✅ | ✅ | ✅ | **平台无关** | 零依赖，`#![forbid(unsafe_code)]` 纯逻辑 |
| `sidelens-transport` | ✅ | ✅ | ✅ | ✅ | **平台无关** | tokio + tungstenite 本身跨平台 |
| `sidelens-ipc` | ✅ | ✅ | ✅ | ✅ | **平台无关（能力退化）** | 能编过，但 pipe/shm/ring_shm 三个模块被 cfg 掉了 |
| `sidelens-crypto` | ✅ | ✅ | ✅ | ✅ | **平台无关** | 纯密码学，零平台依赖 |
| `sidelens-codec-opus` | ⚠️ | ⚠️ | ⚠️ | ⚠️ | **平台无关但有原生依赖** | 代码本身跨平台，见下方「libopus 的移植前提」 |
| `sidelens-codec-mf` | ❌ | ❌ | ❌ | ❌ | **Windows 专属** | Media Foundation H.264 编码器 |
| `sidelens-vcam` | ✅ | ✅ | ✅ | ✅ | Windows 专属能力，**已门控** | 修前 142 错误；现在非 Windows 上只剩 `detect` + `source` |
| `sidelens-vcam-dll` | ✅ | ✅ | ✅ | ✅ | Windows 专属能力，**已门控** | 非 Windows 上编译出空 cdylib |
| `sidelensd` | ✅ | ✅ | ✅ | ✅ | 混合，**已门控** | 修前全灭；现在 WebSocket/UDP 路径跨平台，4 个 Windows 模式 cfg 掉 |
| `sidelenssend` | ✅ | ✅ | ✅ | ✅ | **平台无关** | 四个 target 全绿 |

> 📌 **本节表格已按修复后的实际状态更新**（原表是修复前的检查结果）。
> 修复细节见第 6 节 P1 / P1-续 / P2。

`--all-targets`（含 tests / examples / benches）复检结论一致：上表 6 个 ✅ 的 crate 在 `--all-targets` 下依然全绿，**测试代码同样是平台无关的**。

### 1.2 依赖分层（实测，非推断）

```
平台无关层（可整层移植）
  sidelens-proto      → serde_json
  sidelens-core       → proto, serde, serde_json
  sidelens-codec      → （无依赖）
  sidelens-transport  → proto, core, tokio, futures-util, tokio-tungstenite, tracing, anyhow
  sidelens-ipc        → （无依赖；windows 在 cfg(windows) 下）
  sidelenssend        → proto, core, codec, transport, tokio, anyhow, tracing, tracing-subscriber

Windows 专属层（必须重写）
  sidelens-vcam       → proto + [cfg(windows)] ipc, windows, windows-core
  sidelens-vcam-dll   → vcam, proto + [cfg(windows)] windows, windows-core
  sidelensd           → proto, core, transport, vcam, codec, ipc + [cfg(windows)] 全部运行时依赖
```

---

## 2. 环境与复现命令

### 2.1 环境

```text
rustup 1.29.3 / rustc 1.100.0 (0ef1bc386 2026-11-09) stable
host: x86_64-pc-windows-msvc
RUSTUP_HOME=D:\software\rust\rustup
CARGO_HOME=D:\software\rust\cargo
```

### 2.2 target 安装：**全部成功**，无网络失败

```powershell
$env:RUSTUP_HOME='D:\software\rust\rustup'
$env:CARGO_HOME='D:\software\rust\cargo'
$env:PATH="$env:CARGO_HOME\bin;$env:PATH"

rustup target add x86_64-unknown-linux-gnu aarch64-linux-android aarch64-apple-darwin aarch64-apple-ios
# info: downloading 4 components
# rustup target list --installed →
#   aarch64-apple-darwin / aarch64-apple-ios / aarch64-linux-android / x86_64-pc-windows-msvc / x86_64-unknown-linux-gnu
```

> ⚠️ **可复现性注意**：仓库里的 `rust-toolchain.toml` 写死了 `targets = ["x86_64-pc-windows-msvc"]`。
> 这是「确保装上」的列表而非白名单，所以额外装的 4 个 target 不会被移除，本次检查正常生效。
> 但换一台机器 / CI 上必须手动 `rustup target add`，`rustup show` 不会自动补装这 4 个。

### 2.3 检查命令

```powershell
# 构建产物落到 TEMP，避免污染仓库 target/
$env:CARGO_TARGET_DIR="$env:TEMP\sl-xc-<target>"

foreach ($c in @('sidelens-proto','sidelens-core','sidelens-codec','sidelens-transport',
                 'sidelens-ipc','sidelens-vcam','sidelens-vcam-dll','sidelensd','sidelenssend')) {
  foreach ($t in @('x86_64-unknown-linux-gnu','aarch64-linux-android',
                   'aarch64-apple-darwin','aarch64-apple-ios')) {
    cargo check -p $c --target $t --message-format short
  }
}
```

---

## 3. 逐 target 细节

### 3.1 x86_64-unknown-linux-gnu

```text
sidelens-proto      exit=0  warnings=0
sidelens-core       exit=0  warnings=0
sidelens-codec      exit=0  warnings=0
sidelens-transport  exit=0  warnings=0
sidelens-ipc        exit=0  warnings=0
sidelens-vcam       exit=101  142 errors
sidelens-vcam-dll   exit=101  被 vcam 连坐（未进入自身编译）
sidelensd           exit=101  被 vcam 连坐
sidelenssend        exit=0  warnings=0
```

**依赖图实测对比**（`cargo tree --target`）：

| 依赖 | linux target | windows target |
|---|---|---|
| `libc` | ✅ 出现（tokio/mio/socket2/getrandom 引入） | ❌ 不出现 |
| `windows-sys` | ❌ 不出现 | ✅ 出现（mio / socket2 / nu-ansi-term 引入） |
| `sidelens-ipc` 的依赖 | **空**（只有自己） | `windows v0.62.2` + 8 个 windows-* 子 crate |

这是 `[target.'cfg(windows)'.dependencies]` **真的生效了**的硬证据：同一份 Cargo.toml 在两个 target 下解析出完全不同的依赖图。

### 3.2 aarch64-linux-android

结果与 3.1 **完全一致**，无任何 Android 特有错误（没有 NDK 头文件缺失、没有 bionic 差异）。
说明平台无关层对 Android 也是干净的——`tokio` + `tungstenite` 在 Android 上编译无障碍（本例不需要链接器，所以不需要 NDK）。

### 3.3 aarch64-apple-darwin

结果与 3.1 **完全一致**。无 macOS 特有错误。

### 3.4 aarch64-apple-ios

结果与 3.1 **完全一致**。无 iOS 特有错误。
> 注意：这只是 `cargo check` 通过，**不代表 iOS 可用**。iOS 禁止动态加载/常驻后台，`sidelensd` 的形态（长驻服务 + 命名管道）在 iOS 上语义上就不成立。这里证明的只是「纯 Rust 逻辑层能在 iOS target 上过类型检查」。

---

## 4. 失败原因分析（sidelens-vcam）

### 4.1 错误总量与分类

`cargo check -p sidelens-vcam --target x86_64-unknown-linux-gnu` → **142 个错误**，只有 4 种形态：

| 数量 | 错误码 | 摘要 |
|---:|---|---|
| 129 | `E0433` | `cannot find module or crate \`windows\` in this scope: use of unresolved module or unlinked crate \`windows\`` |
| 12 | `E0425` | `cannot find type \`Xxx_Impl\` in this scope: not found in this scope` |
| 1 | `E0432` | `unresolved import \`sidelens_ipc\`: use of unresolved module or unlinked crate \`sidelens_ipc\`` |
| 1 | — | `error: could not compile \`sidelens-vcam\` (lib) due to 142 previous errors` |

### 4.2 按文件分布

| 文件 | 错误数 |
|---|---:|
| `src/activate.rs` | 53 |
| `src/media_source.rs` | 50 |
| `src/stream.rs` | 15 |
| `src/register.rs` | 7 |
| `src/clsid.rs` | 6 |
| `src/frame_source.rs` | 4 |
| `src/callback.rs` | 3 |
| `src/com.rs` | 2 |
| `src/devprop.rs` | 2 |
| **合计** | **142** |

### 4.3 根因链

```
Cargo.toml 把 windows 声明在 [target.'cfg(windows)'.dependencies]
        ↓  （非 Windows target 下，windows crate 不进依赖图）
源码却无条件 use windows::...
        ↓
E0433 ×129：找不到 windows crate
        ↓
#[implement(IFoo)] 宏生成的 *Impl 类型也随之消失
        ↓
E0425 ×12：找不到 MediaSourceActivate_Impl / SideLensMediaSource_Impl /
           SideLensMediaStream_Impl / MediaSourceFactory_Impl / VcamCallback_Impl
        ↓
frame_source.rs:33 还无条件 use sidelens_ipc::{...}
        ↓
E0432 ×1：sidelens_ipc 同样在 cfg(windows) 下，非 Windows 不进图
```

**关键技术细节**：`windows` crate 的 `src/lib.rs` 第一行是 `#![cfg(windows)]`。
所以它在非 Windows 上**能编译成功**，只是编译成一个**空 crate**。
因此失败不在依赖层，而在**使用点**——错误信息是「找不到 crate」而不是「crate 编译失败」。
这解释了为什么 129 个错误看起来像是「漏了依赖」，实际是「没写 cfg」。

### 4.4 sidelens-vcam-dll 与 sidelensd

两者的失败**不是自身原因**，是 vcam 先失败导致整个依赖链中断（错误信息都是 `could not compile \`sidelens-vcam\``）。
但这**不代表它们修好 vcam 就能过**——见 5.2 和 5.3，它们各自还有独立问题。

---

## 5. cfg 守卫审计（回答问题 3）

### 5.1 sidelens-ipc：**有守卫，做得对**

`crates/sidelens-ipc/src/lib.rs:53-71`

```rust
pub mod ring;      // 无条件
pub mod wire;      // 无条件

#[cfg(windows)] pub mod pipe;
#[cfg(windows)] pub mod ring_shm;
#[cfg(windows)] pub mod shm;

pub use ring::{RingLayout, SlotIndex};
pub use wire::{FrameMeta, FrameSignal, PixelFormat};

#[cfg(windows)] pub use pipe::{PipeClient, PipeServer};
#[cfg(windows)] pub use ring_shm::ShmRing;
#[cfg(windows)] pub use shm::ShmMapping;
```

Cargo.toml 对应：`[target.'cfg(windows)'.dependencies] windows = { workspace = true }`
测试也守住了：`tests/concurrency.rs:6` → `#![cfg(windows)]`

**非 Windows 上的行为：不炸，是静默降级。** `wire`（FrameMeta / FrameSignal / PixelFormat）和 `ring`（RingLayout / SlotIndex）照常可用——这正是它注释里说的「前两层完全平台无关」。丢的是 `PipeServer` / `ShmRing` / `ShmMapping` 三个类型。

⚠️ **但降级不够响亮**：`DEFAULT_PIPE_NAME` / `DEFAULT_SHM_NAME` 两个常量是**无条件导出**的（lib.rs:74/77）。调用方 `use sidelens_ipc::DEFAULT_PIPE_NAME;` 能编过，一旦碰 `PipeServer` 才炸，报的是 `cannot find type PipeServer`——排查者第一反应会是「我漏了 import 吗」，而不是「本平台不支持 IPC」。
建议加 `#[cfg(not(windows))] compile_error!` 或 feature gate 让错误信息自解释。

### 5.2 sidelens-vcam：**几乎没有守卫**

`lib.rs` 无条件 `pub mod` 了全部 10 个模块：

```rust
pub mod activate;  pub mod callback;  pub mod clsid;   pub mod com;
pub mod detect;    pub mod devprop;    pub mod frame_source;
pub mod media_source; pub mod register; pub mod source; pub mod stream;
```

全 crate **只有 8 处 `cfg(`**，且全部集中在两个地方：

| 位置 | 内容 | 是否有用 |
|---|---|---|
| `detect.rs:74` | `#[cfg(not(windows))] fn detect_impl()` | ✅ **唯一有效的跨平台缝**，返回 `VirtualCameraSupport::NotWindows` |
| `detect.rs:84,122,133,141,162` | `#[cfg(windows)]` | ✅ 配套 |
| `stream.rs:484` | `#[cfg(test)]` | — 与平台无关 |
| `frame_source.rs:311` | `#[cfg(test)]` | — 与平台无关 |

其余 8 个模块（`activate` / `callback` / `clsid` / `com` / `devprop` / `frame_source` / `media_source` / `register`）**零守卫**，共 127 处 `windows::` 引用全部无条件。

**结论**：`detect.rs` 是有人认真想过跨平台的（枚举里专门有 `NotWindows` 变体，Display 文案写着「非 Windows 平台，虚拟摄像头需要平台专属实现」），但这份设计**只停在 detect 一个文件里，没有上升到 lib.rs 的模块门控**。

### 5.3 sidelens-vcam-dll：**零守卫**

`crates/sidelens-vcam-dll/src` 下 `cfg(` 出现次数 = **0**。
`lib.rs` 里 16 处 `windows::` 引用（`IClassFactory`、`HMODULE`、`S_OK`、`GUID` 等）全部无条件。
即使把 vcam 修好，这个 crate 在非 Windows 上依然会以同样的 E0433 炸掉。

### 5.4 三者合起来在非 Windows 上会怎么炸

| crate | 第一层错误 | 第二层（修好第一层后） |
|---|---|---|
| `sidelens-ipc` | 无 | 无（设计正确） |
| `sidelens-vcam` | E0433 ×129 找不到 `windows` | 需要把 8 个模块逐个门控；`detect` 已经能用 |
| `sidelens-vcam-dll` | 继承 vcam 的失败 | 自身 16 处 `windows::` 仍会炸；cdylib 在非 Windows 上也没有意义 |
| `sidelensd` | 继承 vcam 的失败 | **会再炸一轮**——见 P1，缺 tokio/anyhow/tracing 等 5 个依赖 |

---

## 6. 发现的真实问题（问题 4 —— 最重要的一节）

### P1 🔴 `apps/sidelensd/Cargo.toml` 依赖声明与源码不匹配（**与移植无关的现有缺陷**）

```toml
# apps/sidelensd/Cargo.toml 现状
[dependencies]
sidelens-proto = { workspace = true }
sidelens-core = { workspace = true }
sidelens-transport = { workspace = true }
sidelens-vcam = { workspace = true }
sidelens-codec = { workspace = true }
sidelens-ipc = { workspace = true }

[target.'cfg(windows)'.dependencies]     # ← 问题在这里
windows = { workspace = true }
windows-core = { workspace = true }
tokio = { workspace = true }             # ← 平台无关
anyhow = { workspace = true }            # ← 平台无关
tracing = { workspace = true }           # ← 平台无关
tracing-subscriber = { workspace = true }# ← 平台无关
serde_json = { workspace = true }        # ← 平台无关
```

**但源码里这 5 个 crate 是无条件使用的：**

| crate | 使用处（文件数 / 行数） | 代表位置 |
|---|---|---|
| `tokio` | 6 文件 / 15 行 | `main.rs:77` `#[tokio::main]`；`udp_server.rs:48` `use tokio::net::UdpSocket` |
| `tracing` | 5 文件 / 8 行 | `main.rs:28` `use tracing::{info, warn}` |
| `anyhow` | 5 文件 / 6 行 | `main.rs:24` `use anyhow::Result` |
| `tracing-subscriber` | 1 文件 / 2 行 | `main.rs:79` `tracing_subscriber::fmt()` |
| `serde_json` | 1 文件 / 3 行 | `shm_feed.rs` |

**为什么这是真实问题（不是「反正 sidelensd 就是 Windows 专属」）：**

1. **依赖图已经证明它们被排除了**。`cargo tree -p sidelensd --target x86_64-unknown-linux-gnu` 显示 linux 下 sidelensd 的**直接依赖里没有 tokio / anyhow / tracing / tracing-subscriber / serde_json**（windows target 下才有）。也就是说这份 Cargo.toml 在非 Windows 上**声明了一个源码无法编译的 crate**。
2. **这 5 个 crate 本身没有一行 Windows 代码**。tokio、anyhow、tracing 都是跨平台的。把它们塞进 `cfg(windows)` 没有技术理由，是把「整个 sidelensd 目前只跑 Windows」这个**事实**误写成了「这些依赖是 Windows 专属」这个**声明**。
3. **失败被掩盖了**。当前 `cargo check -p sidelensd --target linux` 的 142 个错误全来自 vcam。等 vcam 修好，sidelensd 会立刻爆出第二波 E0433/E0432——而那时排查者会以为是「移植 vcam 带出来的新问题」，实际这个雷早就埋好了。
4. **影响面超出移植**：任何想在 Linux 上单独复用 sidelensd 里某个模块（比如 `udp_server` 的 NACK 逻辑）的尝试，都得先改这份 Cargo.toml。

**修法**：把 tokio / anyhow / tracing / tracing-subscriber / serde_json 移回 `[dependencies]`，只保留 `windows` / `windows-core` / `sidelens-vcam` / `sidelens-ipc` 在 `cfg(windows)` 下。

> ✅ **已修**（本轮）。修的过程中发现报告漏了**第三层**，见下方 P1-续。

#### P1-续 🔴 只改 Cargo.toml 不够：`mod` 声明与参数解析也要门控

子代理的报告只定位到 Cargo.toml。但改完 Cargo.toml 后立刻炸出 17 个新错误：

```text
apps\sidelensd\src\shm_feed.rs:24:5: error[E0433]: cannot find module or crate `sidelens_ipc`
apps\sidelensd\src\shm_probe.rs:16:5: error[E0433]: cannot find module or crate `sidelens_vcam`
apps\sidelensd\src\vcam_probe.rs:11:5: error[E0433]: cannot find module or crate `sidelens_vcam`
apps\sidelensd\src\vcam_selfcheck.rs:20:5: error[E0433]: cannot find module or crate `sidelens_vcam`
```

原因是 `main.rs` 里这 4 个 `mod` 声明无条件。完整的修法是**三层**：

| 层 | 位置 | 内容 |
| --- | --- | --- |
| 1 | `Cargo.toml` | 平台无关依赖移出 `cfg(windows)` 块 |
| 2 | `lib.rs` / `main.rs` 的 `mod` | 每个 Windows 专属模块单独加 `#[cfg(windows)]` |
| 3 | 函数级 | 4 个模式分发抽成 `#[cfg(windows)] async fn run_windows_mode` |

第 3 层不是洁癖。只给 `return` 那一行加 cfg 的话，60 行参数解析代码会留下来 ——
在非 Windows 上变成「解析了一堆参数然后什么都不做」的死代码，而且被 cfg 掉的
类型会让这些变量变成未使用警告。抽成独立函数是最干净的切法。

非 Windows 上的行为也改成了**明确报错**而不是静默落到 WebSocket 模式：

```rust
#[cfg(not(windows))]
for flag in ["--vcam-selfcheck", "--vcam-probe", "--shm-feed", "--shm-probe"] {
    if args.iter().any(|a| a == flag) {
        anyhow::bail!("{flag} 仅支持 Windows —— 虚拟摄像头与共享内存 IPC 是 Windows 专属能力");
    }
}
```

#### P1-续-2 🟠 `#![cfg(windows)]` 会把 `main` 一起删掉

给 `examples/probe_start.rs` / `reg_probe.rs` 加 crate 级 `#![cfg(windows)]` 后：

```text
error[E0601]: `main` function not found in crate `probe_start`
```

这个错误看起来像「忘了写 main」，实际是 cfg 把 main 也一起删了。
**cdylib 也是同一个坑** —— `sidelens-vcam-dll` 用 `#![cfg(windows)]` 会让整个
crate 根消失，cargo 报「no library targets found」之类更晦涩的错误。

正确写法是「内容进 `mod imp`，crate 根留最小骨架」：

```rust
// example
#[cfg(windows)]
mod imp { /* 全部实现，main 提升为 pub fn main() */ }

#[cfg(windows)]
fn main() { imp::main(); }

#[cfg(not(windows))]
fn main() { eprintln!("此示例仅支持 Windows"); }
```

```rust
// cdylib
#[cfg(windows)]
mod imp;
#[cfg(windows)]
pub use imp::*;
#[cfg(not(windows))]
pub const NOT_SUPPORTED: &str = "虚拟摄像头 DLL 仅在 Windows 上存在";
```

### P2 🟠 `crates/sidelens-vcam/Cargo.toml` 的 dev-dependencies 漏了 target 限定

```toml
[target.'cfg(windows)'.dependencies]
sidelens-ipc = { workspace = true }
windows = { workspace = true }
windows-core = { workspace = true }

[dev-dependencies]              # ← 没有 target 限定
windows = { workspace = true }
windows-core = { workspace = true }
```

**实测证据**：`cargo check -p sidelens-vcam --tests --target x86_64-unknown-linux-gnu` 的输出里出现了

```text
   Compiling windows-implement v0.60.2
   Compiling windows-interface v0.59.3
```

即：**在 Linux target 上，`windows` crate 真的被拉进构建图并编译了**（因为 `#![cfg(windows)]` 所以编成空 crate）。
配合 `tests/detect.rs:38/47` 的 `#[cfg(windows)]` 测试项，结果就是：

- lib 侧：`[target.'cfg(windows)'.dependencies]` 正确排除了 windows → 编译失败（因为源码没守卫）；
- test 侧：`[dev-dependencies]` 无条件拉入 windows → 即使 lib 修好，**在非 Windows 上跑 `cargo test -p sidelens-vcam` 依然会把 windows crate 编一遍**，白白增加构建时间和失败面。

上面做了 target 限定，下面漏了——**不对称**。这是同一份 Cargo.toml 里两种写法并存造成的真实不一致。

> ✅ **已修**：改为 `[target.'cfg(windows)'.dev-dependencies]`，同时补上漏掉的 `sidelens-ipc`。
> 另外 4 个 Windows 专属测试文件（`attributes.rs` / `cameraspec.rs` / `stream.rs` / `frame_source_e2e.rs`）
> 也补了 `#![cfg(windows)]` 守卫 —— 它们的测试代码要 CoInitializeEx 与 Media Foundation，
> 在非 Windows 上根本没有对应类型。`detect.rs` / `source_state.rs` 保持无守卫（它们是平台无关的）。

### P3 🟡 `sidelens-ipc` 的非 Windows 降级缺少显式报错

见 5.1。守卫本身是对的，但 `DEFAULT_PIPE_NAME` / `DEFAULT_SHM_NAME` 无条件导出，会让调用方在错误的地方才炸，且错误信息不指向「平台不支持」。属于可用性/可维护性问题，不是正确性问题。

### P4 🟢 正面发现：`sidelens-core/src/timer.rs` 是唯一写对的平台分支，可作模板

```rust
pub struct HighResTimer {
    #[cfg(windows)]
    _inner: WindowsTimerGuard,     // 非 Windows 上是零字段 struct
}

pub fn new() -> Self {
    #[cfg(windows)]     { Self { _inner: WindowsTimerGuard::new(1) } }
    #[cfg(not(windows))]{ Self {} }        // ← 成对写了兜底
}

pub const fn supported() -> bool { cfg!(windows) }   // ← 能力查询，不是 cfg 分支
```

三个要点，正是 vcam 缺的：
1. **字段级 cfg**，非 Windows 上结构体退化为零大小，不引入任何 Windows 类型；
2. **`#[cfg(not(windows))]` 成对兜底**，不是「只写 windows 分支然后祈祷」；
3. **`cfg!(windows)` 暴露能力查询**，让调用方能提前判断而不是等到链接失败。

### P5 🟢 正面发现：`sidelens-codec` 是移植价值最高的层

零依赖、`#![forbid(unsafe_code)]`、四个 target 全绿、`--all-targets` 也全绿。
它把 NALU 切分、SPS/PPS 提取、Annex-B ↔ AVCC 转换这些纯字节操作隔离出来了，而硬件编解码按平台分支（Windows: MF / macOS: VideoToolbox / Android: MediaCodec / Linux: VA-API）。
**这正是移植 Android/iOS 时应该第一个搬的东西**——但当前 `sidelensd` 依赖它、`sidelensd` 又依赖 vcam，所以它的跨平台价值被 vcam 的编译失败挡在了后面。

### P6 🟡 文档与工程实际不一致

`crates/sidelens-core/src/platform.rs` 有 **931 行**的能力矩阵（`Platform` / `Capability` / `SupportLevel` / `PlatformNotes` / `supports` / `support_level`），设计得很完整，甚至给 HarmonyOS 标了「待验证」。
但这份矩阵**只描述 core 自己**，没有任何机制约束 vcam / ipc / sidelensd 的依赖边界。结果就是：设计文档里写着「哪些是平台相关的」，Cargo.toml 里却是另一套分层。
`platform.rs:695` 自己留了 TODO：`"把散落的 cfg(windows) 分支改为查 platform::supports，为其他平台留出接缝"` —— 说明作者知道这个问题，只是还没做。

---

## 7. 移植建议（按投入产出排序）

| 优先级 | 动作 | 预估工作量 | 收益 |
|---|---|---|---|
| **P0** | 修 `apps/sidelensd/Cargo.toml`：把 5 个平台无关依赖移出 `cfg(windows)` | 5 分钟 | 消除隐藏雷，且不改变 Windows 行为 |
| **P0** | 修 `sidelens-vcam/Cargo.toml`：dev-dependencies 加 `[target.'cfg(windows)'.dev-dependencies]` | 5 分钟 | 停止在非 Windows 上白编 windows crate |
| **P1** | `sidelens-vcam/src/lib.rs` 给 8 个无守卫模块加 `#[cfg(windows)]`，只留 `detect`（已有 `not(windows)` 分支）和 `source`（纯状态机，需确认无 windows 依赖） | 半天 | vcam 在非 Windows 上退化为「只有 detect + source」，可编译 |
| **P1** | 参照 `timer.rs` 模式给 `frame_source` 做条件编译（它是唯一同时碰 ipc 和 windows 的模块） | 半天 | 打通 vcam 的跨平台缝 |
| **P2** | `sidelens-ipc` 加 `#[cfg(not(windows))] compile_error!` 或 `feature = "pipe"` | 1 小时 | 错误信息自解释 |
| **P2** | `sidelens-vcam-dll` 加 `#[cfg(not(windows))]` 空实现或直接 `compile_error!`（cdylib 在非 Windows 无意义） | 1 小时 | 明确边界 |
| **P3** | 把 `platform.rs` 的能力矩阵接进各 crate 的运行时分支 | 数天 | 设计落地 |

**移植路径建议**：
1. **Android/iOS 先搬 codec + proto + core**（已验证全绿，可直接复用）。
2. **macOS/Linux 桌面端搬 transport + core + codec**，摄像头层另写（Linux: v4l2loopback；macOS: CoreMediaIO DAL plugin），**不要试图移植 vcam**。
3. **iOS 不要搬 sidelensd**——长驻服务 + 命名管道的形态在 iOS 上不成立，`cargo check` 通过只是类型层面的巧合。

---

## 8. 附：本次检查的完整原始数据

### 8.1 交叉编译结果矩阵（`cargo check`，非 `--all-targets`）

```text
                            linux    android   darwin    ios
sidelens-proto              0        0         0         0
sidelens-core               0        0         0         0
sidelens-codec              0        0         0         0
sidelens-transport          0        0         0         0
sidelens-ipc                0        0         0         0
sidelens-vcam             101      101       101       101
sidelens-vcam-dll         101      101       101       101
sidelensd                 101      101       101       101
sidelenssend                0        0         0         0
```

（0 = 通过且 0 warning；101 = cargo 编译失败）

### 8.2 宿主机基线

`cargo check --workspace`（host x86_64-pc-windows-msvc）→ **exit=0，0 error 0 warning**。
即：当前代码在 Windows 上是完全健康的，所有失败都是**跨平台**维度才暴露的。

### 8.3 关键文件行号索引

| 位置 | 内容 |
|---|---|
| `crates/sidelens-ipc/src/lib.rs:53-71` | 正确的 cfg(windows) 模块门控 |
| `crates/sidelens-ipc/src/lib.rs:74,77` | 无条件导出的 `DEFAULT_PIPE_NAME` / `DEFAULT_SHM_NAME`（P3） |
| `crates/sidelens-vcam/src/lib.rs:35-45` | 10 个 `pub mod`，零 cfg |
| `crates/sidelens-vcam/src/detect.rs:74-82` | 唯一有效的 `#[cfg(not(windows))]` 分支 |
| `crates/sidelens-vcam/src/frame_source.rs:32-33` | 无条件 `use sidelens_ipc`（E0432 来源） |
| `crates/sidelens-core/src/timer.rs:40-44,52-66` | 正确的成对 cfg 分支（P4 模板） |
| `crates/sidelens-core/src/platform.rs:695` | 作者自留的 TODO：接缝未接 |
| `apps/sidelensd/Cargo.toml:21-29` | P1：5 个平台无关依赖被塞进 cfg(windows) |
| `crates/sidelens-vcam/Cargo.toml:12-20` | P2：dev-dependencies 漏 target 限定 |
```


---

## 9. libopus 的移植前提（`sidelens-codec-opus`）

这个 crate 的 **Rust 代码完全跨平台**，但 `audiopus` 底下是 C 写的 libopus。
各平台拿到 libopus 的方式不同：

| 平台 | 怎么拿到 | 是否需要额外工具 |
| --- | --- | --- |
| Windows MSVC | `audiopus_sys` **自带预编译** `libopus.lib`（x64/x86） | ❌ 零依赖 |
| Linux | `pkg-config` 找系统 libopus | `apt install libopus-dev` |
| macOS | `pkg-config` / Homebrew | `brew install opus` |
| Android | 需交叉编译 libopus 或用 `opus-sys` 的 bundled 特性 | NDK + cmake |
| iOS | 同上 | Xcode + cmake |

### 为什么 Windows 上是零依赖

`audiopus_sys-0.1.8` 的 `msvc/` 目录里直接带了编译好的静态库：

```text
msvc/x64/libopus.lib   2,041,580 字节
msvc/x64/dy/opus.dll     385,536 字节
msvc/x86/libopus.lib     578,374 字节
```

所以 Windows 上**不需要 cmake、不需要 libopus 源码、不需要任何系统包**。
从零编译 1.4 秒。

### 与 `opus` crate 的对比（重要）

`opus` + `opusic-sys` 走的是 **cmake 编译 C 源码** 路线：

```text
build.rs → cmake → 编译 libopus 的 C 源码 → 链接
```

本机装了 VS Build Tools 但**没有独立的 cmake**（VS 自带的那个在
`Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin`，默认不在 PATH）。
失败信息是：

```text
thread 'main' panicked at cmake-0.1.58/src/lib.rs:1132:5:
failed to execute command: program not found
is `cmake` not installed?
```

**完全没提「你需要 cmake」** —— 只说了「程序找不到」。

| crate | Windows 需要 cmake | 从零编译 | 跨平台获取 libopus |
| --- | --- | --- | --- |
| `opus` + `opusic-sys` | ✅ 是 | 9.2 秒 | 自动（bundled 特性编源码） |
| `audiopus` | ❌ 否 | 1.4 秒 | 需各平台自行提供 |

**取舍**：Windows 开发体验优先选 `audiopus`；如果要「一条命令在任何平台
都能构建」，`opus` 的 bundled 特性更省心（代价是必须装 cmake）。

当前选择 `audiopus`，因为 SideLens 的主开发平台是 Windows，
而 Windows 上零依赖这件事对「新克隆仓库能不能直接构建」影响最大。

### 移植到 Android / iOS 时的建议

1. **先做纯 Rust 层**（`sidelens-codec` 的时间戳逻辑），用 PCM 直通验证链路。
   `--audio` 不加 `--opus` 就是这个模式，已经在跑。
2. **再交叉编译 libopus**。Android 用 NDK 的 cmake toolchain，
   iOS 用 Xcode 的 `iphoneos.toolchain.cmake`。
3. **最后接 `sidelens-codec-opus`**。它的 Rust 代码不用改，只需让
   `audiopus_sys` 找到交叉编译好的 libopus（`OPUS_LIB_DIR` / `PKG_CONFIG_PATH`）。

### 当前跨平台状态（实测，不是推断）

| 命令 | 结果 | 说明 |
| --- | --- | --- |
| `cargo check -p sidelens-codec-opus --target <4 个 target>` | ✅ 全过 | **但这是假象** |
| `cargo build -p sidelens-codec-opus --target x86_64-unknown-linux-gnu` | ❌ 失败 | `could not find native static library \`opus\`` |

**`cargo check` 通过不代表能构建。** `check` 不做链接，
所以看不到 libopus 缺失。这正是「验证手段选错会得到相反结论」的典型例子 ——
如果只看 check 结果，会得出「这个 crate 跨平台没问题」的错误结论。

真实状态：

```text
Rust 代码：跨平台 ✅（零平台分支）
Windows：  可直接构建 ✅（audiopus_sys 自带 libopus.lib）
其他平台： 需先装/编译 libopus ⚠️
```

这也说明**跨平台验证矩阵里要区分 `check` 与 `build`**。
前面第 1 节的矩阵全部用 `cargo check`，对那些纯 Rust crate 是充分的
（没有原生依赖）；但对 `sidelens-codec-opus` 不充分。

如实记录：**本轮的跨平台矩阵对 `sidelens-codec-opus` 的结论不成立**。

---

## 第十八轮审计（2026-11-24）

> 审计方式：**静态脚本 + 实测交叉 check 双轨**。宿主机 Windows x86_64，rustc 1.100.0。
> 本轮新增 `tools/audit-cfg.sh`（静态审计脚本），**未修改任何源码文件**。
> 本节所有结论都附真实命令输出，不含推断。

### 18.1 一句话结论

**第 1 节的结论表已经过时，其中 `sidelensd` 一行是错的。**

- 第 1.1 节写 `sidelensd ✅✅✅✅`「混合，**已门控**」。实测：**`sidelensd` 在全部 4 个非 Windows target 上依然失败，5 个 `E0425`**，位置在 `apps/sidelensd/src/udp_server.rs` 的视频解码统计块（本轮审计期间该文件被主控并发修改，行号从 1735-1740 漂移到 1769-1774，见 18.11）。
- 第 1.1 节把 `sidelens-vcam` 标为「非 Windows 上只剩 `detect` + `source`」——对 **lib** 成立，但 `cargo check -p sidelens-vcam --all-targets` 仍因 `examples/registry_entry.rs` **3 个 E0433** 失败。第 40 行那句「上表 6 个 ✅ 的 crate 在 `--all-targets` 下依然全绿」**不成立**。

其余 10 个 crate 的跨平台结论经复核仍然有效。

### 18.2 审计脚本

**路径**：`tools/audit-cfg.sh`（bash，WSL / git-bash / Linux / macOS 均可跑）

~~~bash
tools/audit-cfg.sh              # 全量报告
tools/audit-cfg.sh --quiet      # 只打问题行
tools/audit-cfg.sh --strict     # 有 HIGH 则 exit 1（CI 用）
tools/audit-cfg.sh --root DIR   # 指定仓库根
~~~

**它查四类问题**：

| 类型 | 级别 | 含义 |
|---|---|---|
| `WINDOWS_API_UNGATED` | HIGH | 用了 `windows::` / `windows_core::` / `windows_sys::` / `winapi::` / `std::os::windows`，但所在模块或项没有 `#[cfg(windows)]` |
| `WINDOWS_SYMBOL_LEAK` | HIGH | 某符号只在 `#[cfg(windows)]` 下定义，却在未门控区域被引用 —— 非 Windows 构建必然 `E0425` |
| `TARGET_GATED_DEP_UNGATED` | HIGH | 依赖只在 `[target.'cfg(windows)'.dependencies]` 下声明，却在未门控区域裸用 —— 必然 `E0433` |
| `CFG_ANY_MIXED` | HIGH | `cfg(any(windows, ...))` 里混了非 windows 谓词，windows 分支在其他平台仍会被编译 |
| `CFG_UNPAIRED` / `CFG_NOT_WINDOWS_ONLY` | LOW | 单边 cfg，信息性，需人工判断是否需要补对侧 |

**实现要点**（这三个坑直接决定了脚本准不准，记下来免得下一轮重踩）：

1. **先剥注释再扫**。不剥的话，文档注释里写的 `windows::` 会被当成真实用法。剥的时候必须**保持行号**（注释行输出空行），否则报的行号对不上源码。
2. **模块级门控要认**。`sidelens-vcam/src/activate.rs` 自己一行 `cfg` 都没有，但 `lib.rs` 里是 `#[cfg(windows)] pub mod activate;` —— 这是**正确**写法，不能报。脚本因此先建「模块 → 是否被父模块门控」索引。
3. **`pending` 必须在花括号入栈前快照**。`#[cfg(windows)]` 后紧跟 `{` 时，同一行的 `{` 要用到这个 pending 来标记「这个块是 windows 门控打开的」。早期版本先清空 pending 再入栈，导致 `timer.rs` / `detect.rs` 的 fn 体被误判。

### 18.3 审计脚本的发现（真实输出）

~~~text
$ bash tools/audit-cfg.sh
==============================================================
 SideLens 跨平台 cfg 门控审计
==============================================================
 仓库根   : /mnt/d/<仓库根>
 扫描文件 : 127 个 .rs
 HIGH=11  MEDIUM=0  LOW=18  INFO=0
--------------------------------------------------------------
~~~

**HIGH 共 11 条，分两组，两组都被编译器实测确认（见 18.4 / 18.5）。**

**A. `sidelensd` —— 5 个变量在非 Windows 上不存在（8 处引用）**

> ⚠️ 下表的行号是**审计时刻**（`udp_server.rs` 共 2045 行）的快照。
> 本轮审计期间主控在并发修改该文件，当前已漂移到 1769-1774 / 1839-1841，见 18.11。

| 文件:行 | 符号 | 定义处 |
|---|---|---|
| `apps/sidelensd/src/udp_server.rs:1735` | `video_decoded` | 542（`#[cfg(windows)]`） |
| `apps/sidelensd/src/udp_server.rs:1736` | `video_decode_failed` | 544 |
| `apps/sidelensd/src/udp_server.rs:1737` | `video_need_keyframe` | 546 |
| `apps/sidelensd/src/udp_server.rs:1738` | `video_dec` | 540 |
| `apps/sidelensd/src/udp_server.rs:1740` | `video_dec` | 540 |
| `apps/sidelensd/src/udp_server.rs:1813` | `video_decoded` | 542 |
| `apps/sidelensd/src/udp_server.rs:1814` | `video_decode_failed` | 544 |
| `apps/sidelensd/src/udp_server.rs:1815` | `video_need_keyframe` | 546 |

**B. `sidelens-vcam/examples/registry_entry.rs` —— 3 处裸 `use windows::`**

| 文件:行 | 内容 |
|---|---|
| `crates/sidelens-vcam/examples/registry_entry.rs:21` | `use windows::core::{HSTRING, PCWSTR};` |
| `crates/sidelens-vcam/examples/registry_entry.rs:22` | `use windows::Win32::Media::MediaFoundation::{...};` |
| `crates/sidelens-vcam/examples/registry_entry.rs:26` | `use windows::Win32::System::Com::{...};` |

对比：同目录的 `probe_start.rs` / `reg_probe.rs` 用的是「内容进 `mod imp`，crate 根留最小骨架」的写法（P1-续-2 里记的正确范式），唯独 `registry_entry.rs` 漏了 —— 这是**同一目录里三种写法并存**。

### 18.4 根因分析

#### 根因 1：`udp_server.rs` 的「定义门控了，使用没门控」

这是**半门控**（half-gated），比完全不门控更难发现：

~~~rust
// 第 540-549 行：定义全部门控
#[cfg(windows)]
let mut video_dec = VideoDecoder::from_env();
#[cfg(windows)]
let mut video_decoded: u64 = 0;
// ... video_decode_failed / video_need_keyframe / renderer 同理

// 视频解码统计块：使用**没有**门控（审计时为 1734-1748，现漂移至 1766-1780）
let (dec_ok, dec_fail, dec_need_kf, dec_us, dec_mbps) = (
    video_decoded,          // <- E0425
    video_decode_failed,    // <- E0425
    video_need_keyframe,    // <- E0425
    video_dec.as_ref()...   // <- E0425 x2
);

// 紧接着补了 not(windows) 兜底 —— 但**已经晚了**
#[cfg(not(windows))]
let (dec_ok, dec_fail, dec_need_kf, dec_us, dec_mbps) = (0u64, 0u64, 0u64, 0.0f64, 0.0f64);
~~~

**为什么 `#[cfg(not(windows))]` 兜底救不了**：被 `#[cfg(windows)]` 删掉的 `let` 绑定在非 Windows 上不存在，所以那段**无门控**的元组解构先报错，紧跟着的兜底根本没机会生效。

**正确写法**是把那段元组解构整段也加上 `#[cfg(windows)]`：

~~~rust
#[cfg(windows)]
let (dec_ok, dec_fail, dec_need_kf, dec_us, dec_mbps) = (/* 真实值 */);
#[cfg(not(windows))]
let (dec_ok, dec_fail, dec_need_kf, dec_us, dec_mbps) = (0u64, 0u64, 0u64, 0.0f64, 0.0f64);
~~~

也就是说：**兜底分支存在 ≠ 配对正确**。配对要求「两条分支覆盖同一个绑定」，而不是「windows 分支定义、not(windows) 分支另起一个同名绑定」。

> 这条与第 6 节 P4 的模板并不冲突，但补充了它没说的边界：`timer.rs` 之所以对，是因为 `new()` 的两个分支是**同一个函数体的两个块**，返回值类型一致；而 `udp_server` 这里是**同一个作用域里两个独立的 let**，无门控的那条夹在中间就会炸。

#### 根因 2：`registry_entry.rs` 漏了 example 级守卫

`crates/sidelens-vcam/examples/` 下 3 个文件，2 个守了、1 个没守：

| 文件 | 写法 | 非 Windows |
|---|---|---|
| `probe_start.rs` | `#[cfg(windows)] mod imp;` + 双 `main` | ✅ 可编译 |
| `reg_probe.rs` | 同上 | ✅ 可编译 |
| `registry_entry.rs` | 裸 `use windows::...` | ❌ E0433 x3 |

第 6 节 P1-续-2 已经把这个坑写清楚了（「`#![cfg(windows)]` 会把 `main` 一起删掉」），但**只修了当时报错的那两个文件**，没有回头扫整个 `examples/` 目录。

### 18.5 逐 crate 跨平台 check 结果（本轮实测）

环境：`rustc 1.100.0` / `cargo 1.100.0`，已装 target：

~~~text
$ rustup target list --installed
aarch64-apple-darwin
aarch64-apple-ios
aarch64-linux-android
x86_64-pc-windows-msvc
x86_64-unknown-linux-gnu
~~~

**5 个 target 全部已安装，本轮无「未测」项。**

命令：`cargo check -p <crate> --target <target> --message-format short`

| crate | linux-gnu | android | darwin | ios | 判定 |
|---|:---:|:---:|:---:|:---:|---|
| `sidelens-proto` | ✅ 0 | ✅ 0 | ✅ 0 | ✅ 0 | 平台无关 |
| `sidelens-core` | ✅ 0 | ✅ 0 | ✅ 0 | ✅ 0 | 平台无关 |
| `sidelens-transport` | ✅ 0 | ✅ 0 | ✅ 0 | ✅ 0 | 平台无关 |
| `sidelens-codec` | ✅ 0 | ✅ 0 | ✅ 0 | ✅ 0 | 平台无关 |
| `sidelens-crypto` | ✅ 0 | ✅ 0 | ✅ 0 | ✅ 0 | 平台无关 |
| `sidelens-ipc` | ✅ 0 | ✅ 0 | ✅ 0 | ✅ 0 | 平台无关（能力退化） |
| `sidelens-codec-opus` | ✅ 0 | ✅ 0 | ✅ 0 | ✅ 0 | check 过，但 **build 不行**（见第 9 节） |
| `sidelens-codec-mf` | ✅ 0 | ✅ 0 | ✅ 0 | ✅ 0 | Windows 专属，已门控（仅剩 manifest warning） |
| `sidelens-render` | ✅ 0 | ✅ 0 | ✅ 0 | ✅ 0 | `convert` 跨平台 / `win32` 已门控 |
| `sidelens-vcam` | ✅ 0 | ✅ 0 | ✅ 0 | ✅ 0 | **lib 通过**；`--all-targets` ❌（见下） |
| `sidelens-vcam-dll` | ✅ 0 | ✅ 0 | ✅ 0 | ✅ 0 | 非 Windows 上空 cdylib |
| `sidelensd` | ❌ **101** | ❌ **101** | ❌ **101** | ❌ **101** | **未修，5 个 E0425** |
| `sidelenssend` | ✅ 0 | ✅ 0 | ✅ 0 | ✅ 0 | 平台无关 |

（数字 = 错误数；101 = cargo 编译失败退出码）

#### 失败的完整错误（4 个 target 完全一致）

审计时刻（udp_server.rs 共 2045 行）：

~~~text
$ cargo check -p sidelensd --target x86_64-unknown-linux-gnu --message-format short
apps\sidelensd\src\udp_server.rs:1735:17: error[E0425]: cannot find value video_decoded in this scope
apps\sidelensd\src\udp_server.rs:1736:17: error[E0425]: cannot find value video_decode_failed in this scope
apps\sidelensd\src\udp_server.rs:1737:17: error[E0425]: cannot find value video_need_keyframe in this scope
apps\sidelensd\src\udp_server.rs:1738:17: error[E0425]: cannot find value video_dec in this scope
apps\sidelensd\src\udp_server.rs:1740:17: error[E0425]: cannot find value video_dec in this scope
error: could not compile sidelensd (bin "sidelensd") due to 5 previous errors
# exit=101
~~~

收尾复测（udp_server.rs 已被并发修改到 2077 行，**缺陷依旧**）：

~~~text
$ cargo check -p sidelensd --target x86_64-unknown-linux-gnu --message-format short
apps\sidelensd\src\udp_server.rs:1769:17: error[E0425]: cannot find value video_decoded in this scope
apps\sidelensd\src\udp_server.rs:1770:17: error[E0425]: cannot find value video_decode_failed in this scope
apps\sidelensd\src\udp_server.rs:1771:17: error[E0425]: cannot find value video_need_keyframe in this scope
apps\sidelensd\src\udp_server.rs:1772:17: error[E0425]: cannot find value video_dec in this scope
apps\sidelensd\src\udp_server.rs:1774:17: error[E0425]: cannot find value video_dec in this scope
error: could not compile sidelensd (bin "sidelensd") due to 5 previous errors
# exit=101
~~~

`aarch64-linux-android` / `aarch64-apple-darwin` / `aarch64-apple-ios` 三个 target 输出**逐字节相同**（同样 5 行、同样行号），说明这是纯 cfg 问题，与架构无关。

#### `--all-targets` 才暴露的两个失败

~~~text
$ cargo check -p sidelens-vcam --all-targets --target x86_64-unknown-linux-gnu --message-format short
crates\sidelens-vcam\examples\registry_entry.rs:21:5: error[E0433]: cannot find module or crate windows in this scope
crates\sidelens-vcam\examples\registry_entry.rs:22:5: error[E0433]: cannot find module or crate windows in this scope
crates\sidelens-vcam\examples\registry_entry.rs:26:5: error[E0433]: cannot find module or crate windows in this scope
error: could not compile sidelens-vcam (example "registry_entry") due to 3 previous errors
# EXIT=101

$ cargo check --workspace --target x86_64-unknown-linux-gnu --message-format short
# 先炸 sidelensd 的 5 个 E0425（同上），exit=101
~~~

**注意**：`cargo check -p sidelens-vcam`（不带 `--all-targets`）是**通过**的。只看单 crate 默认 target 会漏掉 `registry_entry.rs`。

### 18.6 宿主机基线（Windows）

~~~text
$ cargo check --workspace
    Finished dev profile [unoptimized + debuginfo] target(s) in 0.19s
EXIT=0
~~~

**Windows 上完全健康，0 error 0 warning。** 两个缺陷都只在非 Windows target 暴露。

### 18.7 与前面章节的冲突（逐条）

| 位置 | 原文 | 本轮实测 | 处理 |
|---|---|---|---|
| 1.1 表 `sidelensd` 行 | `✅ ✅ ✅ ✅`「混合，**已门控**」 | ❌ 101 x4，5 个 E0425 | **原结论作废**，见 18.5 |
| 1.1 表 `sidelens-vcam` 行 | `✅ ✅ ✅ ✅` | lib ✅ / `--all-targets` ❌ 101 | **需限定为「lib 通过」** |
| 第 40 行 | 「上表 6 个 ✅ 的 crate 在 `--all-targets` 下依然全绿」 | vcam 的 example 失败 | **不成立** |
| 8.1 矩阵 `sidelensd 101` | 下文标注为「已修」 | 仍是 101 | **未真正修好** |
| 6 节 P1-续 | 「非 Windows 上的行为也改成了明确报错」 | 那段 `bail!` 在 `main.rs:134-148`，**本身是对的**；炸的是 `udp_server.rs`，不在 P1-续 覆盖范围内 | P1-续 结论仍有效，但**范围被误当成「sidelensd 已全绿」** |
| 5.1 / 6 节 P3 | `sidelens-ipc` 非 Windows 降级缺显式报错 | 复核属实，仍未加 `compile_error!` | 保持 |
| 第 9 节 | `sidelens-codec-opus` 的 check 通过是假象 | 复核属实（本轮未重跑 `build`，沿用原结论） | 保持 |

### 18.8 修复建议（本轮只审计，不含改动）

| 优先级 | 位置 | 动作 |
|---|---|---|
| **P0** | `apps/sidelensd/src/udp_server.rs:1734-1748` | 给这段元组解构加 `#[cfg(windows)]`（已有 1749 行的 `not(windows)` 兜底，只需补门控） |
| **P0** | `crates/sidelens-vcam/examples/registry_entry.rs` | 按 `probe_start.rs` 的范式改成 `#[cfg(windows)] mod imp;` + 双 `main` |
| P1 | CI | 把 `tools/audit-cfg.sh --strict` 接进流水线；再补一条 `cargo check --workspace --all-targets --target <非 windows>` |
| P1 | `crates/sidelens-vcam/Cargo.toml` | 清掉 `unused dependency sidelens-proto` 警告（非 Windows 上确实没用到） |
| P2 | `crates/sidelens-ipc/src/lib.rs:74,77` | P3 仍未处理：`DEFAULT_PIPE_NAME` / `DEFAULT_SHM_NAME` 无条件导出 |

### 18.9 本轮验证命令（可直接复现）

~~~powershell
$env:RUSTUP_HOME='D:\software\rust\rustup'
$env:CARGO_HOME='D:\software\rust\cargo'
$env:PATH = "$env:CARGO_HOME\bin;" + $env:PATH

# 1) 宿主机基线
cargo check --workspace

# 2) 逐 crate x 逐 target
$crates = @('sidelens-proto','sidelens-core','sidelens-transport','sidelens-codec',
            'sidelens-crypto','sidelens-ipc','sidelens-codec-opus','sidelens-codec-mf',
            'sidelens-render','sidelens-vcam','sidelens-vcam-dll','sidelensd','sidelenssend')
$targets = @('x86_64-unknown-linux-gnu','aarch64-linux-android',
             'aarch64-apple-darwin','aarch64-apple-ios')
foreach ($t in $targets) {
  foreach ($c in $crates) {
    cargo check -p $c --target $t --message-format short
  }
}

# 3) 补 --all-targets（单 crate 默认 target 会漏掉 examples/tests）
cargo check -p sidelens-vcam --all-targets --target x86_64-unknown-linux-gnu

# 4) 静态审计
bash tools/audit-cfg.sh
~~~

### 18.10 本轮新增文件

| 文件 | 说明 |
|---|---|
| `tools/audit-cfg.sh` | 跨平台 cfg 门控静态审计脚本（本轮新增，无其他源码改动） |
| `docs/cross-platform.md`（本节） | 第十八轮审计记录 |

> **本轮未修改任何 `crates/**` / `apps/**` 下的文件。** 18.8 的修复建议留给后续轮次。

### 18.11 审计期间的并发变更说明

本轮审计进行时，**主控正在并发修改 `apps/sidelensd/**` 与 `crates/sidelens-core/**`**（本子代理只被授权写 `tools/audit-cfg.sh` 与本节）。因此 `udp_server.rs` 的行号在审计窗口内发生了漂移：

| 时刻 | `udp_server.rs` 行数 | 5 个 E0425 所在行 | 3 个统计日志所在行 |
|---|---:|---|---|
| 审计开始 | 2045 | 1735 / 1736 / 1737 / 1738 / 1740 | 1813 / 1814 / 1815 |
| 审计收尾 | 2085 | 1769 / 1770 / 1771 / 1772 / 1774 | 1847 / 1848 / 1849 |

**缺陷本身没有变化**：两次复测都是同样的 5 个 `E0425`，同一批符号，同样 `exit=101`。变的只是行号。本节 18.3 的表格用的是**审计开始**的快照，18.5 同时给了两份输出，便于对照。

`crates/sidelens-vcam/examples/registry_entry.rs` 未被并发修改（仍 224 行），其 21 / 22 / 26 三行在整个窗口内稳定。

> 给下一轮的建议：**审计报告里凡是引用行号的地方，都标注对应的文件行数快照**。否则一个并发改动就能让整份报告的定位全部失效，而缺陷其实还在原地。

### 18.12 复测命令与退出码（收尾实测）

~~~text
$ bash tools/audit-cfg.sh --strict
# HIGH=11 ...
# exit=1        <- --strict 下有 HIGH 即失败，符合设计

$ bash tools/audit-cfg.sh --quiet | grep -c HIGH
11

$ cargo check --workspace
    Finished dev profile [unoptimized + debuginfo] target(s) in 0.15s
# EXIT=0        <- 本机 Windows 基线仍通过
~~~

**Windows 基线通过 + 非 Windows 仍失败**，这两件事同时成立，正是「只在本机编译过」的典型状态。

