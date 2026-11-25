//! DirectShow 源滤镜和推帧线程生命周期。

#![allow(unsafe_code)]
// DirectShow 的 COM 接口由 windows crate 包装，不带 Send+Sync 标记，
// 但实际调用都发生在构建滤镜图的同一个线程上（MTA）。
#![allow(clippy::arc_with_non_send_sync)]

use std::sync::{Arc, Mutex};

use windows::core::{implement, ComObject, IUnknown, Interface, Ref, Result as WinResult, GUID};
use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL, E_POINTER, S_FALSE, S_OK};
use windows::Win32::Media::DirectShow::{
    IBaseFilter, IBaseFilter_Impl, IEnumPins, IEnumPins_Impl, IFilterGraph, IMediaFilter,
    IMediaFilter_Impl, IPin, State_Paused, State_Running, State_Stopped, FILTER_STATE,
};
use windows::Win32::Media::IReferenceClock;
use windows::Win32::System::Com::{IPersist, IPersist_Impl};

use crate::pin::{OutputPin, Pusher};
use crate::source::FrameSource;

/// SideLens 虚拟摄像头的 CLSID
///
/// 这个值会写进注册表，消费端靠它 `CoCreateInstance` 创建滤镜。
/// **一旦发布就不能改** —— 改了等于换了一个新设备。
pub const CLSID_SIDELENS_DSHOW: GUID = GUID::from_u128(0x9E4B7C31_5A2D_4F8E_B1C6_3D7A0E5F92B4);

/// 源滤镜
#[implement(IBaseFilter, IMediaFilter, IPersist)]
pub struct SideLensFilter {
    state: FilterState,
}

struct FilterState {
    pin: ComObject<OutputPin>,
    pusher: Mutex<Option<Pusher>>,
    filter_state: Mutex<FILTER_STATE>,
    graph: Mutex<Option<IFilterGraph>>,
    clock: Mutex<Option<IUnknown>>,
}

impl FilterState {
    fn start_pusher(&self) -> WinResult<()> {
        let mut worker = self.pusher.lock().map_err(|_| E_FAIL)?;
        if worker.is_none() {
            *worker = self.pin.start()?;
        }
        Ok(())
    }

    fn stop_pusher(&self) {
        let worker = self.pusher.lock().ok().and_then(|mut worker| worker.take());
        self.pin.decommit();
        if let Some(worker) = worker {
            worker.stop();
        }
    }

    fn state(&self) -> FILTER_STATE {
        self.filter_state
            .lock()
            .map(|state| *state)
            .unwrap_or(State_Stopped)
    }
}

impl Drop for FilterState {
    fn drop(&mut self) {
        self.stop_pusher();
    }
}

impl SideLensFilter {
    pub fn new() -> ComObject<Self> {
        let source = Arc::new(FrameSource::new(1280, 720));
        let pin = OutputPin::new(source);
        let filter = ComObject::new(Self {
            state: FilterState {
                pin,
                pusher: Mutex::new(None),
                filter_state: Mutex::new(State_Stopped),
                graph: Mutex::new(None),
                clock: Mutex::new(None),
            },
        });
        filter.state.pin.set_filter(&filter.to_interface());
        filter
    }

    pub fn current_state(&self) -> FILTER_STATE {
        self.state.state()
    }
}

impl Default for SideLensFilter {
    fn default() -> Self {
        // ComObject 已经包好了，这里只为满足 clippy
        unreachable!("请用 SideLensFilter::new()")
    }
}

// ───────── IPersist（GetClassID）─────────

impl IPersist_Impl for SideLensFilter_Impl {
    fn GetClassID(&self) -> WinResult<GUID> {
        Ok(CLSID_SIDELENS_DSHOW)
    }
}

// ───────── IMediaFilter（状态机 + 时钟）─────────

impl IMediaFilter_Impl for SideLensFilter_Impl {
    fn Stop(&self) -> WinResult<()> {
        self.state.stop_pusher();
        *self.state.filter_state.lock().map_err(|_| E_FAIL)? = State_Stopped;
        Ok(())
    }

