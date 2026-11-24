//! 色彩矩阵与 range 的**位域计算**（纯跨平台，可单测）
//!
//! # 为什么单独一个模块
//!
//! `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` 是一个 `u32` 位域，但 windows-rs 把它
//! 包成了不透明结构体（只有一个 `_bitfield` 字段），没有任何类型安全的构造器。
//! 位摆错一位的后果是**颜色不对但不报错**：BT.601/BT.709 搞反是「红变橙」，
//! range 搞反是「黑变灰、白变 235」—— 都属于肉眼能看出、但代码里查半天的那类。
//!
//! 所以位域拼装放在这里，用纯函数 + 单元测试钉死；Windows 侧只是把结果塞进
//! `_bitfield`。这样 Linux CI 也能验证「BT.709 limited 的输入色彩空间是 0x14」。
//!
//! # 位域布局（来自 `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` 文档）
//!
//! ~~~text
//! bit 0     : Usage            0 = playback, 1 = video processing
//! bit 1     : RGB_Range        0 = full (0-255), 1 = limited (16-235)
//! bit 2     : YCbCr_Matrix     0 = BT.601, 1 = BT.709
//! bit 3     : YCbCr_xvYCC      0 = conventional, 1 = xvYCC
//! bit 4..5  : Nominal_Range    0 = undefined, 1 = 16-235, 2 = 0-255
//! ~~~
//!
//! # 为什么默认 BT.709 limited
//!
//! 和 `sidelens-render` 的 `convert.rs` 保持**同一个决策**：本项目的码流
//! （MF H.264 编码器）不写 VUI 色彩描述，行业默认对 HD（≥720p）按 BT.709
//! limited 解释。两条渲染路径必须给出同样的颜色，否则「GPU 版偏色」会变成
//! 一个假 bug —— 所以这里直接沿用那个结论，理由见 `sidelens-render/src/convert.rs`
//! 的模块文档。

/// 输入 YUV 的矩阵与 range
///
/// 默认 [`ColorSpace::Bt709Limited`]，理由见模块文档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorSpace {
    /// BT.709 + limited range（Y: 16..235，Cb/Cr: 16..240）—— **默认**
    #[default]
    Bt709Limited,
    /// BT.601 + limited range —— 只在对接老 SD 素材时用
    Bt601Limited,
    /// BT.709 + full range —— 对接自己转码、明确是 0..255 的素材
    Bt709Full,
}

impl ColorSpace {
    /// 人类可读的名字
    pub fn as_str(self) -> &'static str {
        match self {
            ColorSpace::Bt709Limited => "BT.709 limited",
            ColorSpace::Bt601Limited => "BT.601 limited",
            ColorSpace::Bt709Full => "BT.709 full",
        }
    }

    /// 视频处理器**输入**流的 `_bitfield`
    pub fn input_bitfield(self) -> u32 {
        match self {
            // matrix = BT.709, nominal range = 16-235
            ColorSpace::Bt709Limited => bit(2) | nominal(1),
            // matrix = BT.601, nominal range = 16-235
            ColorSpace::Bt601Limited => nominal(1),
            // matrix = BT.709, nominal range = 0-255
            ColorSpace::Bt709Full => bit(2) | nominal(2),
        }
    }
}

/// 输出（RGB）的 `_bitfield`
///
/// 恒为 **full range**：swap chain 是 `B8G8R8A8_UNORM`，DWM 按 sRGB 全范围合成。
/// 如果这里声明成 limited，白就不是 255 而是 235，画面会整体发灰 ——
/// 这正是「GPU 渲染看着比 GDI 暗淡」的经典成因。
pub const fn output_bitfield() -> u32 {
    // RGB_Range = 0 (full)，Nominal_Range = 2 (0-255)
    nominal(2)
}

const fn bit(n: u32) -> u32 {
    1 << n
}

/// Nominal_Range 是 bit 4..5 的两比特字段
const fn nominal(v: u32) -> u32 {
    (v & 0b11) << 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt709_limited_is_the_documented_0x14() {
        // 0x14 = matrix(BT.709) | nominal(16-235)。这个值在 MF / D3D 视频播放
        // 示例里是「HD 素材」的标准写法，改动会让颜色整体偏移。
        assert_eq!(ColorSpace::Bt709Limited.input_bitfield(), 0x14);
    }

    #[test]
    fn bt601_limited_only_sets_nominal_range() {
        assert_eq!(ColorSpace::Bt601Limited.input_bitfield(), 0x10);
    }

    #[test]
    fn bt709_full_sets_nominal_0_255() {
        assert_eq!(ColorSpace::Bt709Full.input_bitfield(), 0x24);
    }

    #[test]
    fn output_is_full_range() {
        // RGB_Range 位（bit 1）必须是 0，Nominal_Range 必须是 2
        assert_eq!(output_bitfield() & bit(1), 0, "输出不能声明成 limited");
        assert_eq!(output_bitfield(), 0x20);
    }

    #[test]
    fn default_is_bt709_limited() {
        assert_eq!(ColorSpace::default(), ColorSpace::Bt709Limited);
        assert_eq!(ColorSpace::default().as_str(), "BT.709 limited");
    }

    #[test]
    fn names_are_stable_for_logs() {
        assert_eq!(ColorSpace::Bt601Limited.as_str(), "BT.601 limited");
        assert_eq!(ColorSpace::Bt709Full.as_str(), "BT.709 full");
    }
}
