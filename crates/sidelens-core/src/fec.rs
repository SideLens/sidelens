//! XOR 奇偶校验 FEC（前向纠错）
//!
//! # 为什么需要它
//!
//! 同 crate 的 [`crate::nack`] 已经能发现丢包并请求重传，但重传至少要 1 个 RTT：
//!
//! - 局域网 RTT 1ms，重传来了还能赶上播放时刻
//! - 跨城 40ms、跨境 150ms，等重传回来这一帧早该渲染了
//!
//! FEC 用**额外带宽换 0 延迟的恢复**：发送端把冗余包和原始包一起发出去，
//! 接收端丢包时用冗余包当场反解，不需要任何往返。代价是不管丢不丢包，
//! 冗余包都要占带宽。
//!
//! # 什么时候划算（决定上层要不要创建这个编码器）
//!
//! | 丢包率 | 做法 | 理由 |
//! | --- | --- | --- |
//! | 0-1% | 不启用 FEC | 99% 的冗余包永远用不上，纯白费带宽 |
//! | 1-10% | 启用，`redundant` 取 1-2 | 1 个 RTT 的重传延迟比 10-20% 冗余更贵 |
//! | >10% | 不加 FEC，降码率 / 请求关键帧 | 冗余开销随丢包率线性增长，且组内丢失超出可解范围时 FEC 完全失效 |
//!
//! [`FecConfig::validate`] 要求 `redundant >= 1`，所以"关闭 FEC"的做法是
//! **根本不创建 [`FecEncoder`]**，而不是把 `redundant` 设成 0。
//!
//! # 冗余包怎么算
//!
//! 第 `k` 个冗余包 `R_k` = 组内所有满足 `i % redundant != k` 的原始包
//! `P_i` 逐字节 XOR（变长包先补齐到组内最长长度，尾部补 0）。
//!
//! 等价说法：把 `group_size` 个原始包按 `i % redundant` 分成 `redundant` 个余类，
//! `R_k` 就是"除第 `k` 类之外所有包"的 XOR。
//!
//! `redundant == 1` 是特例：余类只有一个（`i % 1 == 0` 恒成立），照公式会得到
//! 一个全 0、毫无用处的冗余包。所以这里退化成经典的单奇偶校验 ——
//! `R_0` = 组内**全部**原始包的 XOR，任意单丢都能恢复。
//!
//! # 反解：为什么必须解方程组，不能一个包一个包地套公式
//!
//! 只丢一个包时确实可以套公式：`R_k`（`k = m % redundant`）恰好排除了 `m` 自己，
//! 于是 `P_m = R_k XOR（所有参与 R_k 的已收到包）`。
//!
//! 但丢两个包就不成立了。设丢了 `P_0`（第 0 类）和 `P_1`（第 1 类），`redundant = 2`：
//! `R_0` 排除第 0 类、包含 `P_1`，所以 `R_0 XOR（其余已收到的奇数包）` 得到的是
//! `P_1` 而不是 `P_0`。**把算错的数据当成"已恢复"交给上层比丢包更糟**：
//! 解码器拿到的是垃圾帧，还可能连带污染后续所有 P 帧。所以"逐包套公式"这条路是错的。
//!
//! 正确做法是把整组看成一个 GF(2) 上的线性方程组：
//!
//! - 未知数 `x_m` = 缺失的原始包（`m` 遍历缺失集合）
//! - 每个收到的冗余包给出一条方程：
//!   `XOR{ m 缺失且 m % redundant != k } x_m = R_k XOR（所有参与 R_k 的已收到包）`
//!
//! 方程数 `<= redundant`、未知数 `<= redundant`，规模极小（默认最多 2 个），
//! 直接做高斯-约当消元即可。**只有化简成单位行的未知数才会返回**
//! （该行除本列外没有其它未知数系数），欠定的宁可返回空，也绝不返回错误数据。
//!
//! # 这套方案真正能救什么（务必读完，别被"可恢复 redundant 个"误导）
//!
//! **纯 XOR 做不到"任意 t 丢都能恢复"**：GF(2) 上不存在非平凡的 MDS 码。
//! 想让任意 t 个擦除都可解，必须上 GF(256) 的 Reed-Solomon。本方案的实际能力
//! 由"缺失包落在几个余类"决定。下表是在 `group_size = 10` 上穷举全部丢包组合
//! 实测出来的（`tests/fec.rs` 里的 `exhaustive_recovery_never_returns_wrong_bytes`
//! 会在 `group_size = 8` 上重跑同样的穷举，保证这张表不会随代码改动失真）：
//!
//! | 配置 | 丢 1 个 | 丢 2 个 | 丢 3 个 |
//! | --- | --- | --- | --- |
//! | `redundant = 2` | 10/10 全恢复 | 25/45 全恢复（20 种同余类无解） | 超过冗余数，0 |
//! | `redundant = 3` | 10/10 全恢复 | 33/45 全恢复（12 种同余类无解） | 0/120 全恢复，78/120 能救出 1 个 |
//! | `redundant = 4` | 10/10 全恢复 | 37/45 全恢复（8 种同余类无解） | 60/120 全恢复，58/120 能救出 1 个 |
//!
//! 规律：
//!
//! 1. **单丢**：一定能恢复，只要收到过任意一个冗余包。
//! 2. **多丢**：每个余类最多救回 1 个包 —— 一个冗余包只提供一条方程。
//!    所以 t 个缺失落在 t 个不同余类时才可能全解；两个包同余类就废掉那一条方程。
//! 3. **部分恢复是常态**：解不出的未知数跳过，解得出的照常返回。
//!    `redundant = 3` 丢 3 个时经常能救回 1 个（那个包独占一个余类），
//!    剩下 2 个交给 NACK —— 这比"要么全解要么全弃"实用得多。
//! 4. `redundant = group_size` 时每个余类只有一个成员，退化得最好
//!    （每个冗余包恰好排除一个原始包），但带宽开销 100%，实时视频用不起。
//!
//! 因此**不要指望"把 redundant 从 2 调到 3 就能救任意三丢"**：它只是把双丢的
//! 失败率从 20/45 降到 12/45。真要抗突发丢失，靠的是 NACK 兜底 + 降码率，
//! 而不是继续堆冗余。本模块选纯 XOR 的理由是：零依赖、每字节一次 XOR、
//! 几十行能看懂，覆盖"随机单丢"这个最常见、也最值得用 0 延迟去救的场景。
//!
//! # 其他设计取舍与已知限制
//!
//! 1. **恢复出来的包长度是补齐长度**。XOR 要求等长，变长包按组内最长包尾部补 0；
//!    冗余包里只有 XOR 结果、没有长度表，所以恢复出的丢失包长度 = 组内最长包长度，
//!    多出来的尾部是 0。调用方要自己按 NAL / 帧边界截断（H.264 的 length-prefixed
//!    NAL 或起始码都能定位真实边界）。也不能靠"去掉尾部 0"猜长度：
//!    原始数据本身就可能以 0 结尾。本项目真实流量是分片后的 MTU 满包
//!    （见 [`crate::fragment::SAFE_MTU_PAYLOAD`]），长度基本一致，这个误差通常是 0。
//! 2. **不做长度表**。想精确还原长度，可以让冗余包额外携带 `group_size` 个 u16 长度，
//!    但那要改线上包格式；本模块只产出纯 XOR 字节，格式由上层（帧头 + 分片）决定。
//! 3. **超长包不截断**。`max_packet_size` 只用于配置校验与带宽预算，
//!    [`FecEncoder::push`] 不会截断超长包（截断 = 静默损坏数据）。超长包会把冗余包
//!    撑到同样长、可能超过 MTU，所以上层应当先分片到 `SAFE_MTU_PAYLOAD` 以内。
//! 4. **组延迟**。必须凑满 `group_size` 个包才能算冗余包：组越大恢复能力越强，
//!    但组内最后一个包要等整组凑齐才被保护。实时视频建议
//!    `group_size * 帧间隔 <= 抖动缓冲深度`（默认 10 包 ≈ 160ms @60fps）。
//! 5. **收尾的半组不受保护**。流结束、丢帧或切换分辨率时当前组没满，
//!    [`FecEncoder::take_redundant`] 返回空（不产出残缺冗余包，因为接收端无法区分
//!    "组只到这里"和"后面的包全丢了"）。确实要保护尾组，就用长度为 0 的空包
//!    把半组补齐 —— 空包本身合法，接收端会当成正常收到的包参与 XOR。
//! 6. **两端配置必须一致**。`redundant` 或 `group_size` 对不上，`i % redundant` 就错位，
//!    恢复出来全是垃圾数据而且不会报错。配置属于会话协商的一部分。
//! 7. **重复包先到为准**。同一下标重复投递会被忽略，避免一个损坏的副本污染 XOR。
//!
//! # 和 NACK 的关系
//!
//! 两者互补而非二选一：FEC 先手救回可解范围内的丢失（0 延迟），
//! 救不回来的（同余类冲突、丢得太多、冗余包自己也丢了）再交给 NACK 走 1 RTT。
//! 典型流程：收齐本组数据包与冗余包 -> [`FecDecoder::recover`] ->
//! 仍然缺失的下标转成 NACK 请求 -> [`FecDecoder::next_group`] 切组。

