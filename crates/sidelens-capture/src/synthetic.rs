//! 合成采集源（**纯跨平台**，不碰任何平台 API）
//!
//! 这是整个采集层的「标尺」：任何平台、任何权限下都能跑，产出的帧内容
//! **完全确定**，所以测试可以逐字节断言，压测可以排除采集源本身的影响。
//!
//! # 为什么不能拿随机字节当 NV12
//!
//! 随机噪声的每个宏块都不可预测，H.264 编码器只能全部走帧内，码控也拉不住
//! （项目实测 1280x720 全噪声在 2 Mbps 目标下跑出 56 Mbps，28 倍）。
//! 合成图案必须是「局部变化」的画面，帧间预测才有意义：
//! 渐变背景给编码器大量平坦块，移动竖条提供真实的运动矢量。
//!
//! # 内容
//!
//! - **Y 平面**：水平渐变（16..216）+ 逐行 ±3 的细微抖动，再叠一条每帧右移
//!   7 像素的高亮竖条（亮度 235，limited range 的白）。
//!   行抖动是为了让「连续几行完全一样」这种病态输入不出现 ——
//!   真实屏幕几乎不会有整屏纯色，用它做压测会低估码率。
//! - **UV 平面**：恒 128（中性灰）。加色度噪声只会让码率随帧号乱跳，
//!   对判断「采集 + 编码」链路的稳定性没有帮助。

use crate::convert::{align16, C_NEUTRAL, Y_WHITE};
use crate::frame::Frame;

/// 合成源的帧号步进（竖条每帧右移多少像素）
///
/// 7 与常见的屏幕宽度（640 / 1280 / 1920）互质，竖条不会在几帧后
/// 回到同一个位置 —— 那会让码率呈现虚假的周期性。
pub const SYNTH_BAR_STEP: usize = 7;

/// 合成源状态
#[derive(Debug, Clone)]
pub struct SyntheticSource {
    /// 显示宽度
    width: u32,
    /// 显示高度
    height: u32,
    /// 已产出的帧号
    seq: u64,
    /// 复用的输出缓冲（避免每帧分配）
    buf: Vec<u8>,
}

impl SyntheticSource {
    /// 按显示尺寸创建；宽高会被对齐到 16 的倍数后再分配缓冲
    pub fn new(width: u32, height: u32) -> Self {
        let cw = align16(width.max(1));
        let ch = align16(height.max(1));
        Self {
            width: width.max(1),
            height: height.max(1),
            seq: 0,
            buf: vec![0u8; (cw as usize) * (ch as usize) * 3 / 2],
        }
    }

    /// 显示宽度
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// 显示高度
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// 已产出的帧号（下一帧的 seq）
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// 生成一帧
    ///
    /// 返回的 Frame 自带 `captured_at_us = 0`：合成源没有真实时间基准，
    /// 由 [`crate::ScreenCapture`] 统一盖上时间戳，保证三种后端的时间戳
    /// 语义一致（否则调用方要按后端分支处理）。
    pub fn next_frame(&mut self) -> Frame {
        let cw = align16(self.width);
        let ch = align16(self.height);
        let need = (cw as usize) * (ch as usize) * 3 / 2;
        if self.buf.len() < need {
            self.buf.resize(need, C_NEUTRAL);
        }
        render_pattern(self.seq, self.width, self.height, cw, ch, &mut self.buf);
        self.seq += 1;
        Frame {
            data: self.buf.clone(),
            coded_width: cw,
            coded_height: ch,
            display_width: self.width,
            display_height: self.height,
            captured_at_us: 0,
        }
    }

    /// 直接往调用方的缓冲里写（压测路径，省一次 clone）
    ///
    /// 返回 `(coded_width, coded_height, seq)`。
    pub fn next_frame_into(&mut self, dst: &mut Vec<u8>) -> (u32, u32, u64) {
        let cw = align16(self.width);
        let ch = align16(self.height);
        let need = (cw as usize) * (ch as usize) * 3 / 2;
        if dst.len() < need {
            dst.resize(need, C_NEUTRAL);
        }
        render_pattern(self.seq, self.width, self.height, cw, ch, dst);
        let seq = self.seq;
        self.seq += 1;
        (cw, ch, seq)
    }
}

/// 渲染合成图案（纯函数，测试可直接调用）
///
/// # 参数
///
/// - `seq`：帧号，决定竖条位置
/// - `dw` / `dh`：显示尺寸（有效区域）
/// - `cw` / `ch`：编码尺寸（`dst` 的实际布局）
/// - `dst`：至少 `cw * ch * 3 / 2` 字节
///
/// 有效区域之外的填充用**边缘复制**，与 [`crate::convert::bgra_to_nv12`]
/// 的策略一致 —— 这样合成源与真实采集产出的帧在「填充区长什么样」上
/// 没有区别，压测结果可以直接外推。
pub fn render_pattern(seq: u64, dw: u32, dh: u32, cw: u32, ch: u32, dst: &mut [u8]) {
    let dwu = (dw as usize).max(1);
    let dhu = (dh as usize).max(1);
    let cwu = cw as usize;
    let chu = ch as usize;
    let y_len = cwu * chu;
    if dst.len() < y_len + y_len / 2 {
        return;
    }

    let bar_w = (dwu / 12).max(8);
    let bar_x = ((seq as usize).wrapping_mul(SYNTH_BAR_STEP)) % dwu;

    for y in 0..chu {
        let sy = y.min(dhu - 1);
        let row = &mut dst[y * cwu..y * cwu + cwu];
        // 行抖动 ±3：让相邻两行不完全相同，避免「整屏纯色」这种
        // 真实屏幕几乎不出现的输入
        let jitter = (sy % 7) as i32 - 3;
        for (x, px) in row[..dwu].iter_mut().enumerate() {
            // 竖条跨过右边界时折回左侧，保持「总有一条」的视觉连续性
            let in_bar = if bar_x + bar_w <= dwu {
                x >= bar_x && x < bar_x + bar_w
            } else {
                x >= bar_x || x + dwu < bar_x + bar_w
            };
            *px = if in_bar {
                Y_WHITE
            } else {
                let grad = ((x * 200) / dwu) as i32;
                (16 + grad + jitter).clamp(16, 235) as u8
            };
        }
        let last = row[dwu - 1];
        for px in row[dwu..].iter_mut() {
            *px = last;
        }
    }

    for px in dst[y_len..y_len + y_len / 2].iter_mut() {
        *px = C_NEUTRAL;
    }
}
