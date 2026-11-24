//! 用 D3D11 timestamp query 测 **GPU 侧**耗时
//!
//! # 为什么不能拿 CPU 时间当 GPU 时间
//!
//! `VideoProcessorBlt` 是**异步**的：调用返回时命令只是进了命令队列，
//! GPU 可能还没开始跑。所以「Blt 调用花了多少 CPU 时间」测的是提交开销
//! （通常 5~20 微秒），不是转换本身的成本。要报「GPU 上这一帧花了多久」，
//! 只能问 GPU。
//!
//! # 为什么要 timestamp + disjoint 一对
//!
//! - `D3D11_QUERY_TIMESTAMP` 给出 GPU 时钟计数值，但**单看它没有意义** ——
//!   必须两个配对相减，再除以 `Frequency`；
//! - `D3D11_QUERY_TIMESTAMP_DISJOINT` 告诉这次测量期间 GPU 时钟是否
//!   被降频/切换过。`Disjoint == TRUE` 说明频率变了，相减的结果不可信，
//!   必须丢弃（典型场景：笔记本从集显切到独显、或者 GPU 降频省电）。
//!
//! 丢弃而不是「照样算」很重要：否则报出来的数字会莫名其妙地差一个数量级，
//! 而排查方向会完全错。
//!
//! # 为什么允许「这一帧没有 GPU 时间」
//!
//! query 的结果要等 GPU 跑完才可读。为了不阻塞渲染线程，本模块用
//! **延迟一帧**的方式读：本帧 `Begin/End`，下一帧再 `GetData`。
//! 读不到就返回 `None`，调用方（统计）跳过即可 —— 统计里少几个样本
//! 远好过每帧阻塞一次。

use windows::Win32::Graphics::Direct3D11::{
    ID3D11Query, D3D11_QUERY_DATA_TIMESTAMP_DISJOINT, D3D11_QUERY_DESC, D3D11_QUERY_TIMESTAMP,
    D3D11_QUERY_TIMESTAMP_DISJOINT,
};

use super::device::GpuDevice;
use crate::error::{hresult, GpuError};

/// GPU 侧计时器（timestamp + disjoint 一对 query）
pub struct GpuTimer {
    disjoint: ID3D11Query,
    start: ID3D11Query,
    end: ID3D11Query,
    /// 上一次 Begin/End 之后是否还没读过结果
    pending: bool,
    /// 累计有效样本数与总耗时（微秒）
    samples: u64,
    total_us: f64,
}

impl std::fmt::Debug for GpuTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuTimer")
            .field("pending", &self.pending)
            .field("samples", &self.samples)
            .finish_non_exhaustive()
    }
}

impl GpuTimer {
    /// 创建三只 query
    ///
    /// 设备不支持 timestamp query 时返回错误 —— 调用方应当降级为「不报 GPU 时间」
    /// 而不是让整个渲染器创建失败。
    pub fn new(device: &GpuDevice) -> Result<Self, GpuError> {
        let mk = |q: windows::Win32::Graphics::Direct3D11::D3D11_QUERY| -> Result<ID3D11Query, GpuError> {
            let desc = D3D11_QUERY_DESC {
                Query: q,
                MiscFlags: 0,
            };
            let mut out: Option<ID3D11Query> = None;
            unsafe { device.device().CreateQuery(&desc, Some(&mut out)) }
                .map_err(|e| GpuError::Device(hresult("CreateQuery(timestamp)", &e)))?;
            out.ok_or_else(|| GpuError::Device("CreateQuery 未返回 query".into()))
        };

        Ok(GpuTimer {
            disjoint: mk(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
            start: mk(D3D11_QUERY_TIMESTAMP)?,
            end: mk(D3D11_QUERY_TIMESTAMP)?,
            pending: false,
            samples: 0,
            total_us: 0.0,
        })
    }

    /// 开始测量（必须在真正的工作之前调用）
    pub fn begin(&self, device: &GpuDevice) {
        let ctx = device.context();
        unsafe {
            ctx.Begin(&self.disjoint);
            ctx.End(&self.start);
        }
    }

    /// 结束测量（必须在工作之后、Present 之前调用）
    pub fn end(&mut self, device: &GpuDevice) {
        let ctx = device.context();
        unsafe {
            ctx.End(&self.end);
            ctx.End(&self.disjoint);
        }
        self.pending = true;
    }

    /// 尝试取上一次测量的结果（微秒）
    ///
    /// - `Some(us)`：拿到有效样本
    /// - `None`：还没跑完 / 时钟不连续（Disjoint）—— 两种情况都不该计入统计
    pub fn try_read(&mut self, device: &GpuDevice) -> Option<f64> {
        if !self.pending {
            return None;
        }
        let ctx = device.context();

        let mut disjoint = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
        // D3D11_ASYNC_GETDATA_DONOTFLUSH：不刷命令队列。不加的话 GetData
        // 会先把队列推空，等于每帧强制一次同步 —— 反而制造了延迟。
        let ok = unsafe {
            ctx.GetData(
                &self.disjoint,
                Some(&mut disjoint as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>() as u32,
                windows::Win32::Graphics::Direct3D11::D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
            )
        };
        if ok.is_err() {
            // S_FALSE = 数据还没准备好，不是错误；下帧再问
            return None;
        }
        self.pending = false;

        if disjoint.Disjoint.as_bool() {
            // GPU 时钟在这次测量期间变过，相减无意义
            return None;
        }
        if disjoint.Frequency == 0 {
            return None;
        }

        let mut t0 = 0u64;
        let mut t1 = 0u64;
        let size = std::mem::size_of::<u64>() as u32;
        let r0 = unsafe {
            ctx.GetData(
                &self.start,
                Some(&mut t0 as *mut _ as *mut std::ffi::c_void),
                size,
                0,
            )
        };
        let r1 = unsafe {
            ctx.GetData(
                &self.end,
                Some(&mut t1 as *mut _ as *mut std::ffi::c_void),
                size,
                0,
            )
        };
        if r0.is_err() || r1.is_err() || t1 < t0 {
            return None;
        }

        let us = (t1 - t0) as f64 * 1_000_000.0 / disjoint.Frequency as f64;
        self.samples += 1;
        self.total_us += us;
        Some(us)
    }

    /// 累计有效样本数
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// 平均 GPU 耗时（微秒）；没有样本时 0
    pub fn avg_us(&self) -> f64 {
        mean_us(self.samples, self.total_us)
    }
}

/// 平均值（微秒）
///
/// 抽成自由函数是为了能在**不构造** `GpuTimer` 的情况下单测 ——
/// `GpuTimer` 持有 `ID3D11Query`，而 COM 接口指针**不允许零初始化**
/// （`windows_core::IUnknown` 内部是 `NonNull`），用 `mem::zeroed()` 造一个
/// 假实例就是 UB。纯算术部分拿出来测，GPU 部分留给集成测试。
fn mean_us(samples: u64, total_us: f64) -> f64 {
    if samples == 0 {
        0.0
    } else {
        total_us / samples as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_of_no_samples_is_zero() {
        assert_eq!(mean_us(0, 0.0), 0.0);
        // 即使 total 非零（不该发生），也不能除零
        assert_eq!(mean_us(0, 123.0), 0.0);
    }

    #[test]
    fn mean_divides_by_sample_count() {
        assert_eq!(mean_us(4, 400.0), 100.0);
        assert!((mean_us(3, 10.0) - 3.333_333_333_333_333_5).abs() < 1e-9);
    }
}
