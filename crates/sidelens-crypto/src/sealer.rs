//! 帧级加密与认证
//!
//! # 加密在协议里的位置
//!
//! ```text
//! 明文帧 = [32B 帧头][载荷]
//!                ↓ seal_frame
//! 密文帧 = [32B 帧头][密文载荷][16B 认证标签]
//!          └──── AAD ────┘
//! ```
//!
//! 帧头**保持明文**。它承载 `seq` / `stream_id` / `flags`，是重组、
//! NACK、路由的依据 —— 加密了这些字段，接收端在解密前就不知道该把包
//! 放进哪个流、要不要请求重传。但它们作为 AAD 参与认证：看得见，改不了。
//!
//! # 三个必须想清楚的点
//!
//! ## 1. `payload_len` 也要进 AAD，而它在帧头里
//!
//! 这是本模块唯一需要**改写帧头**的地方：加密后载荷变长 16 字节，
//! `payload_len` 字段（偏移 28）必须同步更新。它同时参与 AAD，
//! 所以顺序是「先按最终长度写帧头 → 再用最终帧头当 AAD 加密」。
//!
//! 反过来做（先加密再改长度）会让 AAD 与实际帧头不一致，接收端
//! 必然认证失败 —— 而且错误是 `AuthenticationFailed`，看起来像
//! 「有人攻击」，排查方向会被带偏。
//!
//! ## 2. 重传必须复用密文
//!
//! 见 crate 文档。本实现的 nonce 完全由 `(方向, stream_id, seq)` 决定，
//! 所以同一个 `seq` 加密两次会产出**相同**的密文 —— 这在密码学上是
//! 安全的（nonce 没变，明文没变），但**强烈依赖调用方保证明文也没变**。
//!
//! 因此 `seal_frame` 记录已封过的 `seq`，重复封同一个 `seq` 直接报错。
//! 把「不该发生的事」变成显式失败，而不是静默的 nonce 重用。
//!
//! ## 3. 解密失败不能区分原因
//!
//! 认证失败可能是因为：密钥不对、AAD 被改、密文被改。
//! **对外只报一个错误**。区分它们等于给攻击者一个 oracle。
//! 重放不在其中 —— 它在认证**之前**由 [`crate::replay`] 拦下，
//! 因为重放检测不需要密钥。

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use zeroize::Zeroize;

use crate::handshake::Role;
use crate::nonce;
use crate::TAG_SIZE;

/// 协议帧头长度
const HEADER_SIZE: usize = sidelens_proto::HEADER_SIZE;

/// 加密或解密失败
///
/// 故意做成单一认证变体：区分失败原因会给攻击者提供 oracle。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// 认证失败（密钥错 / AAD 被改 / 密文被改，三者不可区分）
    AuthenticationFailed,
    /// 帧太短，连帧头都不完整
    FrameTooShort,
    /// 帧头 magic 不对
    BadMagic,
    /// 同一个 seq 被封装了两次
    ///
    /// 这不是攻击，是**调用方的 bug**：说明重传路径重新加密了。
    /// 后果是 nonce 重用 —— ChaCha20 下这会让两份明文的关系可被推导。
    NonceReuse(u32),
    /// 载荷长度字段与实际不符
    LengthMismatch,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::AuthenticationFailed => f.write_str("认证失败"),
            CryptoError::FrameTooShort => f.write_str("帧长度不足"),
            CryptoError::BadMagic => f.write_str("帧头 magic 不正确"),
            CryptoError::NonceReuse(seq) => {
                write!(f, "seq={seq} 被重复加密（重传路径不应重新加密）")
            }
            CryptoError::LengthMismatch => f.write_str("载荷长度字段与实际长度不符"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// 一个方向的收发密钥
///
/// `send` 用于加密本端发出的帧，`recv` 用于解密收到的帧。
/// 两端构造时自动交叉（发起方的 send = 响应方的 recv）。
pub struct TrafficKeys {
    pub(crate) send: [u8; 32],
    pub(crate) recv: [u8; 32],
}

impl std::fmt::Debug for TrafficKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TrafficKeys { <已隐藏> }")
    }
}

