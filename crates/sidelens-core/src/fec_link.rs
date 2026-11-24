//! FEC 与 UDP 分片链路的桥接
//!
//! # 为什么需要这一层
//!
//! [`crate::fec`] 只做纯 XOR 运算，它不知道「包」在线上长什么样；
//! [`crate::fragment`] 只管把一帧切成 MTU 大小的分片，不管丢包恢复。
//! 两者要配合，中间缺一层：**决定哪些分片进同一个 FEC 组、
//! 冗余包怎么编号、恢复出来的分片怎么送回重组器**。
//!
//! # 分组策略：按帧分组，不跨帧
//!
//! 两种可选：
//!
//! | 策略 | 优点 | 缺点 |
//! | --- | --- | --- |
//! | 按帧分组 | 一帧的所有分片同时可恢复，恢复即完整 | 帧小则组小，恢复能力弱 |
//! | 跨帧分组 | 组大、恢复能力强 | 一组的最后几个分片要等后续帧，**引入额外延迟** |
//!
//! 选按帧分组。理由：实时视频最怕延迟，跨帧分组会让「本帧的冗余包」
//! 依赖「下一帧的数据包」，等于给每一帧都加一个帧间隔的延迟。
//!
//! 代价是组大小 = 帧的分片数。720p 8KB 帧约 7 个分片，
//! `redundant = 2` 时能救任意 2 个不同余类的丢失（见 `fec` 模块的能力表）。
//! 关键帧动辄 30-50 个分片，恢复能力显著更强 —— 这正好符合「关键帧更重要」的直觉。
//!
//! # 冗余包的编号
//!
//! 冗余包需要携带「属于哪个 FEC 组、是第几个冗余包」。
//! 本模块把它们编码进 8 字节的小头：
//!
//! ```text
//! 偏移 0  2B  frame_seq       帧序号（回绕）
//! 偏移 2  2B  group_in_frame  帧内第几个 FEC 组
//! 偏移 4  2B  k               第几个冗余包（0..redundant）
//! 偏移 6  2B  real_count      本组真实包数（不含补齐的空包）
//! 偏移 8  2B  reserved        保留
//! ```
//!
//! 用帧序号当组序号，接收端不需要额外的组计数器，也不会因为丢帧而错位。

#![forbid(unsafe_code)]

use crate::fec::{FecConfig, FecDecoder, FecEncoder};
use crate::fragment::SAFE_MTU_PAYLOAD;

/// 冗余包头长度
pub const FEC_HEADER_SIZE: usize = 10;

/// 一个冗余包（含头）
///
/// # 为什么组号要拆成「帧序号 + 帧内组下标」（踩坑）
///
/// 第一版只用帧序号当组号，结果一帧分片数超过 `group_size` 时，
/// 同一帧会产出**多个组**的冗余包，而它们的组号完全相同、`k` 都从 0 开始。
/// 接收端无法区分，`push_redundant` 的组切换逻辑完全失效。
///
/// 实测：`group_size=4` 的 5 分片帧产出 2 个冗余包，`k = [0, 0]`。
///
/// 现在组号是 32 位里拆开的两段：高 16 位帧序号（回绕即可），
/// 低 16 位帧内组下标。这样一帧最多 65535 个组，每组最多 65535 个冗余包，
/// 实际用量（每帧几个组、每组 1-3 个冗余）绰绰有余。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FecPacket {
    /// 帧序号（高 16 位有效）
    pub frame_seq: u16,
    /// 帧内第几个 FEC 组
    pub group_in_frame: u16,
    /// 第几个冗余包
    pub k: u16,
    /// 本组的**真实**包数（不含发送端补齐的空包）
    ///
    /// # 为什么必须传这个（第三个坑）
    ///
    /// 帧的分片数不是 `group_size` 整数倍时，发送端会把尾组补空包凑满再算冗余。
    /// 但接收端不知道「这一组原本只有几个真实包」，它只看到 `received=1`，
    /// 于是算出 `missing=3` —— 超过 `redundant` 就放弃恢复。
    ///
    /// 实测：`group_size=4` 的 5 分片帧，尾组只有 1 个真实分片，
    /// 补 3 个空包后接收端认为缺 3 个，直接不恢复。
    ///
    /// 传 `real_count` 后，接收端知道只需关心前 `real_count` 个下标。
    pub real_count: u16,
    /// 冗余数据
    pub payload: Vec<u8>,
}

impl FecPacket {
    pub fn new(
        frame_seq: u16,
        group_in_frame: u16,
        k: u16,
        real_count: u16,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            frame_seq,
            group_in_frame,
            k,
            real_count,
            payload,
        }
    }

    /// 全局唯一的组标识（帧序号 << 16 | 帧内组下标）
    pub fn group_id(&self) -> u32 {
        ((self.frame_seq as u32) << 16) | self.group_in_frame as u32
    }

    /// 编码成线格式
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FEC_HEADER_SIZE + self.payload.len());
        out.extend_from_slice(&self.frame_seq.to_le_bytes());
        out.extend_from_slice(&self.group_in_frame.to_le_bytes());
        out.extend_from_slice(&self.k.to_le_bytes());
        out.extend_from_slice(&self.real_count.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&self.payload);
        out
    }

    /// 从线格式解析。长度不足返回 None。
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < FEC_HEADER_SIZE {
            return None;
        }
        let frame_seq = u16::from_le_bytes([buf[0], buf[1]]);
        let group_in_frame = u16::from_le_bytes([buf[2], buf[3]]);
        let k = u16::from_le_bytes([buf[4], buf[5]]);
        let real_count = u16::from_le_bytes([buf[6], buf[7]]);
        Some(Self {
            frame_seq,
            group_in_frame,
            k,
            real_count,
            payload: buf[FEC_HEADER_SIZE..].to_vec(),
        })
    }
}

/// 发送端的 FEC 编码器（按帧分组）
///
/// 用法：一帧的所有分片依次 `push_fragment`，然后 `finish_frame` 取冗余包。
#[derive(Debug)]
pub struct FecSender {
    encoder: FecEncoder,
    config: FecConfig,
    /// 当前帧内已产出的组数（= 下一个组的下标）
    group_in_frame: u16,
    /// 当前帧序号
    frame_seq: u16,
    /// 当前组补齐前的真实包数
    current_real_count: u16,
    /// 统计
    pub frames_encoded: u64,
    pub redundant_sent: u64,
    pub redundant_bytes: u64,
}

impl FecSender {
    pub fn new(config: FecConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            encoder: FecEncoder::new(config)?,
            config,
            group_in_frame: 0,
            frame_seq: 0,
            current_real_count: 0,
            frames_encoded: 0,
            redundant_sent: 0,
            redundant_bytes: 0,
        })
    }

    pub fn config(&self) -> FecConfig {
        self.config
    }

    /// 开始新的一帧
    ///
    /// # 为什么必须有这个方法（踩坑）
    ///
    /// 第一版把帧序号放在 `finish_frame(frame_seq)` 里传。但组满时
    /// `push_fragment` 就已经产出冗余包了 —— 那时 `frame_seq` 还是**上一帧的**，
    /// 于是同一帧的组号前后不一致（实测：push 期产出组(0,0)，finish 期产出组(1,1)）。
    ///
    /// 现在要求调用方在推第一个分片**之前**调 `begin_frame`，
    /// 帧序号从这里开始就是正确的。
    pub fn begin_frame(&mut self, frame_seq: u32) {
        self.frame_seq = frame_seq as u16;
        self.group_in_frame = 0;
        // 上一帧可能留下未满的组（调用方没调 finish_frame），清掉
        self.encoder.reset();
    }

    /// 推入一个分片
    ///
    /// 组满时会自动产出冗余包（通过返回值给出）。
    ///
    /// # 为什么不再维护自己的 `pending` 计数器（踩坑）
    ///
    /// 第一版有个独立的 `self.pending += 1`，用来判断「这一帧有没有推过东西」。
    /// 但它与 encoder 的真实状态会脱节：组满自动 flush 之后 encoder 已清空，
    /// 而 `pending` 还留着上一帧的计数 —— 于是 `finish_frame` 以为还有半组要补，
    /// 实际 encoder 是空的，补空包循环把内存撑到 34GB。
    ///
    /// 现在统一以 `encoder.pending()` 为唯一真相，不再有第二个计数器。
    pub fn push_fragment(&mut self, data: &[u8]) -> Vec<FecPacket> {
        let full = self.encoder.push(data);
        if full {
            // 满组：全部是真实包
            self.current_real_count = self.config.group_size as u16;
            self.flush_group()
        } else {
            Vec::new()
        }
    }

    /// 结束当前帧，产出剩余冗余包
    ///
    /// 帧的分片数不是 `group_size` 整数倍时，最后一组不满。
    /// 这里**用空包补齐**再算冗余 —— 空包合法，接收端会当正常包参与 XOR。
    /// 理由见 `fec` 模块文档第 5 条：不补齐的话尾组完全不受保护。
    pub fn finish_frame(&mut self) -> Vec<FecPacket> {
        let mut out = Vec::new();

        // 补齐尾组到 group_size 再算冗余。
        //
        // # 这里踩过三个坑
        //
        // **坑 1：死循环导致 34GB 分配失败。**
        // 第一版写成 `while !encoder.is_full() { encoder.push(&[]); }`，
        // 但循环体内还调了 `take_redundant()` —— 它清空组后条件又变 false，
        // 无限补空包，`Vec` 一路涨到 34GB。
        //
        // **坑 2：独立的 pending 计数器与 encoder 脱节。**
        // 组满自动 flush 后 encoder 已清空，而计数器还留着上一帧的值。
        //
        // **坑 3：补空包会让接收端误以为「缺了很多」。**
        // 补齐是发送端的内部动作，接收端并不知道这一组原本只有 1 个真实分片。
        // 它看到 `received=1, missing=3`，而 `missing > redundant` 会直接放弃。
        //
        // 修法：补空包**不改变「这一组的有效包数」**。记录 `real_count`，
        // 接收端需要知道它才能正确判断「缺几个才算超限」。
        // 这个信息必须随冗余包传过去 —— 见 `FecPacket::real_count`。
        let gs = self.config.group_size;
        let real_count = self.encoder.pending();
        let mut guard = 0;
        while self.encoder.pending() > 0 && self.encoder.pending() < gs && guard < gs {
            self.encoder.push(&[]);
            guard += 1;
        }
        self.current_real_count = real_count as u16;

        if self.encoder.is_full() {
            out.extend(self.flush_group());
        }

        // 帧结束：组下标归零，准备下一帧
        self.group_in_frame = 0;
        self.frames_encoded += 1;
        out
    }

    fn flush_group(&mut self) -> Vec<FecPacket> {
        let reds = self.encoder.take_redundant();
        let mut out = Vec::with_capacity(reds.len());
        let gi = self.group_in_frame;
        for (k, payload) in reds.into_iter().enumerate() {
            let len = payload.len();
            out.push(FecPacket::new(
                self.frame_seq,
                gi,
                k as u16,
                self.current_real_count,
                payload,
            ));
            self.redundant_sent += 1;
            self.redundant_bytes += len as u64;
        }
        self.group_in_frame = self.group_in_frame.saturating_add(1);
        out
    }

    /// 冗余开销比例（实测）
    pub fn measured_overhead(&self) -> f64 {
        if self.frames_encoded == 0 {
            return 0.0;
        }
        // 粗略估计：每帧冗余字节 / (每帧数据字节)
        // 精确值需要调用方自己算，这里只给「冗余包数量级」
        self.redundant_bytes as f64 / (self.frames_encoded as f64 * 8000.0)
    }
}

