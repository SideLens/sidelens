//! 帧、采集源、后端与统计（**纯跨平台**）
//!
//! 这里的类型是采集层对外的全部数据契约：编码器只认 [`Frame`] 里的 NV12
//! 字节和 coded 尺寸，不关心它是从 GDI、DXGI 还是合成源来的。
//!
//! # 为什么 Frame 自带两套尺寸
//!
//! H.264 的 coded size 必须 16 对齐（1920x1080 → 1920x1088），但**显示**尺寸
//! 仍然是 1920x1080。接收端渲染时要用显示尺寸做缩放/裁剪，编码器要用 coded
//! 尺寸分配缓冲。两者都放进 Frame，调用方就不必自己再算一遍
//! （算错的表现是画面底部被拉伸出 8 行，肉眼可见但很难定位）。
use std::fmt;
use std::time::Instant;

use crate::convert::{FrameLayout, Y_BLACK};

/// 采集源类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureSource {
    PrimaryMonitor,
    Monitor(usize),
    Window(isize),
    Synthetic,
}

impl CaptureSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PrimaryMonitor => "主显示器",
            Self::Monitor(_) => "显示器",
            Self::Window(_) => "窗口",
            Self::Synthetic => "合成源",
        }
    }

    pub const fn is_synthetic(&self) -> bool {
        matches!(self, Self::Synthetic)
    }
}

impl fmt::Display for CaptureSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrimaryMonitor => write!(f, "主显示器"),
            Self::Monitor(n) => write!(f, "显示器 #{n}"),
            Self::Window(h) => write!(f, "窗口 0x{:X}", *h as usize),
            Self::Synthetic => write!(f, "合成源"),
        }
    }
}
/// 实际使用的采集后端
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    Gdi,
    Dxgi,
    Synthetic,
}

impl Backend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gdi => "GDI",
            Self::Dxgi => "DXGI",
            Self::Synthetic => "Synthetic",
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
/// 一帧采集结果
///
/// # 内存布局
///
/// `data` 是一整块 NV12：前 `coded_width * coded_height` 字节是 Y 平面，
/// 后面 `coded_width * coded_height / 2` 字节是**交织**的 UV 平面
/// （U0 V0 U1 V1 …），总长 = `coded_width * coded_height * 3 / 2`。
///
/// # 为什么 data 是 Vec 而不是借用的切片
///
/// 采集循环里「上一帧还在编码、这一帧已经采到」是常态。借用会逼调用方
/// 要么加锁要么双缓冲，而双缓冲的边界（哪一帧给谁）反而是最容易写错的地方。
/// 每帧一个 Vec 的代价是每帧一次分配，换来的是所有权清晰 ——
/// 实测这部分开销在 1080p 上远小于采集本身（见 `examples/bench_capture.rs`）。
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    /// NV12 数据，长度 = `coded_width * coded_height * 3 / 2`
    pub data: Vec<u8>,
    /// 16 对齐后的编码宽度
    pub coded_width: u32,
    /// 16 对齐后的编码高度
    pub coded_height: u32,
    /// 实际显示宽度（可能比 coded 小，比如 1920x1080 vs 1920x1088）
    pub display_width: u32,
    /// 实际显示高度
    pub display_height: u32,
    /// 采集时刻（微秒，相对 capture 创建时刻）
    pub captured_at_us: u64,
}
impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field(
                "display",
                &format_args!("{}x{}", self.display_width, self.display_height),
            )
            .field(
                "coded",
                &format_args!("{}x{}", self.coded_width, self.coded_height),
            )
            .field("bytes", &self.data.len())
            .field("captured_at_us", &self.captured_at_us)
            .finish()
    }
}

impl Frame {
    /// 按 layout 分配一个全黑的帧（兜底用）
    ///
    /// 采集失败但编码器必须拿到一帧时用它，避免上游出现「没有帧」的分支。
    pub fn black(layout: FrameLayout, captured_at_us: u64) -> Self {
        let mut data = vec![0u8; layout.nv12_len()];
        crate::convert::fill_black_nv12(layout, &mut data);
        Self {
            data,
            coded_width: layout.coded_width(),
            coded_height: layout.coded_height(),
            display_width: layout.display_width(),
            display_height: layout.display_height(),
            captured_at_us,
        }
    }