impl Drop for TrafficKeys {
    fn drop(&mut self) {
        self.send.zeroize();
        self.recv.zeroize();
    }
}

impl TrafficKeys {
    /// 从两把裸密钥构造（测试与自定义密钥交换用）
    ///
    /// 生产路径应走 [`crate::handshake::SessionKeys::traffic_keys`]。
    pub fn from_raw(send: [u8; 32], recv: [u8; 32]) -> Self {
        Self { send, recv }
    }

    /// 从一把 32 字节主密钥派生收发密钥
    ///
    /// # 用途
    ///
    /// 生产路径走 [`crate::handshake::SessionKeys::traffic_keys`]（X25519 协商）。
    /// 这个函数是给**预共享密钥**场景用的：命令行传一个 64 位十六进制串，
    /// 两端用同一把主密钥，省掉握手往返。
    ///
    /// 用途很具体：在没有 PKI、也不方便做 SAS 核对的开发/内网环境里，
    /// 先验证「加密后整条链路仍然工作」，而不是等握手协议写完才能测。
    ///
    /// # 为什么不能直接把主密钥当密钥用
    ///
    /// 两个方向必须用**不同**的密钥。共用一把时，同一条连接上
    /// 「A 发给 B 的 seq=5」与「B 发给 A 的 seq=5」的 `(key, nonce)` 只靠
    /// nonce 的方向位区分 —— 那是纵深防御，不该是唯一防线。用 HKDF 派生
    /// 两把互相独立的密钥，即使 nonce 拼法将来改错了，也不会立刻灾难。
    ///
    /// # 两端的 role 必须互补
    ///
    /// 发起方拿 `(k_i2r, k_r2i)`，响应方拿 `(k_r2i, k_i2r)`。
    /// 两端给同一个 role 时密钥会交叉，表现为「每一帧都认证失败」。
    pub fn from_master(master: &[u8; 32], role: Role) -> Self {
        use hkdf::Hkdf;
        use sha2::Sha256;

        let hk = Hkdf::<Sha256>::new(Some(b"sidelens-psk-v1"), master);
        let mut k_i2r = [0u8; 32];
        let mut k_r2i = [0u8; 32];
        // 32 字节远小于 HKDF 的输出上限（255 * 32），expect 不会触发
        hk.expand(b"initiator to responder", &mut k_i2r)
            .expect("HKDF 输出长度合法");
        hk.expand(b"responder to initiator", &mut k_r2i)
            .expect("HKDF 输出长度合法");

        if role.is_initiator() {
            Self {
                send: k_i2r,
                recv: k_r2i,
            }
        } else {
            Self {
                send: k_r2i,
                recv: k_i2r,
            }
        }
    }

    /// 密钥指纹（用于诊断与测试，**不泄露密钥**）
    ///
    /// # 为什么需要它
    ///
    /// 「两端密钥是否一致」是接错时最先要查的事，但密钥本身不能进日志。
    /// 指纹是 `SHA256(send ‖ recv)` 的前 8 字节，两端打印出来比对即可：
    ///
    /// - 相同 → 密钥一致，问题在别处（nonce 方向位、AAD 拼法）
    /// - 不同 → 密钥没对上（角色搞反、master 不同、握手没完成）
    ///
    /// 8 字节（64 位）碰撞概率对「人工比对两个值」这个用途完全够用。
    ///
    /// # 为什么不用完整的 32 字节哈希
    ///
    /// 日志里 32 字节十六进制串没人会真的逐字比对。8 字节 = 16 个十六进制
    /// 字符，肉眼能对齐。
    pub fn fingerprint(&self) -> [u8; 8] {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(self.send);
        h.update(self.recv);
        let full = h.finalize();
        let mut out = [0u8; 8];
        out.copy_from_slice(&full[..8]);
        out
    }