#![forbid(unsafe_code)]

/// FEC 配置
///
/// 三个字段都是"每组"的粒度：一个组 = `group_size` 个原始包 + `redundant` 个冗余包。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecConfig {
    /// 每组多少个原始包
    pub group_size: usize,
    /// 每组多少个冗余包（1..=group_size）
    pub redundant: usize,
    /// 单个包最大字节数
    ///
    /// 只用于配置校验与带宽预算，本模块**不做截断**（截断等于静默损坏数据）。
    pub max_packet_size: usize,
}

impl Default for FecConfig {
    fn default() -> Self {
        Self {
            group_size: 10,
            redundant: 2,
            max_packet_size: 1200,
        }
    }
}

impl FecConfig {
    /// 冗余占比（`redundant / group_size`）
    ///
    /// 默认配置返回 0.2，也就是多花 20% 带宽。丢包率远低于这个值时 FEC 是纯浪费；
    /// 丢包率明显高于这个值时单靠 FEC 救不回来，应当降码率而不是加冗余。
    ///
    /// `group_size` 为 0 时返回 0.0（避免除零 panic；这种配置会被
    /// [`FecConfig::validate`] 拒绝，正常流程走不到）。
    pub fn overhead_ratio(&self) -> f64 {
        if self.group_size == 0 {
            return 0.0;
        }
        self.redundant as f64 / self.group_size as f64
    }

