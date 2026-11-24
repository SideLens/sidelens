//! X25519 握手与密钥派生
//!
//! # 为什么是 X25519 + HKDF，而不是「直接用 ECDH 的输出当密钥」
//!
//! 裸的 ECDH 输出**不能**直接当对称密钥用：它是曲线上的一个点坐标，
//! 分布不是均匀随机的，而且不同曲线的输出长度不一样。标准做法是把
//! ECDH 输出喂进 KDF，得到定长、均匀分布的密钥材料。这里用
//! HKDF-SHA256（RFC 5869）。
//!
//! # 为什么要额外的随机数（transcript）
//!
//! 只把 ECDH 输出喂进 KDF，得到的密钥是「双方公钥的函数」。这本身没错，
//! 但少了两个随机数会带来两个问题：
//!
//! 1. **无法做 SAS**（短认证串）：SAS 必须绑定「这一次交换」，否则重放
//!    一次旧的握手消息就能让双方回到旧密钥。
//! 2. **降级不报警**：某些中间人攻击在只有公钥的 transcript 下不可检测。
//!
//! 所以双方各出一个 32 字节随机数，一起进 salt。
//!
//! # 低阶点攻击
//!
//! X25519 对**低阶公钥**（small-order point）会产生全零的共享密钥 ——
//! 攻击者发一个低阶点，双方算出的"密钥"就是已知的，加密形同虚设。
//!
//! `x25519-dalek` 的 `was_contributory()` 会检测这一点（共享密钥全零
//! 即返回 false）。本模块**必须**在派生前检查，否则整条链路静默失效。

use hkdf::Hkdf;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

/// 握手随机数长度
pub const NONCE_LEN: usize = 32;

/// 会话密钥长度（ChaCha20-Poly1305 的密钥是 32 字节）
const KEY_LEN: usize = 32;

/// HKDF 的 info：流量密钥
const INFO_TRAFFIC: &[u8] = b"sidelens-v1 traffic keys";
/// HKDF 的 info：SAS
const INFO_SAS: &[u8] = b"sidelens-v1 sas";
/// transcript 的域分隔前缀
const TRANSCRIPT_TAG: &[u8] = b"sidelens-v1 handshake";

/// 握手失败的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeError {
    /// 对端公钥是低阶点，共享密钥为全零
    ///
    /// 这不是「对端实现有 bug」，而是**有人在攻击**：攻击者只有故意发
    /// 低阶点才能触发这个条件。所以日志里要按安全事件对待。
    LowOrderPoint,
    /// HKDF 输出长度非法（理论上不会发生，32/64 都在上限内）
    KdfFailed,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::LowOrderPoint => {
                f.write_str("对端公钥是低阶点，共享密钥全零（疑似中间人攻击）")
            }
            HandshakeError::KdfFailed => f.write_str("HKDF 派生失败"),
        }
    }
}

impl std::error::Error for HandshakeError {}

/// 本端在握手里的角色
///
/// 这个枚举同时决定两件事：transcript 的拼接顺序、以及派生出的两把
/// 方向密钥里哪一把用来发。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// 发起方（手机端）
    Initiator,
    /// 响应方（电脑端）
    Responder,
}

impl Role {
    /// 是否发起方
    pub fn is_initiator(self) -> bool {
        matches!(self, Role::Initiator)
    }

    /// 对端角色
    pub fn peer(self) -> Role {
        match self {
            Role::Initiator => Role::Responder,
            Role::Responder => Role::Initiator,
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Role::Initiator => "发起方",
            Role::Responder => "响应方",
        })
    }
}

/// 对端公钥 + 对端随机数
#[derive(Debug, Clone)]
pub struct PeerPublic {
    /// 对端的 X25519 公钥
    pub key: PublicKey,
    /// 对端的 32 字节随机数
    pub nonce: [u8; NONCE_LEN],
}

impl PeerPublic {
    /// 构造
    pub fn new(key: PublicKey, nonce: [u8; NONCE_LEN]) -> Self {
        Self { key, nonce }
    }
}

/// 本端的握手状态
///
/// 一次握手只应产生一个实例，用完即弃。**不要复用**：复用意味着同一份
/// 临时密钥参与多次派生，前向保密性直接归零。
pub struct Handshake {
    secret: StaticSecret,
    public: PublicKey,
    nonce: [u8; NONCE_LEN],
}

impl std::fmt::Debug for Handshake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 私钥绝不能进日志。这里只暴露公钥与随机数。
        f.debug_struct("Handshake")
            .field("public", &self.public)
            .field("nonce", &self.nonce)
            .field("secret", &"<已隐藏>")
            .finish()
    }
}

impl Handshake {
    /// 生成一份临时密钥对 + 随机数
    ///
    /// 用 `OsRng` 而不是用户态 PRNG：临时密钥的随机性直接决定前向保密，
    /// 这里不能省成本。
    pub fn new_ephemeral() -> Self {
        use rand::RngCore;
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        Self {
            secret,
            public,
            nonce,
        }
    }

    /// 本端公钥（发给对端）
    pub fn public_key(&self) -> PublicKey {
        self.public
    }

    /// 本端随机数（发给对端）
    pub fn nonce(&self) -> [u8; NONCE_LEN] {
        self.nonce
    }

    /// 本端要发给对端的那份数据
    pub fn peer_public(&self) -> PeerPublic {
        PeerPublic::new(self.public, self.nonce)
    }