    fn Pause(&self) -> WinResult<()> {
        self.state.start_pusher()?;
        *self.state.filter_state.lock().map_err(|_| E_FAIL)? = State_Paused;
        Ok(())
    }

    fn Run(&self, _tstart: i64) -> WinResult<()> {
        self.state.start_pusher()?;
        *self.state.filter_state.lock().map_err(|_| E_FAIL)? = State_Running;
        Ok(())
    }

    fn GetState(&self, _mstimeout: u32) -> WinResult<FILTER_STATE> {
        Ok(self.state.state())
    }

    fn SetSyncSource(&self, pclock: Ref<IReferenceClock>) -> WinResult<()> {
        if let Ok(mut g) = self.state.clock.lock() {
            *g = pclock.as_ref().map(|c| c.clone().into());
        }
        Ok(())
    }

    fn GetSyncSource(&self) -> WinResult<IReferenceClock> {
        let g = self.state.clock.lock().map_err(|_| {
            windows::core::Error::from_hresult(windows::core::HRESULT(0x8000_4005u32 as i32))
        })?;
        match g.as_ref() {
            Some(c) => c.cast(),
            None => Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                0x8004_0210u32 as i32,
            ))),
        }
    }
}

// ───────── IBaseFilter（引脚枚举 + 图 + 信息）─────────

impl IBaseFilter_Impl for SideLensFilter_Impl {
    fn EnumPins(&self) -> WinResult<IEnumPins> {
        Ok(PinEnum::new(vec![self.state.pin.to_interface()]).into_interface())
    }

    fn FindPin(&self, id: &windows::core::PCWSTR) -> WinResult<IPin> {
        if id.is_null() {
            return Err(E_POINTER.into());
        }
        if unsafe { id.to_string() }.ok().as_deref() == Some("Output") {
            Ok(self.state.pin.to_interface())
        } else {
            Err(windows::Win32::Media::DirectShow::VFW_E_NOT_FOUND.into())
        }
    }

    fn QueryFilterInfo(
        &self,
        pinfo: *mut windows::Win32::Media::DirectShow::FILTER_INFO,
    ) -> WinResult<()> {
        if pinfo.is_null() {
            return Err(E_POINTER.into());
        }
        // 关键：pGraph 要 AddRef —— 调用方负责 Release
        //
        // 第一版直接 clone 了 Option<IFilterGraph>。windows crate 的
        // clone 会 AddRef，所以这里其实是正确的；但必须显式确认，
        // 因为规范要求「调用方 Release」，漏了 AddRef 会导致提前释放。
        let graph = self.state.graph.lock().ok().and_then(|g| g.clone());
        let mut info = windows::Win32::Media::DirectShow::FILTER_INFO {
            achName: [0u16; 128],
            pGraph: std::mem::ManuallyDrop::new(graph),
        };
        for (i, ch) in crate::FILTER_NAME.encode_utf16().enumerate() {
            if i < 127 {
                info.achName[i] = ch;
            }
        }
        // SAFETY: pinfo 已判空
        unsafe { std::ptr::write(pinfo, info) };
        Ok(())
    }

    fn JoinFilterGraph(
        &self,
        pgraph: Ref<IFilterGraph>,
        _pname: &windows::core::PCWSTR,
    ) -> WinResult<()> {
        if let Ok(mut g) = self.state.graph.lock() {
            *g = pgraph.as_ref().cloned();
        }
        Ok(())
    }

    fn QueryVendorInfo(&self) -> WinResult<windows::core::PWSTR> {
        // 没有厂商信息
        Err(E_NOTIMPL.into())
    }
}

// ───────────────────────── 引脚枚举器 ─────────────────────────

/// `IEnumPins` 实现
#[implement(IEnumPins)]
pub struct PinEnum {
    pins: Mutex<Vec<IPin>>,
    pos: Mutex<usize>,
}

impl PinEnum {
    pub fn new(pins: Vec<IPin>) -> ComObject<Self> {
        ComObject::new(Self {
            pins: Mutex::new(pins),
            pos: Mutex::new(0),
        })
    }
}