    /// 每组冗余包的最坏情况字节数（`redundant * max_packet_size`）
    ///
    /// 用来估带宽：默认配置每组最多 `2 * 1200 = 2400` 字节冗余，
    /// 相对 `10 * 1200 = 12000` 字节原始数据正好 20%，与 [`FecConfig::overhead_ratio`] 一致。
    /// 实际冗余包长度等于组内最长包，通常比这个上限小。
    pub fn redundant_budget(&self) -> usize {
        self.redundant.saturating_mul(self.max_packet_size)
    }

    /// 校验配置是否可用
    ///
    /// 返回 `Err` 的场景：`group_size == 0`、`redundant == 0`、
    /// `redundant > group_size`、`max_packet_size == 0`。
    /// 错误信息是给人看的简体中文，直接打到日志里即可。
    pub fn validate(&self) -> Result<(), String> {
        if self.group_size == 0 {
            return Err("FEC 配置非法：group_size 不能为 0".to_string());
        }
        if self.redundant == 0 {
            return Err(
                "FEC 配置非法：redundant 不能为 0（不需要 FEC 时请别创建编码器）".to_string(),
            );
        }
        if self.redundant > self.group_size {
            return Err(format!(
                "FEC 配置非法：redundant({}) 不能大于 group_size({})",
                self.redundant, self.group_size
            ));
        }
        if self.max_packet_size == 0 {
            return Err("FEC 配置非法：max_packet_size 不能为 0".to_string());
        }
        Ok(())
    }
}