/// 一个数据分片的完整信息
///
/// # 为什么用结构体而不是散参数
///
/// 原来 `push_fragment_with_meta` 有 8 个参数，clippy 直接报
/// 「too many arguments」。更重要的是：这 8 个值来源不同（帧头 3 个、
/// 分片头 2 个、载荷 1 个、组内推导 2 个），散传极易顺序传错，
/// 而传错的表现是「恢复出静默的垃圾数据」—— 没有任何报错。
#[derive(Debug, Clone, Copy)]
pub struct FragmentInfo<'a> {
    /// 帧序号（回绕）
    pub frame_seq: u16,
    /// 帧内分片下标
    pub index: usize,
    /// 分片的数据部分（已去掉帧头与分片头）
    pub data: &'a [u8],
    /// 本帧分片总数
    pub count: u16,
    /// 帧载荷总长
    pub total_size: u32,
    /// 呈现时间戳（微秒）
    pub pts_us: u64,
    /// 发送端时间戳（微秒）
    pub send_ts_us: u64,
    /// 原始帧头的 flags（bit0 = 关键帧）
    ///
    /// # 为什么必须带上它（实测抓到的真 bug）
    ///
    /// FEC 恢复出的分片要重建 `DatagramPiece` 送回重组器，而重组器
    /// 只从**第一片的 flags** 里拿帧属性。恢复路径硬编码了
    /// `FLAG_FRAGMENT`，于是：
    ///
    /// ```text
    /// 关键帧丢了第 0 片 → FEC 恢复 → flags = FLAG_FRAGMENT
    /// 重组器输出 flags = FLAG_FRAGMENT & !FLAG_FRAGMENT = 0
    /// 抖动缓冲看到「不是关键帧」→ needs_keyframe 逻辑失效
    /// ```
    ///
    /// 表现是：画面在丢包时出现「灰屏直到下一个自然关键帧」，
    /// 而日志里 `fec_recovered` 一切正常 —— 极难定位。
    pub flags: u8,
}

/// 接收端的 FEC 解码器（按帧分组）
///
/// # 关键：必须按组隔离状态
///
/// 踩过的三个坑都源于「一个 decoder 服务所有组」：
///
/// 1. `FecDecoder` 的 index 是**组内下标**（0..group_size）。一帧跨多组时，
///    帧内分片下标会超过 group_size，直接传进去会越界。
/// 2. 用 `index % group_size` 折算，不同组的下标又会**撞车** ——
///    组 1 的分片 4 折算成组内 0，把组 0 的分片 0 覆盖了。
/// 3. 组切换时调 `next_group()` 会清空，但那时组 0 的冗余包可能还没到。
///
/// 正确做法：**每组一个独立的 `FecDecoder`**，用 `HashMap<组号, decoder>` 存。
/// 组的生命周期很短（一帧内几个组），收到该组所有冗余包后就可以移除。
#[derive(Debug)]
pub struct FecReceiver {
    config: FecConfig,
    /// 组号 → 该组的解码器
    groups: std::collections::HashMap<u32, FecDecoder>,
    /// 组号 → 该帧的时间戳元数据 `(pts_us, send_ts_us)`
    ///
    /// # 为什么需要它
    ///
    /// FEC 恢复出的分片要重建 `DatagramPiece` 送回重组器，而重组出的帧会
    /// 带着 `pts_us` / `send_ts_us` 进抖动缓冲 —— 这两个值是延迟统计与
    /// 音画同步的依据。如果填 0，重组出的帧会被判为「延迟几百万毫秒」，
    /// 抖动缓冲会立刻丢掉它。
    ///
    /// 所以从同组已收到的分片里记一份。
    frame_meta: std::collections::HashMap<u32, (u64, u64, u8)>,
    /// 组号 → 从该组已收到的分片里回读的 (count, total_size)
    ///
    /// # 为什么需要它
    ///
    /// FEC 恢复出的分片只有「数据部分」，没有分片头。而重组器需要
    /// `count`（本帧总片数）与 `total_size`（帧载荷总长）才能正确拼帧。
    ///
    /// 这两个值在同一组**其他已收到的分片**的分片头里 —— 所以收到数据分片时
    /// 顺便记一份，恢复时取出来用。
    group_meta: std::collections::HashMap<u32, (u16, u32)>,
    /// 统计
    pub redundant_received: u64,
    pub fragments_recovered: u64,
}

