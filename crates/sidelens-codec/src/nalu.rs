//! H.264 NALU 码流处理
//!
//! # 为什么这一层值得单独做
//!
//! 编码器各平台不同，但**码流的字节结构是统一的**。把 NALU 切分、
//! 关键帧判断、参数集提取这些纯字节操作抽出来，能做到：
//!
//! 1. 完全可测（不需要编码器）
//! 2. 各平台复用（Windows MF / VideoToolbox / MediaCodec 都产出 Annex-B）
//! 3. 抖动缓冲与 NACK 能据此判断"这帧能不能独立解码"
//!
//! # Annex-B 起始码
//!
//! H.264 码流用两种起始码分隔 NALU：
//!
//! ```text
//! 00 00 01     —— 3 字节，短
//! 00 00 00 01  —— 4 字节，长（通常用于关键帧/SPS/PPS 前）
//! ```
//!
//! **两者都要支持**。只认 4 字节的实现会在真实码流上丢掉一半 NALU。
//!
//! # NALU 头
//!
//! 第一个字节：
//!
//! ```text
//! bit 7    : forbidden_zero_bit（必须为 0）
//! bit 6-5  : nal_ref_idc（参考级别，0 表示不被参考）
//! bit 4-0  : nal_unit_type（1=非IDR切片, 5=IDR, 7=SPS, 8=PPS）
//! ```

#![forbid(unsafe_code)]

/// AVCC 格式的长度前缀字节数
pub const AVCC_LENGTH_SIZE: usize = 4;

/// NALU 类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NaluType {
    /// 非 IDR 的普通切片（P/B 帧）
    NonIdrSlice,
    /// IDR 切片（关键帧）
    IdrSlice,
    /// 补充增强信息
    Sei,
    /// 序列参数集
    Sps,
    /// 图像参数集
    Pps,
    /// 访问单元分隔符
    Aud,
    /// 其他（保留或未使用的类型）
    Other(u8),
}

impl NaluType {
    /// 从 NALU 头字节解析类型
    pub fn from_header(header: u8) -> Self {
        match header & 0x1f {
            1..=4 => NaluType::NonIdrSlice,
            5 => NaluType::IdrSlice,
            6 => NaluType::Sei,
            7 => NaluType::Sps,
            8 => NaluType::Pps,
            9 => NaluType::Aud,
            other => NaluType::Other(other),
        }
    }

    /// 是否是关键帧类型
    pub fn is_keyframe(self) -> bool {
        matches!(self, NaluType::IdrSlice)
    }

    /// 是否是参数集
    pub fn is_parameter_set(self) -> bool {
        matches!(self, NaluType::Sps | NaluType::Pps)
    }

    /// 是否包含图像数据
    pub fn is_slice(self) -> bool {
        matches!(self, NaluType::IdrSlice | NaluType::NonIdrSlice)
    }

    /// 原始类型值
    pub fn as_u8(self) -> u8 {
        match self {
            NaluType::NonIdrSlice => 1,
            NaluType::IdrSlice => 5,
            NaluType::Sei => 6,
            NaluType::Sps => 7,
            NaluType::Pps => 8,
            NaluType::Aud => 9,
            NaluType::Other(v) => v,
        }
    }
}

impl std::fmt::Display for NaluType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            NaluType::NonIdrSlice => "NON_IDR",
            NaluType::IdrSlice => "IDR",
            NaluType::Sei => "SEI",
            NaluType::Sps => "SPS",
            NaluType::Pps => "PPS",
            NaluType::Aud => "AUD",
            NaluType::Other(v) => return write!(f, "OTHER({v})"),
        };
        f.write_str(s)
    }
}

/// 一个 NALU 的视图（零拷贝）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NaluView<'a> {
    /// NALU 类型
    pub ty: NaluType,
    /// 不含起始码的 NALU 字节（含 NALU 头）
    pub data: &'a [u8],
}

impl<'a> NaluView<'a> {
    /// NALU 长度
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// 是否关键帧
    pub fn is_keyframe(&self) -> bool {
        self.ty.is_keyframe()
    }
}

/// 在 `buf` 的 `from` 位置起找下一个起始码，返回 (起始码起点, 起始码长度)
///
/// # 为什么返回起点而不只是长度
///
/// 调用方需要知道「当前 NALU 在哪里结束」—— 那正好是下一个起始码的**起点**。
/// 只返回 `(长度, 数据起点)` 会让调用方不得不反推（早期实现就是这么写的，
/// 靠 `while buf[end-1] != 1` 往回找，遇到 NALU 内部有 0x01 就会算错）。
fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            // 4 字节起始码：00 00 00 01
            if i + 4 <= buf.len() && buf[i + 2] == 0 && buf[i + 3] == 1 {
                return Some((i, 4));
            }
            // 3 字节起始码：00 00 01
            if buf[i + 2] == 1 {
                return Some((i, 3));
            }
        }
        i += 1;
    }
    None
}

