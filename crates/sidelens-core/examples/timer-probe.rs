//! 探测定时器精度，以及 Windows 的 timeBeginPeriod 能带来多少改善
//!
//! # 为什么这个实验决定架构
//!
//! 视频出队要么「轮询定时器」，要么「精确 sleep 到下一帧到期」。
//! 两条路都受**系统定时器分辨率**限制。
//! Windows 默认分辨率是 15.625ms —— 意味着哪怕写 `interval(1ms)`，
//! 实际也是每 15.6ms 才醒一次，抖动缓冲之外白多出平均 7.8ms 延迟。

use std::time::{Duration, Instant};

use tokio::time::{interval, MissedTickBehavior};

#[cfg(windows)]
mod win {
    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(uperiod: u32) -> u32;
        fn timeEndPeriod(uperiod: u32) -> u32;
    }

    pub struct HighResTimer;

    impl HighResTimer {
        pub fn new(period_ms: u32) -> Self {
            unsafe {
                timeBeginPeriod(period_ms);
            }
            Self
        }
    }

    impl Drop for HighResTimer {
        fn drop(&mut self) {
            unsafe {
                timeEndPeriod(1);
            }
        }
    }
}

async fn measure(tick_ms: u64) -> (f64, f64, f64) {
    let mut ticker = interval(Duration::from_millis(tick_ms));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    for _ in 0..5 {
        ticker.tick().await;
    }

    let n = 200;
    let mut gaps = Vec::with_capacity(n);
    let mut last = Instant::now();
    for _ in 0..n {
        ticker.tick().await;
        let now = Instant::now();
        gaps.push(now.duration_since(last).as_secs_f64() * 1000.0);
        last = now;
    }

    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (
        gaps[gaps.len() / 2],
        gaps[(gaps.len() as f64 * 0.95) as usize],
        gaps[gaps.len() - 1],
    )
}

#[tokio::main]
async fn main() {
    println!("=== 默认定时器精度 ===");
    for tick_ms in [1u64, 4, 16] {
        let (p50, p95, max) = measure(tick_ms).await;
        println!("interval({tick_ms:>2}ms)  p50={p50:>7.3}ms  p95={p95:>7.3}ms  max={max:>7.3}ms");
    }

    #[cfg(windows)]
    {
        println!();
        println!("=== timeBeginPeriod(1) 之后 ===");
        let _guard = win::HighResTimer::new(1);
        // 给系统一点时间应用新分辨率
        tokio::time::sleep(Duration::from_millis(50)).await;

        for tick_ms in [1u64, 4, 16] {
            let (p50, p95, max) = measure(tick_ms).await;
            println!(
                "interval({tick_ms:>2}ms)  p50={p50:>7.3}ms  p95={p95:>7.3}ms  max={max:>7.3}ms"
            );
        }
    }
}