impl FecReceiver {
    pub fn new(config: FecConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            groups: std::collections::HashMap::new(),
            group_meta: std::collections::HashMap::new(),
            frame_meta: std::collections::HashMap::new(),
            redundant_received: 0,
            fragments_recovered: 0,
        })
    }

    pub fn config(&self) -> FecConfig {
        self.config
    }

    /// 就地切换配置（**不**整块重建）
    ///
    /// # 为什么需要它（自适应冗余度带来的新需求）
    ///
    /// 原来只有 `group_size` 会变，而它一变，`FecReceiver` 就被整块重建
    /// （`udp_server.rs` 的 Config 分支）。现在 `redundant` 也会随链路丢包率
    /// 自适应，两者任何一个变化都要切配置 —— 而重建会**连带丢掉
    /// `group_meta` / `frame_meta`**，那两张表存的是「同组已收到分片的
    /// `count` / `total_size` / `pts_us` / `send_ts_us` / `flags`」。
    ///
    /// 它们全是**帧级**信息，与 `group_size` / `redundant` 无关。丢掉它们
    /// 的后果是：切换后仍在途的那些组即使恢复出分片，也拼不出
    /// `DatagramPiece`（缺 `count` / `total_size`），恢复出的数据只能被丢弃。
    ///
    /// 所以这里只清 `groups`（每个 `FecDecoder` 的组内下标语义绑死了旧配置，
    /// 必须重建），保留两张 meta 表 —— 它们由 `drop_before_frame` 按帧号回收。
    pub fn set_config(&mut self, config: FecConfig) -> Result<(), String> {
        config.validate()?;
        if config == self.config {
            return Ok(());
        }
        self.config = config;
        // 旧组的下标语义已失效（`i % redundant` 与 `index / group_size` 都变了），
        // 继续用它们只会恢复出静默的垃圾数据 —— 直接丢。
        self.groups.clear();
        Ok(())
    }

    /// 记录收到一个数据分片（带分片头元数据）
    ///
    /// `index` 是**帧内分片下标**（0..该帧分片数）。
    /// `count` 与 `total_size` 来自分片头，用于恢复时重建分片头。
    pub fn push_fragment_with_meta(&mut self, f: FragmentInfo<'_>) {
        let gs = self.config.group_size;
        let group_in_frame = (f.index / gs) as u16;
        let gid = ((f.frame_seq as u32) << 16) | group_in_frame as u32;
        self.group_meta.insert(gid, (f.count, f.total_size));
        self.frame_meta
            .insert(gid, (f.pts_us, f.send_ts_us, f.flags));
        self.push_fragment(f.frame_seq, f.index, f.data);
    }

    /// 取某帧的元数据 `(pts_us, send_ts_us, flags)`
    ///
    /// 用于重建恢复分片时填对时间戳与帧属性：
    /// - 时间戳错了 → 帧被抖动缓冲判为「太迟」直接丢弃
    /// - flags 错了 → 关键帧被当成普通帧，`needs_keyframe` 逻辑失效
    pub fn frame_meta(&self, gid: u32) -> Option<(u64, u64, u8)> {
        self.frame_meta.get(&gid).copied()
    }

    /// 取某组的分片头元数据（`count`, `total_size`）
    ///
    /// 用于把 FEC 恢复出的数据部分重建成完整分片。
    pub fn group_meta(&self, gid: u32) -> Option<(u16, u32)> {
        self.group_meta.get(&gid).copied()
    }

    /// 记录收到一个数据分片
    ///
    /// `index` 是**帧内分片下标**（0..该帧分片数）。
    /// `frame_seq` 用来定位它属于哪一帧的哪一组。
    pub fn push_fragment(&mut self, frame_seq: u16, index: usize, data: &[u8]) {
        let gs = self.config.group_size;
        let group_in_frame = (index / gs) as u16;
        let in_group = index % gs;
        let gid = ((frame_seq as u32) << 16) | group_in_frame as u32;

        let dec = self
            .groups
            .entry(gid)
            .or_insert_with(|| FecDecoder::new(self.config).expect("配置已在 new 里校验"));
        dec.push_original(in_group, data);
    }

    /// 记录收到一个冗余包，返回能立即恢复的分片
    ///
    /// 返回的 `index` 已还原成**帧内分片下标**，调用方可直接用来重组。
    pub fn push_redundant(&mut self, pkt: &FecPacket) -> Vec<(usize, Vec<u8>)> {
        let gid = pkt.group_id();
        let dec = self
            .groups
            .entry(gid)
            .or_insert_with(|| FecDecoder::new(self.config).expect("配置已在 new 里校验"));
        dec.push_redundant(pkt.k as usize, &pkt.payload);
        self.redundant_received += 1;

        // 用 real_count 把「补齐的空包」标记为已收到 —— 它们本来就不需要传输。
        // 见 `FecPacket::real_count` 的说明。
        let real = (pkt.real_count as usize).min(self.config.group_size);
        for i in real..self.config.group_size {
            dec.push_original(i, &[]);
        }

        let base = (pkt.group_in_frame as usize) * self.config.group_size;
        let recovered: Vec<(usize, Vec<u8>)> = dec
            .recover()
            .into_iter()
            .filter(|(in_group, _)| *in_group < real)
            .map(|(in_group, data)| (base + in_group, data))
            .collect();
        self.fragments_recovered += recovered.len() as u64;
        recovered
    }

    /// 某一组还缺几个分片
    pub fn missing(&self, gid: u32) -> usize {
        self.groups
            .get(&gid)
            .map(|d| d.missing())
            .unwrap_or(self.config.group_size)
    }

    /// 某一组收到几个分片
    pub fn received(&self, gid: u32) -> usize {
        self.groups.get(&gid).map(|d| d.received()).unwrap_or(0)
    }

    /// 当前跟踪的组数（用于诊断内存占用）
    pub fn tracked_groups(&self) -> usize {
        self.groups.len()
    }

    /// 丢弃某一组的状态（该组已完整重组或已放弃）
    pub fn drop_group(&mut self, gid: u32) {
        self.groups.remove(&gid);
        self.group_meta.remove(&gid);
        self.frame_meta.remove(&gid);
    }

    /// 丢弃所有早于 `frame_seq` 的组（帧级别清理）
    pub fn drop_before_frame(&mut self, frame_seq: u16) {
        self.groups
            .retain(|gid, _| (*gid >> 16) >= frame_seq as u32);
        self.group_meta
            .retain(|gid, _| (*gid >> 16) >= frame_seq as u32);
        self.frame_meta
            .retain(|gid, _| (*gid >> 16) >= frame_seq as u32);
    }

    /// 按「离当前帧多远」回收组状态（回绕安全的帧级清理）
    ///
    /// # 为什么需要它（实测发现的真实内存泄漏）
    ///
    /// [`Self::drop_before_frame`] 要求调用方传绝对帧号，而 FEC 的帧号是
    /// **u16**（冗余包头只给了它 2 字节）。直接用 `frame_seq as u16` 比较
    /// 会在回绕后一次性清空全部状态，用绝对号又无法表达「比当前帧早 N 帧」。
    ///
    /// 更关键的是：**生产路径此前从未调用过任何清理函数**。
    /// `udp_server.rs` 只在测试与 `e2e_pipeline` 里调 `drop_before_frame`，
    /// daemon 里一次都没有 —— 于是 `groups` / `group_meta` / `frame_meta`
    /// 三张表随帧数**无限增长**。
    ///
    /// 量级：64KB 帧切 55 片，每片不超过 1160 字节，每个 `FecDecoder` 持有
    /// 55 个已到达分片的拷贝 —— 单帧就是约 64KB，30fps 下**每秒 1.9MB**，
    /// 十分钟 1.1GB。这不会立刻崩，但会把跑了半天的接收端拖进 GC 式卡顿。
    ///
    /// # 为什么用「距离」而不是「绝对号」
    ///
    /// `gid` 的高 16 位是帧号，`cur.wrapping_sub(f)` 给出模 2^16 的距离，
    /// 天然回绕安全。距离大于 `keep_frames` 的视为过期；距离「看起来很大」
    /// （大于 32768，即实际是未来的帧）保留 —— 早到的帧不该被误删。
    pub fn prune_stale(&mut self, current_frame_seq: u32, keep_frames: u32) -> usize {
        let cur = current_frame_seq as u16;
        let keep = keep_frames.min(u16::MAX as u32) as u16;
        let stale = |gid: &u32| -> bool {
            let f = (*gid >> 16) as u16;
            let dist = cur.wrapping_sub(f);
            // dist > 32768 说明 f 在 cur 之后（未来帧），保留
            dist <= 32768 && dist > keep
        };
        let before = self.groups.len();
        self.groups.retain(|gid, _| !stale(gid));
        self.group_meta.retain(|gid, _| !stale(gid));
        self.frame_meta.retain(|gid, _| !stale(gid));
        before - self.groups.len()
    }
}
/// 根据丢包率决定是否启用 FEC，以及用多少冗余
///
/// # 决策表（来自 `fec` 模块文档与实测）
///
/// | 包级丢包率 | 建议 |
/// | --- | --- |
/// | < 1% | 不启用（冗余包 99% 用不上） |
/// | 1-3% | `redundant = 1`（单丢全能救） |
/// | 3-8% | `redundant = 2` |
/// | 8-15% | `redundant = 3`（但预期部分恢复） |
/// | > 15% | 不启用，改为降码率（FEC 开销追不上收益） |
pub fn suggest_fec(loss_ratio: f64, group_size: usize) -> Option<FecConfig> {
    let redundant = if loss_ratio < 0.01 {
        return None;
    } else if loss_ratio < 0.03 {
        1
    } else if loss_ratio < 0.08 {
        2
    } else if loss_ratio < 0.15 {
        3
    } else {
        return None;
    };
    let cfg = FecConfig {
        group_size,
        redundant: redundant.min(group_size),
        max_packet_size: SAFE_MTU_PAYLOAD,
    };
    cfg.validate().ok().map(|_| cfg)
}
/// FEC 的净收益评估（基于实测决策表，不是解析模型）
///
/// # 为什么用查表而不是建模（三次建模都错了）
///
/// 我试过三种解析模型来预测「FEC 值不值得开」，全部失败：
///
/// | 尝试 | 错在哪 |
/// | --- | --- |
/// | `p * (1-p)^(n-1) * (1-p^r)` | 两次衰减把数字压到 6-7%，所有丢包率都判负 |
/// | 二项分布 `P(K<=r)` 递推 | `p/q` 递推在 p 大时数值不稳，恒等于 1.0 |
/// | 加「带宽压力」二阶项 | 基准不一致（`1-p` vs `(1-o)(1-p')`），全判负 |
///
/// 根因：FEC 的实际收益取决于**分片大小分布、组边界对齐、拥塞控制响应、
/// NACK 时序**等多个交互因素，不是一个丢包率的函数。
///
/// 所以改用**实测决策表**。数据来自本机回环 30fps / 8KB 帧 / 5 秒的
/// 五档实测（见 [fec-link.md](../../../docs/fec-link.md) §10）：
///
/// | 丢包率 | 冗余度 | 实测开销 | 实测 NACK 请求 | 判定 |
/// | --- | --- | --- | --- | --- |
/// | 0% | — | 0% | 128（baseline） | 不需要 |
/// | 2% | 1 | 13.7% | 27 | **划算** |
/// | 5% | 2 | 27.0% | 21 | **划算** |
/// | 10% | 3 | 38.6% | 85（比 baseline 少但比 5% 多） | 边际 |
///
/// 关键观察：5% → 10% 时 NACK 从 21 涨到 85，说明高开销开始反噬。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FecVerdict {
    /// 是否值得启用
    pub worth_it: bool,
    /// 该配置的带宽开销比例（0-1）
    pub overhead_ratio: f64,
    /// 实测依据的说明（来自哪一档）
    pub evidence: &'static str,
}

/// 按实测决策表判断某个丢包率下是否值得开 FEC
///
/// 返回的 `FecConfig` 是实测验证过的推荐档位，`None` 表示不值得开。
pub fn evaluate_fec(loss_ratio: f64, group_size: usize) -> Option<(FecConfig, FecVerdict)> {
    // 决策表直接来自实测，不做插值/建模。
    // 分档边界取在实测点之间，宁可保守（少开）也不要过冲。
    let (redundant, evidence) = if loss_ratio < 0.01 {
        return None; // 实测：0% 丢包时冗余包纯浪费
    } else if loss_ratio < 0.035 {
        (1, "实测 2% 丢包 / redundant=1 / 开销 13.7% / NACK 27")
    } else if loss_ratio < 0.075 {
        (2, "实测 5% 丢包 / redundant=2 / 开销 27.0% / NACK 21")
    } else if loss_ratio < 0.12 {
        // 10% 档：实测 NACK 85，比 5% 档差但比不开（128）好，边际划算
        (
            3,
            "实测 10% 丢包 / redundant=3 / 开销 38.6% / NACK 85（边际）",
        )
    } else {
        // > 12%：实测趋势显示开销反噬，应改为降码率
        return None;
    };

    let cfg = FecConfig {
        group_size,
        redundant: redundant.min(group_size),
        max_packet_size: SAFE_MTU_PAYLOAD,
    };
    cfg.validate().ok()?;

    Some((
        cfg,
        FecVerdict {
            worth_it: true,
            overhead_ratio: cfg.overhead_ratio(),
            evidence,
        },
    ))
}