    /// 指纹的十六进制表示，便于日志与比对
    pub fn fingerprint_hex(&self) -> String {
        self.fingerprint()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// 两个方向的密钥是否相同（**正常情况必须为 false**）
    ///
    /// 这个检查不该出现在生产路径上（构造时就该保证），
    /// 但作为测试与自检的一个显式断言点很便宜。
    pub fn directions_are_distinct(&self) -> bool {
        self.send != self.recv
    }
}

/// 一帧的路由元数据
///
/// # 为什么需要它
///
/// AAD 是**完整的 32 字节帧头**，而接收端从重组器拿到的只有「帧头之后的
/// 载荷」+ 几个拆开的字段（`InboundFrame` 就是这样）。要把 AAD 拼回去，
/// 必须能按协议逐字段重建帧头 —— 这件事只能有一个出处，否则字段顺序、
/// 大小端、`payload_len` 的语义一旦改一处忘另一处，就是「每帧都认证失败」。
///
/// 所以这个结构体是「重建 AAD 帧头」的唯一入口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHead {
    pub ty: u8,
    pub flags: u8,
    pub stream_id: u16,
    pub seq: u32,
    pub pts_us: u64,
    pub send_ts_us: u64,
}

impl FrameHead {
    /// 按协议拼出 32 字节帧头
    ///
    /// `payload_len` 是**线上长度**（密文 + 16B 标签），因为加密时 AAD 里
    /// 用的就是这个值 —— 见 [`FrameSealer::seal_frame`] 的注释。
    pub fn aad(&self, wire_payload_len: usize) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        out[..4].copy_from_slice(&sidelens_proto::MAGIC);
        out[4] = self.ty;
        out[5] = self.flags;
        out[6..8].copy_from_slice(&self.stream_id.to_le_bytes());
        out[8..12].copy_from_slice(&self.seq.to_le_bytes());
        out[12..20].copy_from_slice(&self.pts_us.to_le_bytes());
        out[20..28].copy_from_slice(&self.send_ts_us.to_le_bytes());
        out[28..32].copy_from_slice(&(wire_payload_len as u32).to_le_bytes());
        out
    }
}

/// 记住多少个已封装的 seq
///
/// 重传窗口（`RetransmitCache`）是 1 秒 / 约 30-60 帧，留 4096 个足够
/// 覆盖任何合理的重传延迟，同时内存占用固定在 32KB 量级。
const SEALED_WINDOW: usize = 4096;

/// 帧封装器 / 解封器
///
/// 同时持有收发两把密钥，因为一条连接上两个方向都要处理。
///
/// # 角色必须互补
///
/// 发起方用 `Role::Initiator`，响应方用 `Role::Responder`。
/// 两端都给同一个角色时：nonce 的方向位对不上，**每一帧都会认证失败**。
/// 这是最常见的接错方式，所以没有「默认角色」的构造函数。
pub struct FrameSealer {
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    role: Role,
    /// 已封装过的 seq（高位是 stream_id）
    ///
    /// 只保留最近若干个：一个长连接跑几小时会有几十万个 seq，
    /// 全存下来是内存泄漏。
    sealed: std::collections::VecDeque<u64>,
}

impl std::fmt::Debug for FrameSealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameSealer")
            .field("role", &self.role)
            .field("sealed_window", &self.sealed.len())
            .finish()
    }
}

impl FrameSealer {
    /// 用会话密钥 + 本端角色构造
    pub fn new(keys: TrafficKeys, role: Role) -> Self {
        Self {
            send: ChaCha20Poly1305::new(Key::from_slice(&keys.send)),
            recv: ChaCha20Poly1305::new(Key::from_slice(&keys.recv)),
            role,
            sealed: std::collections::VecDeque::with_capacity(64),
        }
    }