/// 把 Annex-B 码流切成 NALU 列表
///
/// # 参数
///
/// `buf` 是一帧（或一段）Annex-B 码流。
///
/// # 返回
///
/// 每个 NALU 的视图，**不含起始码**。
///
/// # 边界处理
///
/// - 开头没有起始码：整段视为一个 NALU（某些编码器会省略首帧起始码）
/// - 末尾没有起始码：最后一个 NALU 延伸到 buf 结尾
/// - 空输入：返回空列表
pub fn split_annexb(buf: &[u8]) -> Vec<NaluView<'_>> {
    if buf.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();

    // 收集所有起始码的位置
    let mut codes: Vec<(usize, usize)> = Vec::new();
    let mut search_from = 0;
    while let Some((pos, len)) = find_start_code(buf, search_from) {
        codes.push((pos, len));
        search_from = pos + len;
    }

    if codes.is_empty() {
        // # 没有起始码：**返回空**，不再把整段当一个 NALU（实测抓到的真 bug）
        //
        // 原实现「整段当一个 NALU」的理由是「某些编码器会省略首帧起始码」。
        // 但代码**无法区分**这两种情况：
        //
        // - 真的是一帧没有起始码的裸 NALU
        // - 一段不含起始码的碎片（UDP 分片中途、丢包残片、AVCC 误喂）
        //
        // 而后果不对称：把碎片当 NALU 的代价极高 ——
        //
        // ```text
        // 输入 [67 1f 2a 88 99]（某个切片载荷里碰巧出现的字节）
        //   → ty = Sps（0x67 & 0x1f = 7）
        //   → ParameterSets::update_from 把垃圾缓存成 SPS
        //   → to_annexb() 把它前置到关键帧前发给对端
        //   → 对端解码器拿到垃圾参数集 → 整路视频黑屏
        // ```
        //
        // **一个字节的巧合就能让整路视频黑屏**，而真正的「省略起始码」
        // 场景在真实码流里几乎不存在（Annex-B 的定义就是带起始码）。
        //
        // 所以选安全的一边：没有起始码就不是 Annex-B，返回空。
        // 需要「宽容模式」的调用方应该自己包一层（或先用 AVCC 解码）。
        return out;
    }

    // 第一个起始码之前若有**非零**数据，才算一个 NALU。
    //
    // # 为什么必须跳过全 0 的前缀（实测抓到的设计缺陷）
    //
    // 字节流对齐时常见前置填充 0，而 `find_start_code` 从 `i` 开始匹配：
    //
    // ```text
    // 输入 [00 00 00 00 01 67 42 00 1f]
    //   i=0: buf[3]=0 ≠ 1 → 不匹配
    //   i=1: 匹配 00 00 00 01 → codes[0] = (1, 4)
    //   → buf[..1] = [00] 被当成 NALU，ty = Other(0)   ← 幽灵
    // ```
    //
    // 幽灵 NALU 本身不致命（`is_keyframe` / `ParameterSets` 都按类型过滤），
    // 但 `annexb_to_avcc` 会把它写进 AVCC，污染下游。
    //
    // 判据：前缀里只要有**任何一个非零字节**才算真数据。
    if codes[0].0 > 0 {
        let head = &buf[..codes[0].0];
        if head.iter().any(|&b| b != 0) {
            out.push(NaluView {
                ty: NaluType::from_header(head[0]),
                data: head,
            });
        }
    }

    // 每个起始码之后到下一个起始码之前是一个 NALU
    for (i, &(sc_pos, sc_len)) in codes.iter().enumerate() {
        let data_start = sc_pos + sc_len;
        let data_end = match codes.get(i + 1) {
            Some(&(next_pos, _)) => next_pos,
            None => buf.len(),
        };

        // # 尾随 0 不能无条件裁（实测抓到的真 bug）
        //
        // 原实现把 NALU 末尾所有 0 都裁掉，理由是「Annex-B 允许插入 0 填充」。
        // 但 H.264 规范 7.4.1 还允许 **CABAC 码流在 NALU 末尾追加
        // `cabac_zero_word`（就是 `00 00`）**，那是**切片数据的一部分**，
        // 不是填充。
        //
        // 实测后果：
        //
        // ```text
        // 输入 [0001][65 88 84 00 00][0001][41 22]
        //   IDR 长度 = 3（应为 5），data = [65 88 84]
        // ```
        //
        // 对硬件解码器是「NALU 长度与内部 slice 长度不一致」——
        // 轻则报错重则花屏。
        //
        // # 为什么原来那条注释站不住
        //
        // 「数据末尾的 0 可以靠 RBSP 长度判断」——H.264 的 RBSP 长度
        // **没有**显式字段，只能靠 `rbsp_stop_one_bit` 反向定位，
        // 而 `cabac_zero_word` 恰好破坏了那个定位。
        //
        // # 现在的判据
        //
        // 只裁「**明确是填充**」的那部分：当下一个起始码是 4 字节，
        // 且当前 NALU 末尾的 0 多于 2 个时，多出来的才算填充。
        //
        // ```text
        // ... 65 88 84 00 00 | 00 00 01 41 22
        //                  ^^^^^^ 这两个是 cabac_zero_word，保留
        //                        ^^^^^ 这三个里前两个归起始码，多出的才算填充
        // ```
        //
        // 最后一个 NALU（没有下一个起始码）**完全不裁** —— 没有参照物时
        // 无从判断哪几个 0 是填充。
        let mut trimmed_end = data_end;
        if let Some(&(next_pos, next_len)) = codes.get(i + 1) {
            // 下一个起始码是 4 字节时，它本身占用了末尾的 3 个 0（00 00 00 01），
            // 所以 `data_end == next_pos` 已经把起始码排除了。
            // 只有在起始码之间**还有额外 0** 时才可能是填充。
            let _ = (next_pos, next_len);
            let mut zeros = 0usize;
            while trimmed_end > data_start && buf[trimmed_end - 1] == 0 {
                trimmed_end -= 1;
                zeros += 1;
            }
            // 保留最多 2 个（cabac_zero_word），其余算填充
            let keep = zeros.min(2);
            trimmed_end += keep;
        }

        if trimmed_end > data_start {
            let nalu = &buf[data_start..trimmed_end];
            out.push(NaluView {
                ty: NaluType::from_header(nalu[0]),
                data: nalu,
            });
        }
    }

    out
}