// ============================================================================
// 自适应冗余度：数学层
// ============================================================================
//
// # 问题
//
// `redundant` 一直固定为 2。但「2 个冗余包」的含义完全取决于**组里有几个包**：
//
// | 组内包数 | 丢包率 | 期望丢失 | redundant=2 够不够 |
// | --- | --- | --- | --- |
// | 2 | 3% | 0.06 | 够（但开销 100%，不值） |
// | 8 | 3% | 0.24 | 够（开销 25%） |
// | 55 | 3% | 1.65 | **不够**（P(丢 3 个以上) 不可忽略） |
//
// 正确的问题是：**给定组内包数与丢包率，几个冗余包才能让「可恢复」的概率
// 达到目标？** 这就是 [`suggest_redundant`] 回答的问题。

/// 二项分布 CDF：`P(X <= r)`，其中 `X ~ Binomial(k, p)`
///
/// # 为什么直接算而不是递推（`fec-link.md` §12 踩过的坑）
///
/// 递推式 `binom *= (k-i+1)/i * p/q` 在 `p` 接近 1 时被 `p/q` 放大到失控 ——
/// 实测 10% 丢包下算出 savings 恒等于 1.0，所有配置都判「值得」，模型完全失效。
///
/// 直接按定义算每一项 `C(k,i) p^i q^(k-i)` 再求和：k <= 64 时只有几十次
/// 乘加，开销可忽略，而且没有任何累积放大。
///
/// 边界：`k == 0` 或 `p <= 0` 时恒为 1.0（永远不会丢）；`p >= 1` 时只有
/// `r >= k` 才可能恢复。
pub fn binomial_cdf(k: usize, p: f64, r: usize) -> f64 {
    if k == 0 || r >= k {
        return 1.0;
    }
    if p.is_nan() || p <= 0.0 {
        return 1.0;
    }
    if p >= 1.0 {
        // 全丢：必须 r >= k 才能恢复，而上面已经处理了 r >= k
        return 0.0;
    }
    let q = 1.0 - p;
    let mut comb = 1.0f64; // C(k, i)
    let mut sum = 0.0f64;
    for i in 0..=r {
        sum += comb * p.powi(i as i32) * q.powi((k - i) as i32);
        // C(k, i+1) = C(k, i) * (k - i) / (i + 1)
        comb = comb * (k - i) as f64 / (i + 1) as f64;
    }
    sum.clamp(0.0, 1.0)
}

/// 给定组内分片数与包级丢包率，算出「让 P(可恢复) >= target」需要几个冗余包
///
/// # 数学
///
/// 组内 `k` 个真实包，包级丢包率 `p`，冗余 `r` 个。丢失数 `X ~ Binomial(k, p)`，
/// 纯 XOR 的可恢复条件是 `X <= r`（见 [`crate::fec`] 的能力表），于是
///
/// ```text
/// P(可恢复) = Σ_{i=0}^{r} C(k,i) p^i (1-p)^(k-i)
/// ```
///
/// 返回让上式 `>= target` 的**最小** `r`（`0..=k`）。
///
/// # 两个已知近似（都会让实际恢复率**低于**目标）
///
/// 1. **忽略冗余包自身的丢失**。严格来说 `r` 个冗余包也按 `p` 丢，
///    要救回 `i` 个丢包，至少需要「第 `i` 个可用的冗余包」到达。
///    完整式要枚举余类分布，复杂度高一个量级，第一版不做。
///    代价量级：`r=2, p=3%` 时冗余包至少丢 1 个的概率 5.9%，
///    这些情况下能救的丢包数从 2 降到 1 —— 对 `k <= 10` 的组影响很小
///    （P(丢 2 个) 本来就极低），对 `k = 55` 影响约 1-2 个百分点。
///
/// 2. **假设「任意 `<= r` 个丢失都可恢复」**。纯 XOR 做不到 ——
///    GF(2) 上不存在非平凡的 MDS 码。`r` 个丢包必须落在 `r` 个**不同余类**
///    （`i % r`）才能全解。实测（`crate::fec` 文档的表）：`r=2, k=10` 时
///    双丢只有 25/45 可解（55.6%），不是 100%。
///    所以上式给出的 `r` 是**下界**，真实恢复率会明显低于 `target`。
///    这个偏差在大组上更严重（`k=55, r=5` 时五丢全落在不同余类的概率只有约 60%）。
///
/// 两个近似同向（都高估可恢复率），所以 `suggest_redundant` 偏保守地**少给**冗余 ——
/// 而「少给」的代价由 NACK 兜底，不会造成数据损坏。这是刻意的取舍。
///
/// # 边界行为
///
/// - `loss_rate <= 0` -> 0（不丢包就不需要冗余）
/// - `group_size == 0` -> 0
/// - `target <= 0` -> 0
/// - `loss_rate >= 1` -> `group_size`（全丢，只能全冗余，实际无意义）
pub fn suggest_redundant(group_size: usize, loss_rate: f64, target: f64) -> usize {
    if group_size == 0 {
        return 0;
    }
    let p = if loss_rate.is_nan() {
        0.0
    } else {
        loss_rate.clamp(0.0, 1.0)
    };
    let target = if target.is_nan() {
        0.99
    } else {
        target.clamp(0.0, 1.0)
    };
    if p <= 0.0 || target <= 0.0 {
        return 0;
    }
    for r in 0..=group_size {
        if binomial_cdf(group_size, p, r) >= target {
            return r;
        }
    }
    group_size
}

/// 自适应冗余度的默认目标恢复率
///
/// 0.99 是「每 100 帧最多 1 帧需要走 NACK」的量级。定得更高（0.999）会让
/// `r` 在大组上跳得很快（`k=55, p=3%` 时 0.99 -> r=4，0.999 -> r=7，开销翻 75%），
/// 而定得更低（0.95）就退化成 `r ≈ 2*mean`，等于放弃尾部。
pub const DEFAULT_RECOVERY_TARGET: f64 = 0.99;

/// 自适应冗余度的上限
///
/// 只是防御性上限：真正限制冗余量的是「开销闸门」（见发送端的
/// `FEC_MAX_OVERHEAD_RATIO`），那个按 `r / k` 卡，比固定上限更合理。
pub const MAX_ADAPTIVE_REDUNDANT: usize = 16;