    /// 尺寸布局
    pub fn layout(&self) -> FrameLayout {
        FrameLayout::new(self.display_width, self.display_height)
            .unwrap_or_else(|_| FrameLayout::new(2, 2).expect("2x2 是合法尺寸"))
    }

    /// Y 平面
    pub fn y_plane(&self) -> &[u8] {
        let n = (self.coded_width as usize) * (self.coded_height as usize);
        &self.data[..n.min(self.data.len())]
    }

    /// 交织的 UV 平面
    pub fn uv_plane(&self) -> &[u8] {
        let n = (self.coded_width as usize) * (self.coded_height as usize);
        if n >= self.data.len() {
            return &[];
        }
        &self.data[n..]
    }

    /// 这一帧是否全黑（limited range 下 Y 恒为 16）
    pub fn is_black(&self) -> bool {
        self.y_plane().iter().all(|&v| v == Y_BLACK)
    }

    /// Y 平面的统计（min / max / mean）
    ///
    /// 用来判断「这一帧是不是全黑」—— 全黑帧在有限范围下是 16，
    /// 所以 `min == max == 16` 就是没有任何内容。
    pub fn y_stats(&self) -> PlaneStats {
        let y = self.y_plane();
        if y.is_empty() {
            return PlaneStats::default();
        }
        let mut min = u8::MAX;
        let mut max = 0u8;
        let mut sum = 0u64;
        for &v in y {
            if v < min {
                min = v;
            }
            if v > max {
                max = v;
            }
            sum += v as u64;
        }
        PlaneStats {
            min,
            max,
            mean: sum as f64 / y.len() as f64,
            samples: y.len(),
        }
    }
}

/// 采集统计
///
/// # frames_dropped 的语义
///
/// 只统计**源自己告诉我们的丢帧**，不统计调用方没来得及调 `capture()` 的帧
/// （那种「丢」在采集层根本不可见）。DXGI 用 `AccumulatedFrames` 减 1 得到
/// 真实数字；GDI / 合成源没有这个概念，恒为 0。
///
/// 这个区分很重要：如果是 `frames_dropped` 涨，说明桌面刷新比我们的采集快
/// （该提采集频率或换 DXGI）；如果只是 `frames` 不涨，说明调用方自己太慢。
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureStats {
    /// 成功产出的帧数
    pub frames: u64,
    /// 源侧累计丢掉的帧数
    pub frames_dropped: u64,
    /// 累计采集耗时（微秒）
    pub total_us: u128,
    /// 采集返回 `Ok(None)` 的次数（桌面无变化）
    pub empty_polls: u64,
}

impl CaptureStats {
    /// 单帧平均采集耗时（微秒）
    pub fn avg_us(&self) -> f64 {
        if self.frames == 0 {
            0.0
        } else {
            self.total_us as f64 / self.frames as f64
        }
    }
}

/// 采集计时器：把「相对 capture 创建时刻的微秒」算出来
///
/// # 为什么不用 SystemTime
///
/// `SystemTime` 会跟着系统时钟跳（NTP 校时、手动改时间）。采集时间戳要参与
/// 音画同步与延迟统计，必须单调。`Instant` 在 Windows 上是 QPC，单调且
/// 分辨率约 100 ns。
#[derive(Debug)]
pub struct CaptureClock {
    origin: Instant,
}

impl CaptureClock {
    /// 以当前时刻为原点
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    /// 从原点到现在经过的微秒
    pub fn elapsed_us(&self) -> u64 {
        let d = self.origin.elapsed();
        d.as_secs() * 1_000_000 + u64::from(d.subsec_micros())
    }
}

impl Default for CaptureClock {
    fn default() -> Self {
        Self::new()
    }
}

/// 单个平面的统计
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PlaneStats {
    /// 最小值
    pub min: u8,
    /// 最大值
    pub max: u8,
    /// 均值
    pub mean: f64,
    /// 参与统计的样本数
    pub samples: usize,
}