impl IEnumPins_Impl for PinEnum_Impl {
    fn Next(
        &self,
        cppins: u32,
        pppins: *mut Option<IPin>,
        pcfetched: *mut u32,
    ) -> windows::core::HRESULT {
        if pppins.is_null() || (pcfetched.is_null() && cppins != 1) {
            return E_POINTER;
        }
        let pins = match self.pins.lock() {
            Ok(g) => g,
            Err(_) => return windows::core::HRESULT(0x8000_4005u32 as i32),
        };
        let mut pos = match self.pos.lock() {
            Ok(g) => g,
            Err(_) => return windows::core::HRESULT(0x8000_4005u32 as i32),
        };
        let mut written = 0u32;
        while written < cppins && *pos < pins.len() {
            let pin = pins[*pos].clone();
            // SAFETY: pppins 已判空，索引在范围内
            unsafe { std::ptr::write(pppins.add(written as usize), Some(pin)) };
            written += 1;
            *pos += 1;
        }
        // SAFETY: pcfetched 已判空
        if !pcfetched.is_null() {
            unsafe { *pcfetched = written };
        }
        if written == cppins {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, cpins: u32) -> WinResult<()> {
        let pins = self.pins.lock().map_err(|_| {
            windows::core::Error::from_hresult(windows::core::HRESULT(0x8000_4005u32 as i32))
        })?;
        let mut pos = self.pos.lock().map_err(|_| {
            windows::core::Error::from_hresult(windows::core::HRESULT(0x8000_4005u32 as i32))
        })?;
        *pos = (*pos + cpins as usize).min(pins.len());
        Ok(())
    }

    fn Reset(&self) -> WinResult<()> {
        let mut pos = self.pos.lock().map_err(|_| {
            windows::core::Error::from_hresult(windows::core::HRESULT(0x8000_4005u32 as i32))
        })?;
        *pos = 0;
        Ok(())
    }

    fn Clone(&self) -> WinResult<IEnumPins> {
        let pins = self.pins.lock().map_err(|_| {
            windows::core::Error::from_hresult(windows::core::HRESULT(0x8000_4005u32 as i32))
        })?;
        let pos = self.pos.lock().map_err(|_| {
            windows::core::Error::from_hresult(windows::core::HRESULT(0x8000_4005u32 as i32))
        })?;
        let e = PinEnum::new(pins.clone());
        if let Ok(mut p) = e.pos.lock() {
            *p = *pos;
        }
        Ok(e.into_interface())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clsid_is_stable() {
        // 回归测试：CLSID 一旦发布不能改（改了等于换了设备）
        assert_eq!(
            format!("{:?}", CLSID_SIDELENS_DSHOW),
            "9E4B7C31-5A2D-4F8E-B1C6-3D7A0E5F92B4"
        );
    }

    #[test]
    fn filter_starts_in_stopped_state() {
        let f = SideLensFilter::new();
        assert_eq!(f.current_state(), State_Stopped);
    }

    #[test]
    fn filter_transitions_stopped_running_stopped() {
        let f = SideLensFilter::new();
        let filter: IBaseFilter = f.clone().into_interface();
        unsafe {
            filter.Run(0).unwrap();
            assert_eq!(f.current_state(), State_Running);
            filter.Stop().unwrap();
            assert_eq!(f.current_state(), State_Stopped);
        }
    }

    #[test]
    fn filter_pause_also_unstops() {
        let f = SideLensFilter::new();
        let filter: IBaseFilter = f.clone().into_interface();
        unsafe {
            filter.Pause().unwrap();
            assert_eq!(f.current_state(), State_Paused);
        }
    }

    #[test]
    fn enum_pins_returns_one_output_pin() {
        let f = SideLensFilter::new();
        let filter: IBaseFilter = f.into_interface();
        unsafe {
            let e = filter.EnumPins().unwrap();
            let mut pins: [Option<IPin>; 4] = [None, None, None, None];
            let mut fetched = 0u32;
            // 包装方法：Next(&mut [Option<IPin>], Option<*mut u32>) -> HRESULT
            let hr = e.Next(&mut pins, Some(&mut fetched));
            assert!(hr.is_ok(), "Next 应返回 S_OK，实际 {hr:?}");
            assert_eq!(fetched, 1, "只有一个输出引脚");
            assert!(pins[0].is_some());
        }
    }
}