/// 链路丢包率估计器（发送端用）
///
/// # 为什么用 NACK 量估算
///
/// 发送端没有任何直接测量丢包的手段：UDP 无确认，接收端的 `STATS` 帧只回
/// 拥塞控制用的目标码率。**唯一现成的反馈是 NACK** —— 接收端发现某帧缺片、
/// 且判定「不会再补齐」时会点它的名。
///
/// # 三个必须处理的偏差
///
/// 1. **NACK 的粒度是「帧」不是「分片」**（最容易错的一处）。
///    `NackRequest.seqs` 里装的是帧序号（发送端据此从重传缓存取整帧重发）。
///    而我们要的是**包级**丢包率。直接算 `nack_seqs / fragments_sent` 会把
///    估计值压低 `avg_frags` 倍（8KB 帧压 7 倍、64KB 帧压 55 倍）。
///
/// 2. **NACK 会被重发**。`NackGenerator` 对同一个 seq 最多请求 3 次
///    （`retry_ms = 20ms`），每次都会让 `seqs_requested` 加一。
///    不按 seq 去重会把估计值放大 3 倍 —— 这里用窗口内 `HashSet` 去重。
///
/// 3. **FEC 救回的帧不产生 NACK**（最隐蔽的一个，第一版栽在这里）。
///    观测到的帧级失败率是 `P(丢失数 > redundant)`，**不是** `1-(1-p)^k`。
///    第一版用 `p = 1 - (1 - P_frame)^(1/k)` 反解，在 `k=57, redundant=2`、
///    真实丢包 3% 时算出 `p = 1.26%`（真值 3%），于是 `suggest_redundant`
///    给出 `r=3`，而实际需要 5 —— **自适应在低冗余下会自我固化**：
///    FEC 越有效，观测到的 NACK 越少，估计出的丢包率越低，就越不加冗余。
///    这是正反馈式的低估，必须修。
///
/// # 解法：用「FEC 感知」的模型反解，而不是二项 CDF 的朴素反函数
///
/// 观测到的帧级失败率 `f` 与包级丢包率 `p` 的关系是
///
/// ```text
/// f = 1 - Σ_{i=0}^{r} C(k,i) p^i (1-p)^(k-i)   （r = 当时的冗余度）
/// ```
///
/// 这个式子对 `p` 单调递增，用二分反解即可。`r = 0`（FEC 关）时它退化
/// 成经典形式 `f = 1-(1-p)^k`，与接收端拥塞控制的口径一致。
///
/// 反解出来的 `p` 才是真实链路丢包率，`suggest_redundant` 用它才算得对。
///
/// # 为什么要滑动窗口
///
/// 全程累计会让**早期的高丢包率永久影响后续决策**：链路在第 2 秒恢复健康，
/// 累计值仍然很高，FEC 继续按高冗余发，白烧带宽。窗口只保留最近
/// `window_fragments` 个分片的统计。
///
/// 窗口按**分片数**而不是时间：分片是真实的带宽单位，而且不同帧大小下
/// 窗口覆盖的时间长度自动伸缩（大帧滚得快，小帧滚得慢）。
#[derive(Debug)]
pub struct LossEstimator {
    /// 窗口容量（分片数）
    window_fragments: usize,
    /// 窗口的帧数上限
    ///
    /// 只用分片数会让**小帧**的窗口滚得极慢：2 分片的 H.264 P 帧要 150 帧
    /// （5 秒 @30fps）才凑满 300 个分片，整段测试都等不到一次估计。
    /// 帧数上限把这个上界压到 60 帧（2 秒）。
    window_frames: u32,
    /// 指数平滑系数（0.2 = 新样本占 20%）
    alpha: f64,
    /// 一个窗口至少要包含多少帧才值得采信
    min_frames: u32,
    /// `min_failures` 的**下限**（实际阈值按组大小在 `roll` 里插值）
    ///
    /// 少于阈值时保持旧估计 —— 见 `roll` 的说明（1 个失败帧在 k=57/r=16
    /// 下会被反解放大成 17.6% 的丢包率；而固定 5 又会让 k=8 永远等不到）。
    min_failures: u32,
    /// 已经产出了多少**有效**样本（滚过且通过 `min_failures` 闸门）
    samples: u64,
    /// 被 `min_failures` 闸门挡掉、保持旧值的窗口数
    pub held_windows: u64,
    /// 估计值上限（超过这个值 FEC 已经没有意义，钳住避免下游放大）
    max_loss: f64,
    // ── 当前窗口 ──
    frames: u32,
    frags: u64,
    nack_seqs: std::collections::HashSet<u32>,
    /// 本窗口内**生效过的冗余度**的加权和（按帧数），用于反解
    ///
    /// 窗口内 `redundant` 可能变过（自适应），所以不能只记一个值。
    /// 这里按帧累加，反解时取平均 —— 一阶近似，但比「用最后一个值」准。
    redundant_weighted: u64,
    // ── 累计（诊断用）──
    /// 记录过的帧数
    pub frames_total: u64,
    /// 记录过的分片数
    pub frags_total: u64,
    /// 去重后收到过 NACK 的帧数
    pub nack_frames_total: u64,
    /// 已滚过的窗口数
    pub windows: u64,
    // ── 状态 ──
    smoothed: f64,
    last_sample: f64,
    last_frame_loss: f64,
    last_avg_frags: f64,
    last_redundant: f64,
    /// 最近一个窗口里观测到的失败帧数（去重后），诊断用
    pub last_failures: u64,
}

impl LossEstimator {
    /// 默认窗口 300 个分片、`alpha = 0.2`
    pub fn new() -> Self {
        Self::with_window(300, 0.2)
    }

    pub fn with_window(window_fragments: usize, alpha: f64) -> Self {
        Self {
            window_fragments: window_fragments.max(1),
            window_frames: 60,
            alpha: if alpha.is_nan() {
                0.2
            } else {
                alpha.clamp(0.0, 1.0)
            },
            min_frames: 10,
            min_failures: 2,
            samples: 0,
            held_windows: 0,
            max_loss: 0.5,
            frames: 0,
            frags: 0,
            nack_seqs: std::collections::HashSet::new(),
            redundant_weighted: 0,
            frames_total: 0,
            frags_total: 0,
            nack_frames_total: 0,
            windows: 0,
            smoothed: 0.0,
            last_sample: 0.0,
            last_frame_loss: 0.0,
            last_avg_frags: 0.0,
            last_redundant: 0.0,
            last_failures: 0,
        }
    }

    /// 记录发出一帧
    ///
    /// `fragments` 是这一帧的**数据**分片数（不含冗余包）；
    /// `redundant` 是这一帧实际使用的冗余度（0 = 没发 FEC）。
    ///
    /// 两个都要：反解观测到的帧失败率时，必须知道当时 FEC 能救几个包。
    pub fn record_frame(&mut self, fragments: usize, redundant: usize) {
        self.frames += 1;
        self.frags += fragments as u64;
        self.redundant_weighted += redundant as u64;
        self.frames_total += 1;
        self.frags_total += fragments as u64;
        if self.frames >= self.min_frames
            && (self.frags >= self.window_fragments as u64 || self.frames >= self.window_frames)
        {
            self.roll();
        }
    }

    /// 记录收到一次 NACK 里的帧序号（同一帧重复请求只算一次）
    pub fn record_nack(&mut self, seq: u32) {
        if self.nack_seqs.insert(seq) {
            self.nack_frames_total += 1;
        }
    }

    /// 批量记录（发送端拿到的是 `Vec<u32>`）
    pub fn record_nacks(&mut self, seqs: &[u32]) {
        for s in seqs {
            self.record_nack(*s);
        }
    }

    /// 滚一个窗口：算样本 -> 指数平滑 -> 清零
    ///
    /// # 两个「不更新」的条件（都是实测逼出来的）
    ///
    /// 反解 `p` 用的是观测到的**帧失败率**，而失败率低到一定程度时
    /// 单窗口的样本量根本不足以支撑反解 —— 反解会把 1 个失败帧放大成
    /// 一个荒谬的丢包率。实测（k=57, r=16，真值 3%）：
    ///
    /// ```text
    /// 1 个失败帧 -> f=1.67% -> 反解 p=17.6%  -> suggest = 17  ← 荒谬
    /// 2 个失败帧 -> f=3.33% -> 反解 p=19.0%  -> suggest = 18
    /// ```
    ///
    /// 真值只有 3%。于是 `redundant` 一路爬到上限 16，开销 22.9%。
    /// 实测 C-64k 场景就是这样过冲的（真值只需 5）。
    ///
    /// 所以：
    ///
    /// 1. **失败帧数为 0 -> 保持旧估计**。零失败是下界不是观测值；
    ///    按 0 处理会与「r 变大 -> 失败变少 -> 降 r -> 失败变多」形成振荡
    ///    （实测 B/C 场景都是这个形状：r=5 零失败 -> want=1 -> r=1 失败 31 -> want=5）。
    /// 2. **失败帧数 < `min_failures` -> 保持旧估计**。样本太少，反解不可信。
    ///    阈值按「至少 5 个失败帧」定：二项分布的相对标准误约 `1/sqrt(5) = 45%`，
    ///    再多也压不住大 `r` 下的放大（那是模型本身的病态，不是样本量的问题）。
    ///
    /// 代价：链路真的恢复时估计值会停在旧值，`redundant` 不降。
    /// 这是**刻意**的 —— 降冗余只省带宽，而振荡会让 FEC 完全失效。
    fn roll(&mut self) {
        let frames = self.frames as f64;
        let frags = self.frags as f64;
        if frames <= 0.0 {
            return;
        }
        let failures = self.nack_seqs.len() as f64;
        let frame_loss = (failures / frames).clamp(0.0, 1.0);
        let avg_frags = (frags / frames).max(1.0);
        let avg_red = (self.redundant_weighted as f64 / frames).round() as usize;

        self.last_frame_loss = frame_loss;
        self.last_avg_frags = avg_frags;
        self.last_redundant = avg_red as f64;
        self.last_failures = failures as u64;

        // 闸门阈值按组大小自适应（实测 B 场景暴露的坑）。
        //
        // 固定 5 对 k=8 太高：真 5% 丢包、r=2 时，60 帧窗口的**期望**失败数
        // 只有 0.35 —— 永远过不了闸门，估计器一直停在 0，redundant 掉到 1。
        // 实测 B-8k 场景就是这样退化的（固定档 nack=28，自适应 nack=33）。
        //
        // 小 k 的组里每次失败只损失 1/k 的信息，样本需求本来就低；
        // 大 k 的组里 1 个失败帧会被反解放大成荒谬值（见上面 k=57 的例子）。
        // 所以阈值取「组越大要求越多」：k<=10 要 2 个，k>=40 要 5 个，中间线性。
        let floor = self.min_failures.max(1) as f64;
        let need = if avg_frags <= 10.0 {
            floor
        } else if avg_frags >= 40.0 {
            floor + 3.0
        } else {
            floor + 3.0 * (avg_frags - 10.0) / 30.0
        };
        let usable = failures >= need;
        if !usable {
            // 零失败或样本不足：保持旧估计（见上面两个「不更新」条件）
            self.held_windows += 1;
            self.windows += 1;
            self.frames = 0;
            self.frags = 0;
            self.redundant_weighted = 0;
            self.nack_seqs.clear();
            return;
        }

        // 关键：用「FEC 感知」的模型反解，不是朴素二项反函数。
        // 见类型文档第 3 条 —— 朴素反解在 FEC 生效时会系统性低估，
        // 而低估会让 redundant 停在低位，形成「FEC 越有效越不加冗余」的正反馈。
        let p = unresolve_loss(avg_frags.round() as usize, avg_red, frame_loss);
        let p = p.clamp(0.0, self.max_loss);

        self.last_sample = p;
        self.smoothed = if self.samples == 0 {
            // 第一个**有效**窗口直接采信：否则要从 0 爬上来，前几秒 FEC 等于没开
            p
        } else {
            self.alpha * p + (1.0 - self.alpha) * self.smoothed
        };
        self.samples += 1;
        self.windows += 1;

        self.frames = 0;
        self.frags = 0;
        self.redundant_weighted = 0;
        self.nack_seqs.clear();
    }

