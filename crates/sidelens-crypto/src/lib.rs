//! SideLens 加密层
//!
//! # 为什么需要它
//!
//! 协议 v0 到 v1 的帧头里**没有任何认证字段**：任何人都能伪造一个
//! `SLN0` 开头的包，改掉 `pts_us`、`seq`、`flags`，或者直接把
//! 收到的视频帧录下来重放。手机把摄像头和麦克风共享给电脑，链路跑在
//! 同一段 WiFi 上 —— 这是最容易被动嗅探的场景。
//!
//! 本模块提供三件事：
//!
//! | 层 | 解决什么 | 模块 |
//! | --- | --- | --- |
//! | 密钥协商 | 双方在没有预共享密钥时也能得到同一把会话密钥 | [`handshake`] |
//! | 帧加密与认证 | 窃听者看不到画面，篡改一个 bit 就整帧作废 | [`sealer`] |
//! | 重放防护 | 录下来的旧帧重发不会被当成新帧 | [`replay`] |
//!
//! # 设计上的三个硬约束
//!
//! ## 1. 加密必须在重传缓存**之上**
//!
//! 这是本模块最容易踩错的地方。发送端的重传路径会把**同一个 `seq`
//! 的字节原样再发一遍**。如果按「每发一次就加密一次」实现，同一个
//! `(key, nonce)` 会被用两次 —— ChaCha20 是流密码，两次异或同一个
//! 密钥流会直接泄露两份明文的异或值，这是**灾难性**的。
//!
//! 正确做法：**加密一次，缓存密文**。重传发的是同一份密文，即使被
//! 重放也只是一模一样的字节，不泄露任何新信息。
//!
//! 因此 [`sealer::FrameSealer::seal_frame`] 只应在「帧刚产生」时调用一次，
//! 之后所有重传都复用它的输出。
//!
//! ## 2. nonce 的唯一性来自「每会话一把新密钥」
//!
//! `seq` 是每路流从 0 开始的，发送端重启就回到 0。所以 nonce 不能只
//! 由 `seq` 决定 —— 否则重启后同一个 `(key, nonce)` 又出现了。
//!
//! 解法是握手时双方各出一个随机数，一起进 KDF。**新会话 = 新密钥**，
//! 于是 `seq` 归零是安全的。见 [`handshake`] 里的 `transcript`。
//!
//! ## 3. 帧头必须「可见但被认证」
//!
//! `seq` / `stream_id` / `flags` 是路由、重组、NACK 的依据，不能加密；
//! 但被篡改的后果同样严重（改 `flags` 能把 P 帧标成关键帧，改 `seq`
//! 能打乱重放窗口）。所以它们作为 **AAD** 参与认证：看得见，改不了。
//!
//! # 用法
//!
//! ```
//! use sidelens_crypto::{FrameSealer, Handshake, Role};
//! use sidelens_proto::{encode_frame_owned, FrameSpec, FrameType};
//!
//! // 两端各自产生一份临时密钥对
//! let a = Handshake::new_ephemeral();
//! let b = Handshake::new_ephemeral();
//!
//! // 交换公钥后各自算出同一组会话密钥
//! let ka = a.derive(&b.peer_public(), Role::Initiator).unwrap();
//! let kb = b.derive(&a.peer_public(), Role::Responder).unwrap();
//! assert_eq!(ka.sas(), kb.sas());   // 用户在两端屏幕上核对这 6 位数字
//!
//! // 发送端加密
//! let mut sealer = FrameSealer::new(ka.traffic_keys(Role::Initiator), Role::Initiator);
//! let mut frame = encode_frame_owned(&FrameSpec::new(FrameType::Video, b"pixels")).unwrap();
//! sealer.seal_frame(&mut frame).unwrap();
//!
//! // 接收端解密
//! let mut opener = FrameSealer::new(kb.traffic_keys(Role::Responder), Role::Responder);
//! opener.open_frame(&mut frame).unwrap();
//! ```

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod handshake;
pub mod nonce;
pub mod replay;
pub mod sealer;

pub use handshake::{Handshake, HandshakeError, PeerPublic, Role, SessionKeys};
pub use replay::{ReplayError, ReplayGuard, ReplayWindow};
pub use sealer::{CryptoError, FrameHead, FrameSealer, TrafficKeys};

/// AEAD 认证标签长度（ChaCha20-Poly1305 固定 16 字节）
pub const TAG_SIZE: usize = 16;

/// 会话密钥长度
pub const KEY_SIZE: usize = 32;

/// 单帧加密带来的额外开销（字节）
///
/// 用于带宽预算与 MTU 计算：加密后分片边界要往前收 16 字节。
pub const WIRE_OVERHEAD: usize = TAG_SIZE;