/// 把 `src` 逐字节 XOR 进 `acc`
///
/// `src` 比 `acc` 短时只 XOR 前缀部分，等价于把 `src` 尾部补 0 到 `acc` 长度
/// （0 不改变 XOR 结果）。这就是"变长包补齐到最长包"的实现方式，
/// 不需要真的分配补齐缓冲，省一次拷贝。
fn xor_into(acc: &mut [u8], src: &[u8]) {
    for (a, b) in acc.iter_mut().zip(src.iter()) {
        *a ^= *b;
    }
}

/// FEC 编码器（发送端用）
///
/// 按推入顺序把原始包攒成一组，凑满 `group_size` 个后由 [`FecEncoder::take_redundant`]
/// 一次性算出全部冗余包。**组内下标就是推入顺序**：第 `i` 次推入即 `P_i`，
/// 接收端靠这个下标算 `i % redundant`，所以两端必须用同一套编号。
#[derive(Debug)]
pub struct FecEncoder {
    config: FecConfig,
    /// 当前组已推入的原始包（下标即组内下标）
    group: Vec<Vec<u8>>,
    /// 已凑满的组数（每凑满一组 +1，单调递增）
    group_index: u32,
}

impl FecEncoder {
    /// 用给定配置创建编码器；配置非法时返回 `Err`
    pub fn new(config: FecConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            group: Vec::with_capacity(config.group_size),
            config,
            group_index: 0,
        })
    }

    /// 当前配置
    pub fn config(&self) -> FecConfig {
        self.config
    }

    /// 推入一个原始包，返回本组是否凑满
    ///
    /// - 空包（长度 0）合法：收尾时可以用它把半组补齐，让尾组也能被保护
    /// - 组已满时不接收新包（返回 `false` 并丢弃），调用方应先
    ///   [`FecEncoder::take_redundant`] 把冗余包取走
    /// - 不做 `max_packet_size` 截断，超长包请先分片
    pub fn push(&mut self, payload: &[u8]) -> bool {
        if self.is_full() {
            return false;
        }
        self.group.push(payload.to_vec());
        if self.is_full() {
            self.group_index = self.group_index.saturating_add(1);
            true
        } else {
            false
        }
    }

    /// 本组是否已满
    pub fn is_full(&self) -> bool {
        self.group.len() >= self.config.group_size
    }

    /// 取出冗余包（组满时调用）
    ///
    /// 返回 `redundant` 个 `Vec<u8>`，每个长度都等于本组最长包的字节数
    /// （整组都是空包时为 0）。取走后当前组被清空，可以继续推下一组。
    ///
    /// 未满时返回空 `Vec` 且**不清空**当前组：残缺的冗余包没有意义，
    /// 接收端无法区分"组只到这里"和"后面的包都丢了"。
    pub fn take_redundant(&mut self) -> Vec<Vec<u8>> {
        if !self.is_full() {
            return Vec::new();
        }

        let redundant = self.config.redundant;
        let max_len = self.group.iter().map(|p| p.len()).max().unwrap_or(0);

        let mut out = Vec::with_capacity(redundant);
        for k in 0..redundant {
            let mut acc = vec![0u8; max_len];
            for (i, payload) in self.group.iter().enumerate() {
                // redundant == 1 时余类只有一个，照公式会排除全部包，特例成"包含全部"
                if redundant > 1 && i % redundant == k {
                    continue;
                }
                xor_into(&mut acc, payload);
            }
            out.push(acc);
        }

        self.group.clear();
        out
    }

    /// 当前组内已有的包数
    pub fn pending(&self) -> usize {
        self.group.len()
    }

    /// 已凑满的组数（每凑满一组 +1）
    ///
    /// 在 `push` 让本组变满的那一刻就 +1，[`FecEncoder::take_redundant`] 不再改动它。
    /// 所以它表示"完成了多少组"，而不是"当前是第几组"。
    pub fn group_index(&self) -> u32 {
        self.group_index
    }

    /// 强制清空当前组（丢帧或切换分辨率时用）
    ///
    /// 只丢弃半成品，不回退 [`FecEncoder::group_index`]：组序号是单调的统计量，
    /// 拿它做日志关联时不该因为一次丢帧就出现重复编号。
    pub fn reset(&mut self) {
        self.group.clear();
    }
}

