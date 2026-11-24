use std::time::{SystemTime, UNIX_EPOCH};

/// 跨进程一致的单调时钟（微秒）
///
/// # 为什么不用 `Instant`
///
/// `Instant` 的零点是**进程启动时刻**，两个进程相减得到的不是延迟，
/// 而是「两个进程启动时间差 + 真实延迟」。JS 版本就踩过这个坑：
/// 用 `performance.now()` 测出的延迟是 7148ms，实际只有 1.7ms。
///
/// 所以这里用 `SystemTime` 对齐到 Unix 纪元：同一台机器上的多个进程共享时基，
/// 跨设备时仍需做 NTP 式偏移协商（见 `RttEstimator`）。
///
/// # 代价
///
/// `SystemTime` 会被系统对时（NTP）拉动。生产环境应叠加一个单调时钟做校正：
/// 记录 `(system_time, instant)` 对，之后用 `instant` 的增量推进，
/// 只在必要时重新对齐。当前 demo 量级用不到。
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}