    /// 本端角色
    pub fn role(&self) -> Role {
        self.role
    }

    /// 原地加密一帧
    ///
    /// 输入是完整的明文帧（含 32B 帧头），输出是同一条 `Vec` 里的密文帧。
    /// 长度会增长 `TAG_SIZE` 字节。
    ///
    /// # 顺序（重要）
    ///
    /// 1. 解析明文帧头，拿到 `stream_id` / `seq`
    /// 2. 检查这个 seq 是否已经封过
    /// 3. 把 `payload_len` 改成 `原长度 + TAG_SIZE`
    /// 4. **用改好的帧头当 AAD**，加密载荷并追加标签
    pub fn seal_frame(&mut self, frame: &mut Vec<u8>) -> Result<(), CryptoError> {
        if frame.len() < HEADER_SIZE {
            return Err(CryptoError::FrameTooShort);
        }
        if frame[..4] != sidelens_proto::MAGIC {
            return Err(CryptoError::BadMagic);
        }

        let stream_id = u16::from_le_bytes([frame[6], frame[7]]);
        let seq = u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]);

        let key = (stream_id as u64) << 32 | seq as u64;
        if self.sealed.contains(&key) {
            return Err(CryptoError::NonceReuse(seq));
        }

        let old_len = u32::from_le_bytes([frame[28], frame[29], frame[30], frame[31]]) as usize;
        if HEADER_SIZE + old_len != frame.len() {
            return Err(CryptoError::LengthMismatch);
        }

        // 先把长度字段改成最终值，再拿整个帧头当 AAD —— 顺序反了接收端必然失败
        let new_len = (old_len + TAG_SIZE) as u32;
        frame[28..32].copy_from_slice(&new_len.to_le_bytes());

        // AAD 是帧头的 32 字节。载荷单独切出来原地加密。
        let mut payload = frame.split_off(HEADER_SIZE);
        let raw_nonce = nonce::build(self.role, stream_id, seq);
        let n = Nonce::from_slice(&raw_nonce);

        // 用 detached 版本是为了让「密文长度 = 明文长度」这件事显式可见：
        // 载荷在加密后长度不变，多出来的是 16 字节标签。
        let tag = self
            .send
            .encrypt_in_place_detached(n, frame.as_slice(), &mut payload)
            .map_err(|_| CryptoError::AuthenticationFailed)?;
        payload.extend_from_slice(tag.as_slice());

        frame.append(&mut payload);

        // 记录已封装
        if self.sealed.len() == SEALED_WINDOW {
            self.sealed.pop_front();
        }
        self.sealed.push_back(key);

        Ok(())
    }

    /// 原地解密一帧
    ///
    /// 输入是密文帧，输出是同一条 `Vec` 里的明文帧。长度会减少 `TAG_SIZE`。
    pub fn open_frame(&mut self, frame: &mut Vec<u8>) -> Result<(), CryptoError> {
        if frame.len() < HEADER_SIZE + TAG_SIZE {
            return Err(CryptoError::FrameTooShort);
        }
        if frame[..4] != sidelens_proto::MAGIC {
            return Err(CryptoError::BadMagic);
        }

        let stream_id = u16::from_le_bytes([frame[6], frame[7]]);
        let seq = u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]);

        // 线上长度 = 明文长度 + TAG_SIZE，加密时就是按这个值算 AAD 的，
        // 所以这里**不要改**帧头，直接拿它当 AAD。
        let wire_len = u32::from_le_bytes([frame[28], frame[29], frame[30], frame[31]]) as usize;
        if HEADER_SIZE + wire_len != frame.len() {
            return Err(CryptoError::LengthMismatch);
        }
        if wire_len < TAG_SIZE {
            return Err(CryptoError::FrameTooShort);
        }

        let plain_len = (wire_len - TAG_SIZE) as u32;

        let mut payload = frame.split_off(HEADER_SIZE);
        let tag_at = payload.len() - TAG_SIZE;
        let tag = Tag::clone_from_slice(&payload[tag_at..]);
        payload.truncate(tag_at);

        // 对端加密时用的角色：本端角色取反
        let raw_nonce = nonce::build(self.role.peer(), stream_id, seq);
        let n = Nonce::from_slice(&raw_nonce);

        self.recv
            .decrypt_in_place_detached(n, frame.as_slice(), &mut payload, &tag)
            .map_err(|_| CryptoError::AuthenticationFailed)?;

        // 认证通过后才把帧头恢复成明文长度
        frame[28..32].copy_from_slice(&plain_len.to_le_bytes());
        frame.append(&mut payload);

        Ok(())
    }

    /// 解封后的明文长度（不做认证，仅用于预分配）
    ///
    /// 只读帧头。**不要**用它替代真正的认证 —— 这个值在认证前是不可信的。
    pub fn plain_len_of(frame: &[u8]) -> Option<usize> {
        if frame.len() < HEADER_SIZE {
            return None;
        }
        let wire_len = u32::from_le_bytes([frame[28], frame[29], frame[30], frame[31]]) as usize;
        wire_len.checked_sub(TAG_SIZE)
    }

    /// 原地加密**载荷**（帧头由调用方以 [`FrameHead`] 提供）
    ///
    /// # 与 [`Self::seal_frame`] 的分工
    ///
    /// `seal_frame` 要一整条完整帧（含 32B 头），适合「帧刚产生」的路径 ——
    /// 它顺手把 `payload_len` 改好，零额外拷贝。
    ///
    /// 这个版本给「只有载荷」的路径用（重传缓存取出来的字节、单元测试、
    /// 以及任何不持有完整帧的调用点）。两者的 AAD 必须逐字节一致，
    /// 否则会出现「发送端说成功、接收端每帧都认证失败」这种最难查的组合。
    pub fn seal_payload(
        &mut self,
        head: &FrameHead,
        payload: &mut Vec<u8>,
    ) -> Result<(), CryptoError> {
        let key = (head.stream_id as u64) << 32 | head.seq as u64;
        if self.sealed.contains(&key) {
            return Err(CryptoError::NonceReuse(head.seq));
        }

        let wire_len = payload.len() + TAG_SIZE;
        let aad = head.aad(wire_len);
        let raw_nonce = nonce::build(self.role, head.stream_id, head.seq);
        let n = Nonce::from_slice(&raw_nonce);

        let tag = self
            .send
            .encrypt_in_place_detached(n, &aad, payload)
            .map_err(|_| CryptoError::AuthenticationFailed)?;
        payload.extend_from_slice(tag.as_slice());

        if self.sealed.len() == SEALED_WINDOW {
            self.sealed.pop_front();
        }
        self.sealed.push_back(key);

        Ok(())
    }

    /// 原地解密**载荷**
    ///
    /// 输入是线上载荷（密文 + 16B 标签），输出是明文载荷。
    pub fn open_payload(
        &mut self,
        head: &FrameHead,
        payload: &mut Vec<u8>,
    ) -> Result<(), CryptoError> {
        let wire_len = payload.len();
        if wire_len < TAG_SIZE {
            return Err(CryptoError::FrameTooShort);
        }

        // AAD 必须与加密时逐字节一致：wire_len 就是加密时写进帧头的值
        let aad = head.aad(wire_len);
        let raw_nonce = nonce::build(self.role.peer(), head.stream_id, head.seq);
        let n = Nonce::from_slice(&raw_nonce);

        let tag_at = wire_len - TAG_SIZE;
        let tag = Tag::clone_from_slice(&payload[tag_at..]);
        payload.truncate(tag_at);

        self.recv
            .decrypt_in_place_detached(n, &aad, payload, &tag)
            .map_err(|_| CryptoError::AuthenticationFailed)
    }
}