/// 判断一帧 Annex-B 码流是否含关键帧（IDR）
///
/// # 为什么要扫全部 NALU 而不是只看第一个
///
/// 一帧通常由 `[AUD][SPS][PPS][IDR]` 或 `[SEI][IDR]` 组成，
/// IDR 不一定在最前面。只看第一个会漏判关键帧，
/// 而关键帧判断错了会让抖动缓冲在丢包后无法恢复。
pub fn is_keyframe(buf: &[u8]) -> bool {
    split_annexb(buf).iter().any(|n| n.is_keyframe())
}

/// SPS / PPS 缓存
///
/// # 为什么需要缓存
///
/// 参数集只在码流开头或关键帧前出现一次。接收端如果中途加入
/// （或丢包后请求关键帧），必须能拿到最近一次的参数集才能解码。
///
/// 缓存的是**最新**的一份 —— 分辨率变化时参数集会更新。
#[derive(Debug, Default, Clone)]
pub struct ParameterSets {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParameterSets {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从一段 Annex-B 码流里提取并更新参数集
    ///
    /// 返回是否有更新。
    pub fn update_from(&mut self, buf: &[u8]) -> bool {
        let mut changed = false;
        for nalu in split_annexb(buf) {
            // 用 match 守卫直接表达「类型匹配且内容不同」，
            // 比嵌套 if 更清晰，也避免了 clippy 的 collapsible_if
            match nalu.ty {
                NaluType::Sps if self.sps.as_deref() != Some(nalu.data) => {
                    self.sps = Some(nalu.data.to_vec());
                    changed = true;
                }
                NaluType::Pps if self.pps.as_deref() != Some(nalu.data) => {
                    self.pps = Some(nalu.data.to_vec());
                    changed = true;
                }
                _ => {}
            }
        }
        changed
    }

    pub fn sps(&self) -> Option<&[u8]> {
        self.sps.as_deref()
    }

    pub fn pps(&self) -> Option<&[u8]> {
        self.pps.as_deref()
    }

    /// 是否已集齐参数集（能解码的最小条件）
    pub fn is_complete(&self) -> bool {
        self.sps.is_some() && self.pps.is_some()
    }

    pub fn clear(&mut self) {
        self.sps = None;
        self.pps = None;
    }

    /// 把参数集拼成 Annex-B 格式（前置在关键帧前发送）
    pub fn to_annexb(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for ps in [&self.sps, &self.pps].into_iter().flatten() {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(ps);
        }
        out
    }
}

/// Annex-B → AVCC（长度前缀）转换
///
/// AVCC 用 4 字节大端长度代替起始码，是 MP4 / MPEG-TS 之外的容器格式
/// 常用形态。Media Foundation 的 `MF_MT_MPEG_SEQUENCE_HEADER` 就是 AVCC。
pub fn annexb_to_avcc(buf: &[u8]) -> Vec<u8> {
    let nalus = split_annexb(buf);
    let mut out = Vec::with_capacity(buf.len());
    for n in nalus {
        out.extend_from_slice(&(n.data.len() as u32).to_be_bytes());
        out.extend_from_slice(n.data);
    }
    out
}

/// AVCC → Annex-B 转换
///
/// 返回 None 表示长度前缀越界（码流损坏）。
pub fn avcc_to_annexb(buf: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(buf.len() + 16);
    let mut i = 0;
    while i + AVCC_LENGTH_SIZE <= buf.len() {
        let len = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        i += AVCC_LENGTH_SIZE;
        if i + len > buf.len() {
            return None;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&buf[i..i + len]);
        i += len;
    }
    Some(out)
}