/// FEC 解码器（接收端用）
///
/// 接收端不知道"哪一组丢了哪些包"，只知道组内下标 `0..group_size` 里哪些到了。
/// 原始包和冗余包都往这里投，然后调 [`FecDecoder::recover`] 一次性尝试反解。
///
/// # 为什么不需要单独的长度表
///
/// 收到的原始包自己就带着长度（`Vec::len`），额外维护一份 `lengths` 数组反而
/// 容易和包数据不同步。丢失的包本来就没有长度信息，只能返回补齐长度，
/// 这一点在 [`FecDecoder::recover`] 的文档里说明。
#[derive(Debug)]
pub struct FecDecoder {
    config: FecConfig,
    /// 组内原始包（下标即组内下标，`None` = 还没收到）
    originals: Vec<Option<Vec<u8>>>,
    /// 收到的冗余包（下标即 `k`）
    redundants: Vec<Option<Vec<u8>>>,
    /// 累计成功恢复的包数（跨组累加，[`FecDecoder::next_group`] 不清零）
    recovered_count: u64,
}

impl FecDecoder {
    /// 用给定配置创建解码器；配置非法时返回 `Err`
    ///
    /// 两端配置必须一致，否则 `i % redundant` 对不上，恢复出来的全是垃圾数据。
    pub fn new(config: FecConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            originals: vec![None; config.group_size],
            redundants: vec![None; config.redundant],
            config,
            recovered_count: 0,
        })
    }

    /// 当前配置
    pub fn config(&self) -> FecConfig {
        self.config
    }

    /// 记录收到一个原始包，`index` 是本组内的下标（0..group_size）
    ///
    /// 下标越界或同一下标重复投递都会被忽略（**先到的为准**）：
    /// 重复包内容本来相同，保留首个可以避免被一个损坏的副本污染 XOR。
    pub fn push_original(&mut self, index: usize, payload: &[u8]) {
        if index >= self.config.group_size {
            return;
        }
        if self.originals[index].is_some() {
            return;
        }
        self.originals[index] = Some(payload.to_vec());
    }

    /// 记录收到一个冗余包，`k` 是冗余包下标（0..redundant）
    ///
    /// 同样先到为准；`k` 越界直接忽略。
    pub fn push_redundant(&mut self, k: usize, payload: &[u8]) {
        if k >= self.config.redundant {
            return;
        }
        if self.redundants[k].is_some() {
            return;
        }
        self.redundants[k] = Some(payload.to_vec());
    }

    /// 尝试恢复丢失的包，返回恢复出来的 `(组内下标, 载荷)` 列表
    ///
    /// 触发条件（任一不满足就返回空 `Vec`，且不改动任何状态）：
    ///
    /// - 确实有包缺失
    /// - 缺失数 `<= redundant`（丢得更多说明这一组已经废了，能救回一半也拼不出完整帧，
    ///   交给上层降码率 / 请求关键帧比"部分恢复"更清晰）
    ///
    /// 之后按模块文档的方程组解法做高斯-约当消元：**只有数学上确定的未知数才会返回**，
    /// 欠定的（比如两个包落在同一余类）直接跳过，宁可少返回也不返回错误数据。
    /// 所以返回的列表可能比缺失数少，剩下的请交给 NACK。
    ///
    /// 恢复出的载荷长度 = 组内最长包长度（尾部是补齐的 0，不是真实数据），
    /// 调用方需要按 NAL / 帧边界自己截断。恢复成功的包会被记入本组，
    /// 所以再次调用不会重复返回（幂等），[`FecDecoder::missing`] 也会相应归零。
    pub fn recover(&mut self) -> Vec<(usize, Vec<u8>)> {
        let redundant = self.config.redundant;
        let missing: Vec<usize> = (0..self.config.group_size)
            .filter(|i| self.originals[*i].is_none())
            .collect();

        if missing.is_empty() || missing.len() > redundant {
            return Vec::new();
        }

        // XOR 的对齐长度 = 组内最长包。收到冗余包时它的长度就是发送端的组内最长包长度，
        // 所以优先相信它；只收到原始包时只能取已收包的最大长度（可能偏小）。
        let pad_len = self
            .originals
            .iter()
            .flatten()
            .map(|p| p.len())
            .chain(self.redundants.iter().flatten().map(|p| p.len()))
            .max()
            .unwrap_or(0);

        // 建方程组：每个收到的冗余包给出一行 (未知数系数掩码, 右端项)
        let mut rows: Vec<(Vec<bool>, Vec<u8>)> = Vec::new();
        for (k, slot) in self.redundants.iter().enumerate() {
            let Some(r) = slot.as_ref() else { continue };

            let mut mask = vec![false; missing.len()];
            let mut rhs = vec![0u8; pad_len];
            xor_into(&mut rhs, r);

            for (j, &m) in missing.iter().enumerate() {
                // 包 m 参与 R_k 当且仅当它的余类不是 k（redundant == 1 时 R_0 包含全部）
                if redundant == 1 || m % redundant != k {
                    mask[j] = true;
                }
            }
            for (i, slot) in self.originals.iter().enumerate() {
                let Some(payload) = slot.as_ref() else {
                    continue;
                };
                if redundant == 1 || i % redundant != k {
                    xor_into(&mut rhs, payload);
                }
            }
            rows.push((mask, rhs));
        }

        if rows.is_empty() {
            return Vec::new();
        }

        // 高斯-约当消元：每列找一个主元行，把该列从其余所有行消掉
        let unknowns = missing.len();
        let mut pivot_row_of_col = vec![usize::MAX; unknowns];
        let mut row_cursor = 0usize;
        for col in 0..unknowns {
            let Some(sel) = (row_cursor..rows.len()).find(|&r| rows[r].0[col]) else {
                continue;
            };
            rows.swap(row_cursor, sel);
            let (pivot_mask, pivot_rhs) = rows[row_cursor].clone();
            for (r, row) in rows.iter_mut().enumerate() {
                if r == row_cursor || !row.0[col] {
                    continue;
                }
                for (dst, src) in row.0.iter_mut().zip(pivot_mask.iter()) {
                    *dst ^= *src;
                }
                xor_into(&mut row.1, &pivot_rhs);
            }
            pivot_row_of_col[col] = row_cursor;
            row_cursor += 1;
        }

        // 只接受"单位行"：该行除本列外没有其它未知数系数，解才唯一确定。
        // 有残余系数说明还依赖自由变量（欠定），跳过交给 NACK。
        let mut recovered: Vec<(usize, Vec<u8>)> = Vec::new();
        for (j, &m) in missing.iter().enumerate() {
            let r = pivot_row_of_col[j];
            if r == usize::MAX {
                continue;
            }
            let unit_row = rows[r].0.iter().enumerate().all(|(c, &v)| !v || c == j);
            if !unit_row {
                continue;
            }
            recovered.push((m, rows[r].1.clone()));
        }

        for (m, data) in &recovered {
            self.originals[*m] = Some(data.clone());
        }
        self.recovered_count += recovered.len() as u64;
        recovered
    }

    /// 当前组收到多少个原始包（含恢复出来的）
    pub fn received(&self) -> usize {
        self.originals.iter().filter(|slot| slot.is_some()).count()
    }

    /// 当前组还缺几个
    pub fn missing(&self) -> usize {
        self.config.group_size.saturating_sub(self.received())
    }

    /// 切到下一组：清空本组的原始包与冗余包
    ///
    /// 累计统计（[`FecDecoder::recovered_count`]）不清零 —— 它是给上层做
    /// "FEC 到底救回了多少包"的长期指标用的。
    pub fn next_group(&mut self) {
        for slot in self.originals.iter_mut() {
            *slot = None;
        }
        for slot in self.redundants.iter_mut() {
            *slot = None;
        }
    }

    /// 统计：成功恢复的包数（跨组累计）
    pub fn recovered_count(&self) -> u64 {
        self.recovered_count
    }
}