    /// 当前丢包率估计（指数平滑后的值，0..=max_loss）
    pub fn loss_estimate(&self) -> f64 {
        self.smoothed
    }

    /// 是否已有可信样本（至少滚过一个窗口）
    ///
    /// 没预热时估计值恒为 0，调用方**不应**据此关掉 FEC ——
    /// 那是「还没观测」而不是「链路很好」。
    pub fn is_warm(&self) -> bool {
        self.windows > 0
    }

    /// 最近一个窗口的原始样本（未平滑），诊断用
    pub fn last_sample(&self) -> f64 {
        self.last_sample
    }

    /// 最近一个窗口的**帧级**失败率（= NACK 帧占比），诊断用
    pub fn last_frame_loss(&self) -> f64 {
        self.last_frame_loss
    }

    /// 最近一个窗口的平均分片数，诊断用
    pub fn last_avg_frags(&self) -> f64 {
        self.last_avg_frags
    }

    /// 最近一个窗口平均生效的冗余度，诊断用
    pub fn last_redundant(&self) -> f64 {
        self.last_redundant
    }
}

impl Default for LossEstimator {
    fn default() -> Self {
        Self::new()
    }
}

/// 已知帧级失败率 `f`、组大小 `k`、冗余 `r`，反解包级丢包率 `p`
///
/// # 方程
///
/// ```text
/// f = 1 - Σ_{i=0}^{r} C(k,i) p^i (1-p)^(k-i)
/// ```
///
/// 右边对 `p` 单调递增（`r < k` 时严格递增），二分 64 次足够收敛到 f64 精度。
///
/// # 为什么不用解析反函数（第一版的错）
///
/// 第一版直接套 `p = 1 - (1-f)^(1/k)`，那是 `r = 0` 时的特例。
/// `r > 0` 时它**系统性低估**：`k=57, r=2, p=3%` 的真值下
/// `f = 1 - P(丢<=2) = 24.4%`，朴素反解给 `p = 1 - 0.756^(1/57) = 0.49%`，
/// 差 6 倍。低估的直接后果是 `redundant` 永远停在低位 ——
/// 实测 C-64k 场景（3% 丢包、55 片）就是 `redundant=2` 全程不动。
///
/// # 边界
///
/// - `f <= 0` -> 0（没有 NACK = 没观测到失败，不能反推正数）
/// - `f >= 1` -> 1
/// - `k == 0` -> 0
pub fn unresolve_loss(k: usize, r: usize, f: f64) -> f64 {
    if k == 0 || f.is_nan() || f <= 0.0 {
        return 0.0;
    }
    if f >= 1.0 {
        return 1.0;
    }
    // f 必须小于「r >= k 时的理论上限 1.0」，否则无解（r 覆盖全组时永远能恢复）
    if r >= k {
        return 0.0;
    }
    let mut lo = 0.0f64;
    let mut hi = 1.0f64;
    // 64 次二分：区间宽度降到 2^-64，远超 f64 有效位
    for _ in 0..64 {
        let mid = 0.5 * (lo + hi);
        let model = 1.0 - binomial_cdf(k, mid, r);
        if model < f {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// 冗余度迟滞控制器（**不对称**双闸门）
///
/// # 为什么需要迟滞
///
/// `redundant` 每变一次都要发 `FrameType::Config` 通知接收端，而接收端收到
/// 后必须丢弃所有未完成的组（组内下标语义变了）。丢包率估计本身有噪声
/// （NACK 的到达是成簇的），不加迟滞就会**每帧发两个 Config 帧**，
/// 接收端的 FEC 状态永远处于「刚被清空」的状态 —— FEC 等于没接。
///
/// # 为什么必须不对称（第一版对称迟滞的实测缺陷）
///
/// 第一版用对称闸门（`|want - current| >= 2` + 15 帧驻留），实测出两个问题：
///
/// | 场景 | 现象 | 根因 |
/// | --- | --- | --- |
/// | 64KB / 3% 丢包 | 估计值 1.26% 算出 want=3，`redundant` 全程停在 2 | `|3-2| = 1 < 2`，**升一级永远被挡** |
/// | 64KB / 8% 丢包 | 2→4→13→16→14→11→9→7，6 秒内切 8 次 | 降得太快，与估计值互相追尾 |
///
/// 第一条是**功能性失效**：`redundant` 从 2 升到 3 恰恰是最常见的升级路径
/// （8 分片在 3% 丢包下要 2，55 分片要 5），而对称闸门把整条 2→3 的路封死了。
///
/// # 不对称规则：快升慢降
///
/// | 方向 | 幅度闸门 | 驻留闸门 | 理由 |
/// | --- | --- | --- | --- |
/// | 升（want > current） | `>= 1` | 8 帧（0.27s） | 少给冗余的代价是**丢帧**，多给的代价只是带宽 |
/// | 降（want < current） | `>= 2` | 30 帧（1.0s） | 估计值刚被 FEC 拉低就立刻减，会形成正反馈振荡 |
///
/// 这个形状与 TCP 的 AIMD 同源：对「变差」快速响应，对「变好」保守确认。
/// 区别是这里的方向语义反了过来 —— 对 FEC 而言「加冗余」才是保守动作。
///
/// # 为什么不用「只增不减」（`group_size` 用的那套）
///
/// `group_size` 只增不减是安全的：组大小变大不会浪费带宽，因为 `should_fec`
/// 会跳过小帧。**`redundant` 完全相反** —— 它直接乘在每一帧的带宽上。
///
/// 链路从 10% 丢包恢复到 0.5%，若 `redundant` 停在 9，`k=55` 的帧就要多付
/// 16% 带宽，而且**永远不会降回来**。这是纯损失，没有任何机制兜底。
/// 所以 `redundant` 必须能降，只能靠闸门压住抖动频率。
#[derive(Debug)]
pub struct RedundantHysteresis {
    current: usize,
    /// 降冗余的最小幅度（升冗余恒为 1）
    min_delta_down: usize,
    /// 升冗余后的驻留帧数
    min_dwell_up: u32,
    /// 降冗余后的驻留帧数
    min_dwell_down: u32,
    ceiling: usize,
    frames_since_switch: u32,
    /// 上一次切换的方向（true = 升）。驻留时长按方向取不同的值
    last_was_up: bool,
    /// 累计切换次数（= 会发出的 Config 次数）
    pub switches: u64,
    /// 被闸门挡掉的切换意图（诊断迟滞是否过严）
    pub blocked: u64,
}

impl RedundantHysteresis {
    /// 默认：降幅度 2、升驻留 8 帧、降驻留 30 帧、上限 [`MAX_ADAPTIVE_REDUNDANT`]
    pub fn new(initial: usize) -> Self {
        Self {
            current: initial.max(1),
            min_delta_down: 2,
            min_dwell_up: 8,
            min_dwell_down: 30,
            ceiling: MAX_ADAPTIVE_REDUNDANT,
            frames_since_switch: 0,
            last_was_up: true,
            switches: 0,
            blocked: 0,
        }
    }

    /// 自定义闸门（测试与调参用）
    pub fn with_gates(mut self, min_delta_down: usize, dwell_up: u32, dwell_down: u32) -> Self {
        self.min_delta_down = min_delta_down.max(1);
        self.min_dwell_up = dwell_up;
        self.min_dwell_down = dwell_down;
        self
    }

    /// 当前生效的冗余度
    pub fn current(&self) -> usize {
        self.current
    }

    /// 上限（供调用方判断「是否已经顶格」）
    pub fn ceiling(&self) -> usize {
        self.ceiling
    }

    /// 观测一帧的期望冗余度，返回新值表示**需要切换配置**（`None` = 保持不变）
    pub fn observe(&mut self, want: usize) -> Option<usize> {
        self.frames_since_switch = self.frames_since_switch.saturating_add(1);
        let want = want.clamp(1, self.ceiling.max(1));
        if want == self.current {
            return None;
        }

        let up = want > self.current;
        // 升：幅度恒为 1 就够（少给冗余会丢帧）；降：要 2 以上，挡住 ±1 抖动。
        //
        // # 例外：降到下限（1）时不受幅度限制（实测发现的真 bug）
        //
        // 链路干净时 `suggest_redundant` 返回 0，调用方钳到 1。
        // 若当前是 2，`abs_diff = 1 < min_delta_down = 2` → **永久卡在 2**。
        //
        // 实测症状：`--loss 0` 时 `fec_recovered = 0` 但 `fec_overhead = 28.2%`
        // —— 发了 100% 用不上的冗余包，10 秒白白多发 723 KB。
        //
        // 语义上也该放行：`min_delta_down` 的目的是「挡住 2↔3 这种 ±1 抖动」，
        // 而降到 1 是**触底**，下面没有更小的值可以抖回来 —— 不构成抖动。
        let at_floor = want <= 1;
        if !up && !at_floor && want.abs_diff(self.current) < self.min_delta_down {
            self.blocked += 1;
            return None;
        }
        // 驻留闸门只管「切换之后别马上再切」。一次都没切过时不该受限 ——
        // 否则预热期的第一个真实值要白等一整个驻留期才生效。
        // 驻留时长按**本次要切的方向**取（不是上一次的方向）：
        // 「想降」就该等够降的驻留期，否则链路刚恢复就减冗余、下个窗口又丢包。
        if self.switches > 0 {
            let need = if up {
                self.min_dwell_up
            } else {
                self.min_dwell_down
            };
            if self.frames_since_switch < need {
                self.blocked += 1;
                return None;
            }
        }
        self.current = want;
        self.frames_since_switch = 0;
        self.last_was_up = up;
        self.switches += 1;
        Some(want)
    }
}

// ============================================================================
// 关键帧保护：为什么关键帧必须和 P 帧分开算冗余
// ============================================================================
//
// # 实测暴露的问题
//
// H.264 接入后，1280x720 / 5% 丢包下接收端报 `video_need_keyframe=108`
// （1106 帧里 108 帧没画面，9.8%）。根因是**关键帧太大**：
//
// | 帧类型 | 字节数 | 分片数（1160B/片） | 组内期望丢失（5%） |
// | --- | --- | --- | --- |
// | P 帧 | ~1200 | 2 | 0.10 |
// | **IDR 关键帧** | **~39437** | **35** | **1.75** |
//
// 35 个分片里丢任意一个 → 整帧不可解码 → 后续 P 帧全部失效 → 等下一个
// GOP（60 帧 = 1 秒）。而 1 个分片丢的代价，P 帧只要 1 个 RTT 的 NACK 就补回来。
//
// 旧的 `should_fec` 只看「分片数 > 冗余数」，于是关键帧和 P 帧拿到**同一个**
// `redundant`（通常 1-2）。35 片配 2 个冗余包，实测恢复率只有 58.4%（见下表），
// 等于关键帧完全没被保护。
//
// # 现有 `suggest_redundant` 为什么不能直接拿关键帧的 target 去调
//
// `suggest_redundant` 用的是**二项 CDF**（`P(X <= r)`），它假设「任意 r 个丢失
// 都能救」。纯 XOR 做不到 —— GF(2) 上没有非平凡的 MDS 码，r 个丢包必须落在
// r 个**不同余类**（`i % r`）才可解。这个偏差随组大小急剧放大：
//
// | k | p | 二项模型 @0.999 给出 | 真实可解率 @0.999 需要 | 偏差 |
// | --- | --- | --- | --- | --- |
// | 8 | 5% | r=3 | r=8 | 2.7x |
// | 35 | 5% | r=9 | r=35（仍只有 99.65%） | 3.9x |
// | 55 | 2% | r=12 | r=54 | 4.5x |
//
// 所以在关键帧这条路上必须换**精确模型**：按余类分组，先求「恰好丢了 t 个、
// 且这 t 个落在 t 个互不相同的余类」的概率，再乘上「冗余包 R 覆盖住这些余类」
// 的概率。
//
// 推导（`n_c` = 余类 c 的成员数，`x_c = n_c * p / q`）：
//
//     P(全帧可解) = q^k * sum_d [x^d] prod_c (1 + x_c * x) * R(d)
//     R(0) = 1
//     R(1) = 1 - p^(r-1)                        （单余类：任意 1 个 R 到达就够）
//     R(d) = q^d + d*p*q^(d-1) * (1 - p^(r-d))  （d >= 2）
//
// `prod_c (1 + x_c x)` 是一次多项式 DP（O(r^2)），`r <= 64` 时是几千次乘加，
// 可以直接放在每帧的发送路径上算。
//
// # 实测验证（`tests/fec_keyframe.rs` 里的端到端测试守着这张表）
//
// 用真实 `FecSender` / `FecReceiver` 跑 20 万次随机丢包，与上式对比：
//
// | k | r | p | 模型 | 实测 | 偏差 |
// | --- | --- | --- | --- | --- | --- |
// | 35 | 8 | 5% | 86.81% | 86.83% | -0.02pp |
// | 35 | 17 | 5% | 95.08% | 95.09% | -0.01pp |
// | 35 | 35 | 5% | 99.65% | 99.65% | +0.00pp |
// | 55 | 27 | 5% | 92.32% | 92.23% | +0.09pp |
// | 55 | 27 | 10% | 68.26% | 68.23% | +0.03pp |
// | 8 | 3 | 5% | 98.19% | 98.07% | +0.12pp |
//
// 偏差全部在 0.15pp 以内，可以放心用它做决策。

/// 关键帧的目标恢复率（**实测选定**，不是拍的）
///
/// # 为什么是 0.999 而不是沿用 P 帧的 0.99
///
/// 两者对「一次失败」的容忍度差 30 倍以上：
///
/// | 帧类型 | 丢了的代价 | 可接受失败率 |
/// | --- | --- | --- |
/// | P 帧 | NACK 补 1 个 RTT（局域网 1ms，跨城 40ms） | 1e-2 |
/// | **关键帧** | **等一个 GOP = 60 帧 = 1 秒画面卡死** | 1e-3 |
///
/// 实测（k=35，无带宽限制下 `suggest_redundant_exact` 给出的最小 r）：
///
/// | 丢包率 | target=0.99 | target=0.999 | 倍数 |
/// | --- | --- | --- | --- |
/// | 0.5% | r=2（6%） | r=11（31%） | 5.5x |
/// | 1% | r=6（17%） | r=26（74%） | 4.3x |
/// | 2% | r=15（43%） | r=33（94%） | 2.2x |
/// | 3% | r=25（71%） | r=35（100%，天花板 99.95%） | 1.4x |
///
/// 取 0.999 而不是 0.995：0.995 在 1% 丢包下只要 r=10，对应「每 200 个关键帧
/// 失败 1 次」= 每 200 秒卡一次，用户能感知到。0.999 是「每 1000 个关键帧
/// （约 16 分钟）失败 1 次」的量级，低于「用户会主动抱怨」的阈值。
///
/// 取 0.9999 则不可行：k=35 / p=5% 时**即使 r=35（100% 开销）也只有 99.65%**，
/// p=3% 时天花板 99.95% —— 都够不到 0.9999。目标定得比物理上限还高，只会让
/// `suggest` 恒定返回 `k`，白白烧掉全部带宽换 0 收益。0.999 在 <=3% 丢包区间内
/// 可达，5% 以上由带宽闸门接管（见 [`KEYFRAME_MAX_OVERHEAD_RATIO`]）。
pub const KEYFRAME_RECOVERY_TARGET: f64 = 0.999;

/// 关键帧冗余度的带宽上限（`冗余字节 / 数据字节`）
///
/// # 为什么必须钳（不钳会怎样）
///
/// 55 个分片的关键帧按 `target=0.999` 在 5% 丢包下要 r=55，即
/// `55 x 1160B = 63.8KB` 冗余 —— 原帧才 39.4KB，**码率直接翻 2.6 倍**。
/// 而且这 63.8KB 是每 60 帧（1 秒）一次，等于常驻 +160% 带宽。
///
/// # 为什么是 0.5（实测的边际收益拐点）
///
/// k=35 关键帧，不同上限下的实测恢复率与整条流的码率代价
/// （码率增量 = `ratio * 39437 / 110237`，按 1 个关键帧 / 60 帧算）：
///
/// | 上限 | r | 恢复率 @5% | 恢复率 @2% | 整条流码率增量 |
/// | --- | --- | --- | --- | --- |
/// | 0%（不保护） | 0 | 16.61% | 49.31% | +0.0% |
/// | 5% | 1 | 45.67% | 83.82% | +1.8% |
/// | 10% | 3 | 69.05% | 93.51% | +3.6% |
/// | 20% | 7 | 85.03% | 97.33% | +7.2% |
/// | 30% | 10 | 89.62% | 98.25% | +10.7% |
/// | 40% | 14 | 93.11% | 98.89% | +14.3% |
/// | **50%** | **17** | **95.08%** | **99.24%** | **+17.9%** |
/// | 75% | 26 | 97.45% | 99.63% | +26.8% |
/// | 100% | 35 | 99.65% | 99.99% | +35.8% |
///
/// 拐点在 40%-50%：从 0 到 50% 换来 +78pp（16.6% -> 95.1%），从 50% 到 100%
/// 只再换 +4.6pp 却要多付一倍带宽。50% 是「每多付 1% 带宽换到的恢复率」最高
/// 的那一档之后的第一档。
///
/// 整条流的码率增量 `0.5 * 39437 / 110237 = 17.9%`，与 `docs/fec-link.md` 里
/// 实测的「5% 丢包 + redundant=2 = 27% 开销」同一量级，但换来的是关键帧存活率
/// 从 58% 涨到 95%，是明确的净收益。
///
/// # 为什么不是固定包数
///
/// 固定 16 包对 8 片帧是 200% 开销（比数据还大），对 64 片帧只有 25%（不够）。
/// 按比例钳才同时守住「小帧别浪费」和「大帧别失控」。
pub const KEYFRAME_MAX_OVERHEAD_RATIO: f64 = 0.5;

/// 帧分类：决定用哪套冗余策略
///
/// # 为什么不用裸 `bool`
///
/// 光有 `is_keyframe` 不够 —— 冗余度是 `fragments` 的函数（组内包数决定
/// 需要几个方程）。两个值总是一起用，散传容易漏掉一个。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameClass {
    /// 是否关键帧（`FLAG_KEYFRAME`）
    pub is_keyframe: bool,
    /// 本帧的数据分片数（不含冗余包）
    pub fragments: usize,
}

impl FrameClass {
    /// 关键帧
    pub fn keyframe(fragments: usize) -> Self {
        Self {
            is_keyframe: true,
            fragments,
        }
    }

    /// 非关键帧（P / B 帧）
    pub fn delta(fragments: usize) -> Self {
        Self {
            is_keyframe: false,
            fragments,
        }
    }
}

/// 精确的可恢复概率：组内 `k` 个真实包、`r` 个冗余包、包级丢包率 `p`
///
/// 见本节上方的模块级说明（那张「二项模型 vs 真实可解率」的表）。
///
/// # 与 [`binomial_cdf`] 的区别
///
/// `binomial_cdf(k, p, r)` 是 `P(丢失数 <= r)`，**高估**可恢复率。本函数才是
/// 「`FecDecoder::recover` 能解出全部缺失」的概率，与 `crate::fec` 里
/// 高斯-约当消元的实际行为一致（`tests/fec_keyframe.rs` 的
/// `exact_model_matches_real_decoder` 用真实 `FecSender`/`FecReceiver` 守着）。
///
/// # 边界
///
/// - `k == 0` -> 1.0（空帧无需恢复）
/// - `p <= 0` -> 1.0
/// - `p >= 1` -> 0.0
/// - `r == 0` -> `(1-p)^k`（没有冗余时只有「一个都没丢」才完整）
/// - `r == 1` -> `q^k * (1 + k*p)`（单余类，任意 1 个冗余包到达就能解）
/// - `r >= k` -> 与 `r < k` 走同一个公式（**不要短路成下界**，见下）
///
/// # 踩过的坑：`r >= k` 不能短路成 `q^k (1+p)^k`
///
/// 第一版在 `r >= k` 时直接返回 `q^k * (1+p)^k`，理由是「每个余类恰好 1 个
/// 成员，真实值接近 1，给个保守下界就行」。**这个下界是错的，而且不单调**：
///
/// `k=4, p=1%` 时它给 `r=3` 99.99%、`r=4` 99.96% —— **冗余包越多概率越低**。
/// 真实值（蒙特卡洛 40 万次）是 `r=3` 99.9877%、`r=4` 100.0000%。
///
/// 根因：`q^k (1+p)^k` 假设「每个丢包都恰好有自己那一个冗余包到达」，
/// 但 `r=k` 时丢 1 个包其实有 `r-1 = k-1` 个冗余包都能救它（只有排除它
/// 余类的那个不行）。下界把这份「多重冗余」全丢了，于是在 `r` 接近 `k` 时
/// 严重低估，还制造出非单调。
///
/// 现在统一走通用公式（它在 `r = k` 时自动退化正确），实测误差 < 0.2pp。
///
pub fn exact_recovery_probability(k: usize, r: usize, p: f64) -> f64 {
    if k == 0 {
        return 1.0;
    }
    if p.is_nan() || p <= 0.0 {
        return 1.0;
    }
    if p >= 1.0 {
        return 0.0;
    }
    let r = r.min(k);
    let q = 1.0 - p;
    if r == 0 {
        return q.powi(k as i32);
    }
    if r == 1 {
        // 单余类：只要丢了包，任意一个到达的冗余包就能解。
        // P = q^k * (1 + k*p)
        return (q.powi(k as i32) * (1.0 + k as f64 * p)).clamp(0.0, 1.0);
    }

    // 余类 c 的成员数：下标 i 落在余类 i % r，共 k 个下标
    let mut xs: Vec<f64> = Vec::with_capacity(r);
    for c in 0..r {
        let n_c = if c < k { (k - 1 - c) / r + 1 } else { 0 };
        xs.push(n_c as f64 * p / q);
    }

    // 多项式 DP：poly[d] = prod_c (1 + x_c * x) 里 x^d 的系数
    let mut poly = vec![0.0f64; r + 1];
    poly[0] = 1.0;
    let mut deg = 0usize;
    for x in &xs {
        let mut d = deg;
        loop {
            poly[d + 1] += poly[d] * x;
            if d == 0 {
                break;
            }
            d -= 1;
        }
        deg = (deg + 1).min(r);
    }

    let mut sum = 0.0f64;
    for (d, coef) in poly.iter().enumerate().take(deg + 1) {
        if *coef == 0.0 {
            continue;
        }
        let cover = if d == 0 {
            1.0
        } else if d == 1 {
            // 单个丢失余类：只要有 >= 1 个冗余包到达就能解
            1.0 - p.powi((r - 1) as i32)
        } else {
            // d 个不同余类各丢 1 个：
            //   - 恰好 d 个冗余包到达（无碰撞）-> 一定可解：q^d
            //   - 恰有 d-1 个到达（1 个碰撞）-> 剩下的 r-d+1 个里任意一个到达即可
            let dd = d as f64;
            q.powi(d as i32)
                + dd * p * q.powi((d - 1) as i32) * (1.0 - p.powi(r.saturating_sub(d) as i32))
        };
        sum += coef * cover;
    }
    (q.powi(k as i32) * sum).clamp(0.0, 1.0)
}

/// 让「全帧可解概率 >= target」所需的最小冗余包数（精确模型，不钳带宽）
///
/// 与 [`suggest_redundant`] 的区别：后者用二项 CDF（高估），本函数用
/// [`exact_recovery_probability`]（与真实解码器一致）。
///
/// # 为什么关键帧必须用这个
///
/// 二项模型在 `k=35, p=5%` 下说 r=9 就够 99.9%，真实只有 89.5%；要达到真实的
/// 99.9% 需要 r=35（而且只有 99.65%，还差一点）。用二项模型给关键帧算冗余，
/// 等于**系统性地欠保护 4 倍**。
///
/// # 边界
///
/// - `k == 0` -> 0
/// - `p <= 0` / `target <= 0` -> 0
/// - 目标不可达（`target` 高于 `r=k` 时的可解率）-> 返回 `k`
///   （调用方该用 [`clamp_keyframe_redundant`] 把它钳回预算内）
pub fn suggest_redundant_exact(k: usize, loss_rate: f64, target: f64) -> usize {
    if k == 0 {
        return 0;
    }
    let p = if loss_rate.is_nan() {
        0.0
    } else {
        loss_rate.clamp(0.0, 1.0)
    };
    let target = if target.is_nan() {
        DEFAULT_RECOVERY_TARGET
    } else {
        target.clamp(0.0, 1.0)
    };
    if p <= 0.0 || target <= 0.0 {
        return 0;
    }
    for r in 0..=k {
        if exact_recovery_probability(k, r, p) >= target {
            return r;
        }
    }
    k
}

/// 关键帧冗余度的带宽钳制
///
/// `cap = floor(fragments * KEYFRAME_MAX_OVERHEAD_RATIO)`，再取 `min(want, cap)`。
///
/// # 为什么 `cap` 至少是 1（而不是 0）
///
/// 4 个分片的关键帧按 50% 算 `floor(4 * 0.5) = 2`，没问题；但 1 个分片时
/// `floor(0.5) = 0`，等于完全不保护。而 1 片的关键帧恰恰是最脆弱的（丢这 1 片
/// = 整帧废）。所以有分片时至少给 1 个冗余包 —— 它的开销是「1 个冗余包 /
/// 1 个数据包」= 100%，但绝对字节数极小（1160B），而且只发生在「整帧只有
/// 一个分片」这种罕见情况下。
///
/// 反过来说：`fragments == 0`（空帧）返回 0，没有东西可保护。
///
/// # 边界
///
/// - `fragments == 0` -> 0
/// - `want == 0` -> 0（调用方明确不要冗余）
/// - `want > cap` -> `cap`
pub fn clamp_keyframe_redundant(fragments: usize, want: usize) -> usize {
    if fragments == 0 || want == 0 {
        return 0;
    }
    let cap = ((fragments as f64 * KEYFRAME_MAX_OVERHEAD_RATIO).floor() as usize).max(1);
    want.min(cap)
}

/// 按帧类型给出冗余度建议（**这是发送端该调的那个函数**）
///
/// # 关键帧与 P 帧走两条不同的路
///
/// | 帧类型 | 模型 | 目标 | 上限 |
/// | --- | --- | --- | --- |
/// | 关键帧 | [`suggest_redundant_exact`]（精确） | [`KEYFRAME_RECOVERY_TARGET`] = 0.999 | [`clamp_keyframe_redundant`] |
/// | P 帧 | [`suggest_redundant`]（沿用现有二项模型，**行为不变**） | `target` 参数 | 无（由调用方的开销闸门管） |
///
/// # 为什么 P 帧不改
///
/// 1. P 帧只有 2 个分片，二项模型与精确模型的差距很小
///    （`k=2, p=5%`：二项说 r=1 够，真实 99.28%），没有改的必要。
/// 2. 现有 59 个 `fec_link` 测试与发送端的自适应逻辑都依赖 `suggest_redundant`
///    的既有行为，改它等于同时改两条链路。
///
/// # 单调性保证（`tests/fec_keyframe.rs` 的 `keyframe_never_less_than_delta` 守着）
///
/// 对 `fragments >= 8`（关键帧的真实尺寸段），返回的关键帧冗余度**恒 >=**
/// 同条件下 P 帧的冗余度。`fragments < 8` 时不保证 —— 那是带宽上限（50% 开销）
/// 比二项模型的建议更紧导致的，属于刻意的取舍：小帧走 NACK 比烧 50% 带宽划算。
///
/// # 边界
///
/// - `fragments == 0` -> 0
/// - `loss_rate <= 0` -> 0（不丢包就不付冗余）
pub fn suggest_redundant_for_frame(class: FrameClass, loss_rate: f64, target: f64) -> usize {
    let k = class.fragments;
    if k == 0 {
        return 0;
    }
    if class.is_keyframe {
        let want = suggest_redundant_exact(k, loss_rate, target.max(KEYFRAME_RECOVERY_TARGET));
        // 兜底：精确模型在极小 k 下可能给出比二项模型更小的值（两者对 p 的
        // 敏感度不同）。关键帧**永远不该**比同条件的 P 帧拿到的冗余更少 ——
        // 那是逻辑矛盾，不是优化。
        let floor = suggest_redundant(k, loss_rate, target);
        clamp_keyframe_redundant(k, want.max(floor))
    } else {
        suggest_redundant(k, loss_rate, target)
    }
}