    /// 用对端公钥派生会话密钥
    ///
    /// `role` 是**本端**的角色。两端必须给出互补的角色，否则派生出的
    /// 收发密钥会反掉 —— 表现为「能加密但对方解不开」，而且两边的 SAS
    /// 会不同（SAS 里混入了角色）。
    pub fn derive(&self, peer: &PeerPublic, role: Role) -> Result<SessionKeys, HandshakeError> {
        let shared = self.secret.diffie_hellman(&peer.key);

        // 低阶点 → 共享密钥全零。必须在派生之前拦住。
        if !shared.was_contributory() {
            return Err(HandshakeError::LowOrderPoint);
        }

        let salt = transcript(&self.public, &self.nonce, &peer.key, &peer.nonce, role);

        // 一次 Extract，两次 Expand：流量密钥 64B，SAS 4B。
        // 共用同一次 Extract 是 RFC 5869 的标准用法，不同 info 保证两次
        // Expand 的输出互相独立。
        let hk = Hkdf::<Sha256>::new(Some(&salt), shared.as_bytes());

        let mut okm = [0u8; KEY_LEN * 2];
        hk.expand(INFO_TRAFFIC, &mut okm)
            .map_err(|_| HandshakeError::KdfFailed)?;

        let mut sas_bytes = [0u8; 4];
        hk.expand(INFO_SAS, &mut sas_bytes)
            .map_err(|_| HandshakeError::KdfFailed)?;

        let mut k_i2r = [0u8; KEY_LEN];
        let mut k_r2i = [0u8; KEY_LEN];
        k_i2r.copy_from_slice(&okm[..KEY_LEN]);
        k_r2i.copy_from_slice(&okm[KEY_LEN..]);
        okm.zeroize();

        // 用 4 字节取模 1000000 得到 6 位数字。
        //
        // 严格来说取模有极小的偏置（2^32 不能被 10^6 整除，偏差约 1e-4），
        // 对 SAS 的用途（防中间人，攻击者需要在用户核对前就猜中）完全
        // 可以忽略 —— 即使偏置让某个值出现概率高 0.01%，猜中率仍是 1e-6 量级。
        let sas = u32::from_be_bytes(sas_bytes) % 1_000_000;
        sas_bytes.zeroize();

        Ok(SessionKeys {
            k_i2r,
            k_r2i,
            sas,
            role,
        })
    }
}

/// 拼接 transcript 并哈希
///
/// # 为什么按角色而不是按字节序排序
///
/// 按字节序（"小的公钥在前"）也能让两端算出同一个值，但它有个隐患：
/// 同一对密钥在角色互换后会得到**不同**的 transcript —— 这恰好是我们要的，
/// 但靠排序实现这件事很隐晦，读代码的人会以为是去重优化。
///
/// 按角色拼（发起方在前、响应方在后）语义直白，且天然把角色绑进密钥：
/// 两端如果角色搞反了，SAS 必然不一致，用户核对时立刻发现。
fn transcript(
    local_key: &PublicKey,
    local_nonce: &[u8; NONCE_LEN],
    peer_key: &PublicKey,
    peer_nonce: &[u8; NONCE_LEN],
    role: Role,
) -> [u8; 32] {
    let (i_key, i_nonce, r_key, r_nonce) = if role.is_initiator() {
        (local_key, local_nonce, peer_key, peer_nonce)
    } else {
        (peer_key, peer_nonce, local_key, local_nonce)
    };

    let mut h = Sha256::new();
    h.update(TRANSCRIPT_TAG);
    h.update(i_key.as_bytes());
    h.update(i_nonce);
    h.update(r_key.as_bytes());
    h.update(r_nonce);
    h.finalize().into()
}

/// 派生出的会话密钥
///
/// 里面同时放着两个方向的密钥。`Drop` 时清零 —— 密钥留在释放后的堆内存里
/// 是常见的取证入口。
pub struct SessionKeys {
    k_i2r: [u8; KEY_LEN],
    k_r2i: [u8; KEY_LEN],
    sas: u32,
    role: Role,
}

impl std::fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 只暴露 SAS，密钥一律不打印
        f.debug_struct("SessionKeys")
            .field("sas", &format_args!("{:06}", self.sas))
            .field("role", &self.role)
            .field("keys", &"<已隐藏>")
            .finish()
    }
}

impl Drop for SessionKeys {
    fn drop(&mut self) {
        self.k_i2r.zeroize();
        self.k_r2i.zeroize();
    }
}

impl SessionKeys {
    /// 6 位 SAS（短认证串）
    ///
    /// 两端算出的值必须相同。用户在两块屏幕上各看一眼，一致才继续 ——
    /// 这是抵御中间人的唯一手段（没有 PKI 时，密钥交换本身不认证对端）。
    pub fn sas(&self) -> u32 {
        self.sas
    }

    /// 带空格的 SAS，方便人眼分组阅读：`123 456`
    pub fn sas_display(&self) -> String {
        let s = format!("{:06}", self.sas);
        format!("{} {}", &s[..3], &s[3..])
    }

    /// 按本端角色取出收发密钥
    pub fn traffic_keys(&self, role: Role) -> crate::sealer::TrafficKeys {
        let (send, recv) = if role.is_initiator() {
            (self.k_i2r, self.k_r2i)
        } else {
            (self.k_r2i, self.k_i2r)
        };
        crate::sealer::TrafficKeys { send, recv }
    }

    /// 本端角色
    pub fn role(&self) -> Role {
        self.role
    }
}
