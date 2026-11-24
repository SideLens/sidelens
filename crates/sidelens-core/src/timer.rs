//! 系统定时器分辨率
//!
//! # 这是本项目踩到的最有欺骗性的坑
//!
//! 在 Windows 上，`tokio::time::interval(Duration::from_millis(1))` **不是 1ms 触发一次**，
//! 而是 15.625ms —— 因为 Windows 的默认定时器分辨率就是 15.625ms（64Hz）。
//! 实测数据：
//!
//! ```text
//! 默认:            interval( 1ms)  p50=15.619ms
//! timeBeginPeriod: interval( 1ms)  p50= 1.518ms
//! ```
//!
//! 对视频管线的影响：帧在抖动缓冲里到期后，还要等下一次 tick 才能被取走，
//! **平均白等 7.8ms**。这部分延迟完全隐形 —— 抖动缓冲的报告值只有 20ms，
//! 实际 playable 却是 31ms，差的 11ms 就在这里。
//!
//! # 处置
//!
//! 用 `timeBeginPeriod(1)` 把系统分辨率提到 1ms。代价是全系统功耗略升
//! （其他进程的 sleep 也变精确了），所以只在本进程运行期间生效，Drop 时还原。

// 本模块是全 crate 唯一允许 unsafe 的地方：winmm 的 timeBeginPeriod /
// timeEndPeriod 没有可用的安全封装，且这两个调用幂等、失败也无害。
#![allow(unsafe_code)]

/// 提升系统定时器分辨率的作用域守卫
///
/// 在 Windows 上调用 `timeBeginPeriod`；其他平台是空操作
/// （Linux/macOS 的 tokio 定时器精度本来就是微秒级）。
///
/// # 用法
///
/// ```
/// # use sidelens_core::timer::HighResTimer;
/// let _guard = HighResTimer::new();  // 守卫存活期间分辨率是 1ms
/// // ... 跑管线 ...
/// // 守卫 drop 时自动还原
/// ```
#[derive(Debug)]
pub struct HighResTimer {
    #[cfg(windows)]
    _inner: WindowsTimerGuard,
}

impl Default for HighResTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl HighResTimer {
    /// 请求 1ms 定时器分辨率
    pub fn new() -> Self {
        #[cfg(windows)]
        {
            Self {
                _inner: WindowsTimerGuard::new(1),
            }
        }
        #[cfg(not(windows))]
        {
            Self {}
        }
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsTimerGuard;

#[cfg(windows)]
impl WindowsTimerGuard {
    fn new(period_ms: u32) -> Self {
        // SAFETY: timeBeginPeriod / timeEndPeriod 是幂等的 winmm 调用；
        // 成功返回 0，失败返回非零，但我们没有可做的补救动作，忽略即可。
        unsafe {
            timeBeginPeriod(period_ms);
        }
        Self
    }
}

#[cfg(windows)]
impl Drop for WindowsTimerGuard {
    fn drop(&mut self) {
        // SAFETY: 成对调用，参数与 new 中一致
        unsafe {
            timeEndPeriod(1);
        }
    }
}

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(uperiod: u32) -> u32;
    fn timeEndPeriod(uperiod: u32) -> u32;
}

impl HighResTimer {
    /// 当前平台是否支持提升定时器分辨率
    pub const fn supported() -> bool {
        cfg!(windows)
    }
}
