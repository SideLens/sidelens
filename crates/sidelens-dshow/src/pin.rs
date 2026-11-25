//! DirectShow 采集引脚、媒体类型协商和样本投递。

use std::mem::{size_of, ManuallyDrop};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{
    implement, ComObject, IUnknownImpl, Interface, Ref, Result as WinResult, Weak, GUID, HRESULT,
    PWSTR,
};
use windows::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, SIZE, S_FALSE, S_OK,
};
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Media::DirectShow::{
    IAMStreamConfig, IAMStreamConfig_Impl, IBaseFilter, IEnumMediaTypes, IEnumMediaTypes_Impl,
    IMediaSample, IMemAllocator, IMemInputPin, IPin, IPin_Impl, AMPROPERTY_PIN_CATEGORY,
    AM_GBF_NOWAIT, E_PROP_ID_UNSUPPORTED, E_PROP_SET_UNSUPPORTED, PINDIR_INPUT, PINDIR_OUTPUT,
    PIN_DIRECTION, PIN_INFO, VFW_E_ALREADY_CONNECTED, VFW_E_BUFFER_OVERFLOW,
    VFW_E_INVALID_DIRECTION, VFW_E_NOT_CONNECTED, VFW_E_NOT_STOPPED, VFW_E_NO_ACCEPTABLE_TYPES,
    VFW_E_NO_ALLOCATOR, VFW_E_TYPE_NOT_ACCEPTED, VIDEO_STREAM_CONFIG_CAPS,
};
use windows::Win32::Media::KernelStreaming::{IKsPropertySet, IKsPropertySet_Impl};
use windows::Win32::Media::MediaFoundation::{
    AMPROPSETID_Pin, CLSID_MemoryAllocator, FORMAT_VideoInfo, FORMAT_VideoInfo2, MEDIATYPE_Video,
    MFVideoFormat_NV12, AM_MEDIA_TYPE, MEDIASUBTYPE_RGB24, MEDIASUBTYPE_RGB32,
    PIN_CATEGORY_CAPTURE, VIDEOINFOHEADER, VIDEOINFOHEADER2,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemAlloc, CoTaskMemFree, CoUninitialize,
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

use crate::source::{Frame, FrameSource};

#[cfg(test)]
const FRAME_INTERVAL: i64 = 10_000_000 / 30;
const NV12_FOURCC: u32 = u32::from_le_bytes(*b"NV12");
const KSPROPERTY_SUPPORT_GET: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PixelLayout {
    Rgb32,
    Nv12,
    Rgb24,
}

impl PixelLayout {
    const SUPPORTED: [Self; 3] = [Self::Rgb32, Self::Nv12, Self::Rgb24];

    fn subtype(self) -> GUID {
        match self {
            Self::Rgb32 => MEDIASUBTYPE_RGB32,
            Self::Nv12 => MFVideoFormat_NV12,
            Self::Rgb24 => MEDIASUBTYPE_RGB24,
        }
    }

    fn bits(self) -> u16 {
        match self {
            Self::Rgb32 => 32,
            Self::Nv12 => 12,
            Self::Rgb24 => 24,
        }
    }

    fn stride(self, width: usize) -> usize {
        match self {
            Self::Rgb32 => width * 4,
            Self::Nv12 => width,
            Self::Rgb24 => (width * 3 + 3) & !3,
        }
    }

    fn size(self, width: usize, height: usize) -> usize {
        self.stride(width) * height * if self == Self::Nv12 { 3 } else { 2 } / 2
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VideoFormat {
    width: u32,
    height: u32,
    fps: u32,
    layout: PixelLayout,
    bottom_up: bool,
}

impl VideoFormat {
    fn new(width: u32, height: u32, layout: PixelLayout) -> Self {
        Self {
            width,
            height,
            fps: 30,
            layout,
            bottom_up: layout != PixelLayout::Nv12,
        }
    }

    fn with_fps(mut self, fps: u32) -> Self {
        self.fps = fps.max(1);
        self
    }

    fn frame_interval(self) -> i64 {
        10_000_000 / self.fps as i64
    }

    fn size(self) -> usize {
        self.layout.size(self.width as usize, self.height as usize)
    }

    fn header(self) -> VIDEOINFOHEADER {
        VIDEOINFOHEADER {
            dwBitRate: (self.size() * 8 * self.fps as usize) as u32,
            AvgTimePerFrame: self.frame_interval(),
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.width as i32,
                biHeight: if self.layout != PixelLayout::Nv12 && !self.bottom_up {
                    -(self.height as i32)
                } else {
                    self.height as i32
                },
                biPlanes: 1,
                biBitCount: self.layout.bits(),
                biCompression: if self.layout == PixelLayout::Nv12 {
                    NV12_FOURCC
                } else {
                    0
                },
                biSizeImage: self.size() as u32,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    // DirectShow may supply a partial type to Connect. Accept only layouts we can produce.
    unsafe fn parse(self, mt: &AM_MEDIA_TYPE) -> Option<Self> {
        if mt.majortype != MEDIATYPE_Video
            || mt.subtype != self.layout.subtype()
            || mt.pbFormat.is_null()
        {
            return None;
        }
        let (header, interval) = if mt.formattype == FORMAT_VideoInfo
            && mt.cbFormat as usize >= size_of::<VIDEOINFOHEADER>()
        {
            let vi = mt.pbFormat.cast::<VIDEOINFOHEADER>().read_unaligned();
            (vi.bmiHeader, vi.AvgTimePerFrame)
        } else if mt.formattype == FORMAT_VideoInfo2
            && mt.cbFormat as usize >= size_of::<VIDEOINFOHEADER2>()
        {
            let vi = mt.pbFormat.cast::<VIDEOINFOHEADER2>().read_unaligned();
            (vi.bmiHeader, vi.AvgTimePerFrame)
        } else {
            return None;
        };
        if header.biWidth != self.width as i32
            || header.biHeight.unsigned_abs() != self.height
            || header.biBitCount != self.layout.bits()
            || header.biCompression != self.header().bmiHeader.biCompression
            || (header.biSizeImage != 0 && header.biSizeImage as usize != self.size())
            || (interval != 0 && interval.abs_diff(self.frame_interval()) > 1)
        {
            return None;
        }
        Some(Self {
            bottom_up: self.layout != PixelLayout::Nv12 && header.biHeight > 0,
            ..self
        })
    }
}

// Each returned media type owns a CoTaskMem format block. An enumerator transfers
// both allocations to the consumer, while Connect retains its own format bytes.
#[derive(Clone)]
struct MediaType {
    major: GUID,
    subtype: GUID,
    format_type: GUID,
    sample_size: u32,
    format: Vec<u8>,
}

impl MediaType {
    fn new(video: VideoFormat) -> Self {
        let header = video.header();
        // VIDEOINFOHEADER has no padding on the Windows ABI.
        let format = unsafe {
            std::slice::from_raw_parts(
                (&header as *const VIDEOINFOHEADER).cast::<u8>(),
                size_of::<VIDEOINFOHEADER>(),
            )
        }
        .to_vec();
        Self {
            major: MEDIATYPE_Video,
            subtype: video.layout.subtype(),
            format_type: FORMAT_VideoInfo,
            sample_size: video.size() as u32,
            format,
        }
    }

    unsafe fn copy(mt: &AM_MEDIA_TYPE, video: VideoFormat) -> Self {
        Self {
            major: mt.majortype,
            subtype: mt.subtype,
            format_type: mt.formattype,
            sample_size: video.size() as u32,
            format: std::slice::from_raw_parts(mt.pbFormat, mt.cbFormat as usize).to_vec(),
        }
    }

    fn raw(&self, format: *mut u8) -> AM_MEDIA_TYPE {
        AM_MEDIA_TYPE {
            majortype: self.major,
            subtype: self.subtype,
            bFixedSizeSamples: true.into(),
            bTemporalCompression: false.into(),
            lSampleSize: self.sample_size,
            formattype: self.format_type,
            pUnk: ManuallyDrop::new(None),
            cbFormat: self.format.len() as u32,
            pbFormat: format,
        }
    }

    fn allocate(&self) -> WinResult<AM_MEDIA_TYPE> {
        unsafe {
            let format = CoTaskMemAlloc(self.format.len()).cast::<u8>();
            if format.is_null() {
                return Err(E_OUTOFMEMORY.into());
            }
            std::ptr::copy_nonoverlapping(self.format.as_ptr(), format, self.format.len());
            Ok(self.raw(format))
        }
    }

    fn allocate_ptr(&self) -> WinResult<*mut AM_MEDIA_TYPE> {
        unsafe {
            let dst = CoTaskMemAlloc(size_of::<AM_MEDIA_TYPE>()).cast::<AM_MEDIA_TYPE>();
            if dst.is_null() {
                return Err(E_OUTOFMEMORY.into());
            }
            match self.allocate() {
                Ok(mt) => {
                    dst.write(mt);
                    Ok(dst)
                }
                Err(err) => {
                    CoTaskMemFree(Some(dst.cast()));
                    Err(err)
                }
            }
        }
    }
}

struct Connection {
    pin: IPin,
    media: MediaType,
    delivery: Option<Delivery>,
}

// The downstream input pin and allocator are DirectShow streaming interfaces.
// DirectShow calls their data-delivery methods on the source's streaming thread.
// Keep those two references separate from the graph and filter interfaces.
#[derive(Clone)]
struct Delivery {
    input: IMemInputPin,
    allocator: IMemAllocator,
    format: VideoFormat,
}

// SAFETY: the worker only uses DirectShow's streaming methods (GetBuffer and
// Receive), plus the corresponding COM reference releases. Those methods are
// required to support the source's streaming thread by the DirectShow contract.
unsafe impl Send for Delivery {}

pub(crate) struct Pusher {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Pusher {
    pub(crate) fn stop(self) {
        self.stop.store(true, Ordering::Release);
        self.thread.thread().unpark();
        if self.thread.thread().id() != std::thread::current().id() {
            let _ = self.thread.join();
        }
    }
}

#[implement(IPin, IAMStreamConfig, IKsPropertySet)]
pub(crate) struct OutputPin {
    parent: Mutex<Weak<IBaseFilter>>,
    connection: Mutex<Option<Connection>>,
    selected: Mutex<VideoFormat>,
    source: Arc<FrameSource>,
}

impl OutputPin {
    pub(crate) fn new(source: Arc<FrameSource>) -> ComObject<Self> {
        let (width, height) = source.size();
        ComObject::new(Self {
            parent: Mutex::new(Weak::new()),
            connection: Mutex::new(None),
            selected: Mutex::new(VideoFormat::new(width, height, PixelLayout::Rgb32)),
            source,
        })
    }

    pub(crate) fn set_filter(&self, filter: &IBaseFilter) {
        *self.parent.lock().unwrap() = filter
            .downgrade()
            .expect("SideLens filter supports weak references");
    }

    fn formats(&self) -> Vec<VideoFormat> {
        let mut modes = vec![
            (1280, 720),
            (1920, 1080),
            (640, 480),
            (2560, 1440),
            (3840, 2160),
        ];
        let current = self.source.size();
        if let Some(index) = modes.iter().position(|mode| *mode == current) {
            modes.swap(0, index);
        }
        modes
            .into_iter()
            .flat_map(|(width, height)| {
                [30, 60].into_iter().flat_map(move |fps| {
                    PixelLayout::SUPPORTED
                        .into_iter()
                        .map(move |layout| VideoFormat::new(width, height, layout).with_fps(fps))
                })
            })
            .collect()
    }

    unsafe fn parse_type(&self, mt: &AM_MEDIA_TYPE) -> Option<VideoFormat> {
        self.formats().into_iter().find_map(|fmt| fmt.parse(mt))
    }

    pub(crate) fn start(&self) -> WinResult<Option<Pusher>> {
        let delivery = self
            .connection
            .lock()
            .map_err(|_| E_FAIL)?
            .as_ref()
            .and_then(|c| c.delivery.clone());
        let Some(delivery) = delivery else {
            return Ok(None);
        };
        unsafe {
            delivery.allocator.Commit()?;
        }
        let allocator = delivery.allocator.clone();
        let source = self.source.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        match std::thread::Builder::new()
            .name("sidelens-dshow".into())
            .spawn(move || deliver(delivery, &source, &worker_stop))
        {
            Ok(thread) => Ok(Some(Pusher { stop, thread })),
            Err(_) => {
                let _ = unsafe { allocator.Decommit() };
                Err(E_FAIL.into())
            }
        }
    }

    pub(crate) fn decommit(&self) {
        let allocator = self
            .connection
            .lock()
            .ok()
            .and_then(|g| g.as_ref()?.delivery.as_ref().map(|d| d.allocator.clone()));
        if let Some(allocator) = allocator {
            let _ = unsafe { allocator.Decommit() };
        }
    }
}

unsafe fn negotiate_allocator(
    input: &IMemInputPin,
    format: VideoFormat,
) -> WinResult<IMemAllocator> {
    let mut props = input.GetAllocatorRequirements().unwrap_or_default();
    props.cBuffers = props.cBuffers.max(2);
    props.cbBuffer = props.cbBuffer.max(format.size() as i32);
    props.cbAlign = props.cbAlign.max(1);
    props.cbPrefix = props.cbPrefix.max(0);
    let configure = |allocator: &IMemAllocator| -> WinResult<()> {
        let actual = allocator.SetProperties(&props)?;
        if actual.cBuffers < props.cBuffers
            || actual.cbBuffer < props.cbBuffer
            || actual.cbAlign < props.cbAlign
            || actual.cbAlign % props.cbAlign != 0
            || actual.cbPrefix < props.cbPrefix
        {
            return Err(VFW_E_BUFFER_OVERFLOW.into());
        }
        input.NotifyAllocator(allocator, false)
    };
    if let Ok(allocator) = input.GetAllocator() {
        if configure(&allocator).is_ok() {
            return Ok(allocator);
        }
    }
    // OBS's CapturePin returns VFW_E_NO_ALLOCATOR and accepts ours in NotifyAllocator.
    let allocator: IMemAllocator =
        CoCreateInstance(&CLSID_MemoryAllocator, None, CLSCTX_INPROC_SERVER)?;
    configure(&allocator)?;
    crate::log_to_file("DirectShow 使用源滤镜分配器");
    Ok(allocator)
}

fn matches_partial(mt: &AM_MEDIA_TYPE, candidate: VideoFormat) -> bool {
    (mt.majortype == GUID::zeroed() || mt.majortype == MEDIATYPE_Video)
        && (mt.subtype == GUID::zeroed() || mt.subtype == candidate.layout.subtype())
        && (mt.formattype == GUID::zeroed() || mt.formattype == FORMAT_VideoInfo)
        && mt.cbFormat == 0
}

impl IPin_Impl for OutputPin_Impl {
    fn Connect(&self, peer: Ref<IPin>, pmt: *const AM_MEDIA_TYPE) -> WinResult<()> {
        let peer = peer.as_ref().ok_or(E_POINTER)?;
        if self.connection.lock().map_err(|_| E_FAIL)?.is_some() {
            return Err(VFW_E_ALREADY_CONNECTED.into());
        }
        if unsafe { peer.QueryDirection()? } != PINDIR_INPUT {
            return Err(VFW_E_INVALID_DIRECTION.into());
        }
        let input: IMemInputPin = peer.cast()?;
        let me: IPin = self.to_interface();
        let selected = *self.selected.lock().map_err(|_| E_FAIL)?;
        let requested = unsafe { pmt.as_ref() };
        let mut candidates = Vec::new();
        if let Some(mt) = requested.filter(|mt| mt.cbFormat != 0) {
            let video = unsafe { self.parse_type(mt) }.ok_or(VFW_E_TYPE_NOT_ACCEPTED)?;
            candidates.push((video, unsafe { MediaType::copy(mt, video) }));
        } else {
            for video in std::iter::once(selected)
                .chain(self.formats().into_iter().filter(|fmt| *fmt != selected))
            {
                if requested.map_or(true, |mt| matches_partial(mt, video)) {
                    candidates.push((video, MediaType::new(video)));
                }
            }
        }
        let mut last = VFW_E_NO_ACCEPTABLE_TYPES.into();
        for (video, mut media) in candidates {
            let format = media.format.as_mut_ptr();
            let raw = media.raw(format);
            {
                let mut connection = self.connection.lock().map_err(|_| E_FAIL)?;
                if connection.is_some() {
                    return Err(VFW_E_ALREADY_CONNECTED.into());
                }
                *connection = Some(Connection {
                    pin: peer.clone(),
                    media,
                    delivery: None,
                });
            }
            match unsafe { peer.ReceiveConnection(&me, &raw) } {
                Ok(()) => match unsafe { negotiate_allocator(&input, video) } {
                    Ok(allocator) => {
                        let mut connection = self.connection.lock().map_err(|_| E_FAIL)?;
                        let entry = connection.as_mut().ok_or(VFW_E_NOT_CONNECTED)?;
                        entry.delivery = Some(Delivery {
                            input,
                            allocator,
                            format: video,
                        });
                        *self.selected.lock().map_err(|_| E_FAIL)? = video;
                        crate::log_to_file(&format!(
                            "DirectShow 引脚已连接，格式 {:?}",
                            video.layout
                        ));
                        self.source.set_mode(video.width, video.height, video.fps);
                        return Ok(());
                    }
                    Err(err) => {
                        let _ = unsafe { peer.Disconnect() };
                        last = err;
                    }
                },
                Err(err) => last = err,
            }
            self.connection.lock().map_err(|_| E_FAIL)?.take();
        }
        Err(last)
    }

    fn ReceiveConnection(
        &self,
        _connector: Ref<IPin>,
        _pmt: *const AM_MEDIA_TYPE,
    ) -> WinResult<()> {
        Err(VFW_E_INVALID_DIRECTION.into())
    }

    fn Disconnect(&self) -> WinResult<()> {
        if let Some(connection) = self.connection.lock().map_err(|_| E_FAIL)?.take() {
            if let Some(delivery) = connection.delivery {
                unsafe {
                    delivery.allocator.Decommit()?;
                }
            }
            Ok(())
        } else {
            Err(S_FALSE.into())
        }
    }

    fn ConnectedTo(&self) -> WinResult<IPin> {
        self.connection
            .lock()
            .map_err(|_| E_FAIL)?
            .as_ref()
            .map(|c| c.pin.clone())
            .ok_or_else(|| VFW_E_NOT_CONNECTED.into())
    }

    fn ConnectionMediaType(&self, pmt: *mut AM_MEDIA_TYPE) -> WinResult<()> {
        if pmt.is_null() {
            return Err(E_POINTER.into());
        }
        let connection = self.connection.lock().map_err(|_| E_FAIL)?;
        let mt = connection
            .as_ref()
            .ok_or(VFW_E_NOT_CONNECTED)?
            .media
            .allocate()?;
        unsafe {
            pmt.write(mt);
        }
        Ok(())
    }

    fn QueryPinInfo(&self, info: *mut PIN_INFO) -> WinResult<()> {
        if info.is_null() {
            return Err(E_POINTER.into());
        }
        let filter = self
            .parent
            .lock()
            .map_err(|_| E_FAIL)?
            .upgrade()
            .ok_or(E_FAIL)?;
        let mut name = [0; 128];
        for (dst, ch) in name.iter_mut().zip("Output".encode_utf16()) {
            *dst = ch;
        }
        unsafe {
            info.write(PIN_INFO {
                pFilter: ManuallyDrop::new(Some(filter)),
                dir: PINDIR_OUTPUT,
                achName: name,
            });
        }
        Ok(())
    }

    fn QueryDirection(&self) -> WinResult<PIN_DIRECTION> {
        Ok(PINDIR_OUTPUT)
    }

    fn QueryId(&self) -> WinResult<PWSTR> {
        const NAME: &[u16] = &[79, 117, 116, 112, 117, 116, 0];
        unsafe {
            let p = CoTaskMemAlloc(std::mem::size_of_val(NAME)).cast::<u16>();
            if p.is_null() {
                return Err(E_OUTOFMEMORY.into());
            }
            std::ptr::copy_nonoverlapping(NAME.as_ptr(), p, NAME.len());
            Ok(PWSTR(p))
        }
    }

    fn QueryAccept(&self, pmt: *const AM_MEDIA_TYPE) -> HRESULT {
        let Some(mt) = (unsafe { pmt.as_ref() }) else {
            return E_POINTER;
        };
        if unsafe { self.parse_type(mt) }.is_some() {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn EnumMediaTypes(&self) -> WinResult<IEnumMediaTypes> {
        let formats = self.formats().into_iter().map(MediaType::new).collect();
        Ok(ComObject::new(MediaTypes {
            formats,
            index: Mutex::new(0),
        })
        .into_interface())
    }

    fn QueryInternalConnections(
        &self,
        _pins: windows::core::OutRef<IPin>,
        _count: *mut u32,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn EndOfStream(&self) -> WinResult<()> {
        Ok(())
    }
    fn BeginFlush(&self) -> WinResult<()> {
        Ok(())
    }
    fn EndFlush(&self) -> WinResult<()> {
        Ok(())
    }
    fn NewSegment(&self, _start: i64, _stop: i64, _rate: f64) -> WinResult<()> {
        Ok(())
    }
}

impl IAMStreamConfig_Impl for OutputPin_Impl {
    fn SetFormat(&self, pmt: *const AM_MEDIA_TYPE) -> WinResult<()> {
        let mt = unsafe { pmt.as_ref() }.ok_or(E_POINTER)?;
        let video = unsafe { self.parse_type(mt) }.ok_or(VFW_E_TYPE_NOT_ACCEPTED)?;
        if let Some(connection) = self.connection.lock().map_err(|_| E_FAIL)?.as_ref() {
            return if connection
                .delivery
                .as_ref()
                .is_some_and(|delivery| delivery.format == video)
            {
                Ok(())
            } else {
                Err(VFW_E_NOT_STOPPED.into())
            };
        }
        *self.selected.lock().map_err(|_| E_FAIL)? = video;
        Ok(())
    }

    fn GetFormat(&self) -> WinResult<*mut AM_MEDIA_TYPE> {
        if let Some(connection) = self.connection.lock().map_err(|_| E_FAIL)?.as_ref() {
            return connection.media.allocate_ptr();
        }
        MediaType::new(*self.selected.lock().map_err(|_| E_FAIL)?).allocate_ptr()
    }

    fn GetNumberOfCapabilities(&self, count: *mut i32, size: *mut i32) -> WinResult<()> {
        if count.is_null() || size.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *count = self.formats().len() as i32;
            *size = size_of::<VIDEO_STREAM_CONFIG_CAPS>() as i32;
        }
        Ok(())
    }

    fn GetStreamCaps(
        &self,
        index: i32,
        mt: *mut *mut AM_MEDIA_TYPE,
        caps: *mut u8,
    ) -> WinResult<()> {
        if mt.is_null() || caps.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *mt = std::ptr::null_mut();
        }
        let formats = self.formats();
        let video = *formats.get(index as usize).ok_or(E_INVALIDARG)?;
        let size = SIZE {
            cx: video.width as i32,
            cy: video.height as i32,
        };
        let bitrate = video.header().dwBitRate as i32;
        let data = VIDEO_STREAM_CONFIG_CAPS {
            guid: FORMAT_VideoInfo,
            InputSize: size,
            MinCroppingSize: size,
            MaxCroppingSize: size,
            CropGranularityX: 1,
            CropGranularityY: 1,
            CropAlignX: 1,
            CropAlignY: 1,
            MinOutputSize: size,
            MaxOutputSize: size,
            OutputGranularityX: 1,
            OutputGranularityY: 1,
            MinFrameInterval: video.frame_interval(),
            MaxFrameInterval: video.frame_interval(),
            MinBitsPerSecond: bitrate,
            MaxBitsPerSecond: bitrate,
            ..Default::default()
        };
        let media = MediaType::new(video).allocate_ptr()?;
        unsafe {
            caps.cast::<VIDEO_STREAM_CONFIG_CAPS>()
                .write_unaligned(data);
            *mt = media;
        }
        Ok(())
    }
}

#[implement(IEnumMediaTypes)]
struct MediaTypes {
    formats: Vec<MediaType>,
    index: Mutex<usize>,
}

impl IEnumMediaTypes_Impl for MediaTypes_Impl {
    fn Next(&self, count: u32, out: *mut *mut AM_MEDIA_TYPE, fetched: *mut u32) -> HRESULT {
        if out.is_null() || (fetched.is_null() && count != 1) {
            return E_POINTER;
        }
        let Ok(mut index) = self.index.lock() else {
            return E_FAIL;
        };
        let available = (self.formats.len() - *index).min(count as usize);
        if !fetched.is_null() {
            unsafe {
                *fetched = 0;
            }
        }
        for offset in 0..available {
            let mt = match self.formats[*index + offset].allocate_ptr() {
                Ok(mt) => mt,
                Err(err) => {
                    for previous in 0..offset {
                        unsafe {
                            let slot = out.add(previous);
                            CoTaskMemFree(Some((**slot).pbFormat.cast()));
                            CoTaskMemFree(Some((*slot).cast()));
                            *slot = std::ptr::null_mut();
                        }
                    }
                    return err.code();
                }
            };
            unsafe {
                out.add(offset).write(mt);
            }
        }
        *index += available;
        if !fetched.is_null() {
            unsafe {
                *fetched = available as u32;
            }
        }
        if available == count as usize {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, count: u32) -> WinResult<()> {
        let mut index = self.index.lock().map_err(|_| E_FAIL)?;
        let available = (self.formats.len() - *index).min(count as usize);
        *index += available;
        if available == count as usize {
            Ok(())
        } else {
            Err(S_FALSE.into())
        }
    }

    fn Reset(&self) -> WinResult<()> {
        *self.index.lock().map_err(|_| E_FAIL)? = 0;
        Ok(())
    }

    fn Clone(&self) -> WinResult<IEnumMediaTypes> {
        Ok(ComObject::new(MediaTypes {
            formats: self.formats.clone(),
            index: Mutex::new(*self.index.lock().map_err(|_| E_FAIL)?),
        })
        .into_interface())
    }
}

impl IKsPropertySet_Impl for OutputPin_Impl {
    fn Set(
        &self,
        _guidpropset: *const windows::core::GUID,
        _dwpropid: u32,
        _pinstancedata: *const core::ffi::c_void,
        _cbinstancedata: u32,
        _ppropdata: *const core::ffi::c_void,
        _cbpropdata: u32,
    ) -> WinResult<()> {
        // 只读属性集，不支持写
        Err(E_NOTIMPL.into())
    }

    fn Get(
        &self,
        guidpropset: *const windows::core::GUID,
        dwpropid: u32,
        _pinstancedata: *const core::ffi::c_void,
        _cbinstancedata: u32,
        ppropdata: *mut core::ffi::c_void,
        cbpropdata: u32,
        pcbreturned: *mut u32,
    ) -> WinResult<()> {
        if guidpropset.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: 已判空
        let propset = unsafe { *guidpropset };

        if propset != AMPROPSETID_Pin {
            return Err(E_PROP_SET_UNSUPPORTED.into());
        }
        if dwpropid != AMPROPERTY_PIN_CATEGORY.0 as u32 {
            return Err(E_PROP_ID_UNSUPPORTED.into());
        }
        if ppropdata.is_null() && pcbreturned.is_null() {
            return Err(E_POINTER.into());
        }

        if !pcbreturned.is_null() {
            // SAFETY: 已判空
            unsafe { *pcbreturned = std::mem::size_of::<windows::core::GUID>() as u32 };
        }
        if !ppropdata.is_null() {
            if cbpropdata < std::mem::size_of::<windows::core::GUID>() as u32 {
                return Err(windows::Win32::Foundation::E_UNEXPECTED.into());
            }
            // 关键：声明自己是采集引脚
            //
            // SAFETY: 调用方保证 ppropdata 至少 cbpropdata 字节
            unsafe {
                std::ptr::write_unaligned(
                    ppropdata as *mut windows::core::GUID,
                    PIN_CATEGORY_CAPTURE,
                );
            }
        }
        Ok(())
    }

    fn QuerySupported(
        &self,
        guidpropset: *const windows::core::GUID,
        dwpropid: u32,
    ) -> WinResult<u32> {
        if guidpropset.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: 已判空
        let propset = unsafe { *guidpropset };
        if propset != AMPROPSETID_Pin {
            return Err(E_PROP_SET_UNSUPPORTED.into());
        }
        if dwpropid != AMPROPERTY_PIN_CATEGORY.0 as u32 {
            return Err(E_PROP_ID_UNSUPPORTED.into());
        }
        Ok(KSPROPERTY_SUPPORT_GET)
    }
}

fn copy_frame(frame: &Frame, video: VideoFormat, output: &mut [u8]) {
    let width = video.width as usize;
    let height = video.height as usize;
    let luma = width * height;
    if frame.data.len() != luma * 3 / 2 {
        if video.layout == PixelLayout::Nv12 {
            output[..luma].fill(16);
            output[luma..].fill(128);
        } else {
            output.fill(0);
        }
        return;
    }
    if video.layout == PixelLayout::Nv12 {
        output.copy_from_slice(&frame.data);
        return;
    }

    let step = (video.layout.bits() / 8) as usize;
    let stride = video.layout.stride(width);
    for y in 0..height {
        let source_y = y * width;
        let source_uv = luma + y / 2 * width;
        let target_y = if video.bottom_up { height - 1 - y } else { y };
        let row = &mut output[target_y * stride..(target_y + 1) * stride];
        row[width * step..].fill(0);
        for x in 0..width {
            let lum = (frame.data[source_y + x] as i32 - 16).max(0) * 298;
            let uv = source_uv + (x & !1);
            let u = frame.data[uv] as i32 - 128;
            let v = frame.data[uv + 1] as i32 - 128;
            let dst = x * step;
            row[dst] = ((lum + 541 * u + 128) >> 8).clamp(0, 255) as u8;
            row[dst + 1] = ((lum - 55 * u - 136 * v + 128) >> 8).clamp(0, 255) as u8;
            row[dst + 2] = ((lum + 459 * v + 128) >> 8).clamp(0, 255) as u8;
            if step == 4 {
                row[dst + 3] = 255;
            }
        }
    }
}

fn deliver(delivery: Delivery, source: &FrameSource, stop: &AtomicBool) {
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() {
        crate::log_to_file(&format!("DirectShow 推帧线程 COM 初始化失败 {hr:?}"));
        return;
    }
    let started = Instant::now();
    let interval = delivery.format.frame_interval();
    let frame_period = Duration::from_nanos(interval as u64 * 100);
    let mut deadline = started;
    let mut sent = 0u64;
    let mut logged_error = false;
    let mut logged_live = false;
    while !stop.load(Ordering::Acquire) {
        let time = (started.elapsed().as_nanos() / 100) as i64;
        let frame = source.poll(time);
        if stop.load(Ordering::Acquire) {
            break;
        }
        match unsafe { deliver_sample(&delivery, &frame, time, sent == 0) } {
            Ok(true) => {
                if sent == 0 {
                    crate::log_to_file(&format!(
                        "DirectShow 下游已接收样本，格式 {:?}，{} 字节",
                        delivery.format.layout,
                        delivery.format.size()
                    ));
                }
                if !logged_live && source.stats().frames.load(Ordering::Relaxed) != 0 {
                    let luma = frame.width as usize * frame.height as usize;
                    let peak = frame.data[..luma]
                        .iter()
                        .step_by(97)
                        .copied()
                        .max()
                        .unwrap_or(0);
                    if peak > 16 {
                        logged_live = true;
                        crate::log_to_file(&format!(
                            "DirectShow 下游已接收非黑共享内存视频帧，亮度峰值 {peak}"
                        ));
                    }
                }
                sent += 1;
            }
            Err(err) if !logged_error => {
                logged_error = true;
                crate::log_to_file(&format!("DirectShow 投递样本失败 {err}"));
            }
            _ => {}
        }
        deadline += frame_period;
        let now = Instant::now();
        if deadline <= now {
            deadline = now;
        }
        while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
        }
    }
    drop(delivery);
    unsafe { CoUninitialize() };
}

unsafe fn deliver_sample(
    delivery: &Delivery,
    frame: &Frame,
    start: i64,
    first: bool,
) -> WinResult<bool> {
    let mut sample = None;
    delivery
        .allocator
        .GetBuffer(&mut sample, None, None, AM_GBF_NOWAIT)?;
    let sample: IMediaSample = sample.ok_or(VFW_E_NO_ALLOCATOR)?;
    let size = delivery.format.size();
    if sample.GetSize() < size as i32 {
        return Err(VFW_E_BUFFER_OVERFLOW.into());
    }
    let pixels = sample.GetPointer()?;
    if pixels.is_null() {
        return Err(E_POINTER.into());
    }
    copy_frame(
        frame,
        delivery.format,
        std::slice::from_raw_parts_mut(pixels, size),
    );
    sample.SetActualDataLength(size as i32)?;
    sample.SetTime(
        Some(&start),
        Some(&(start + delivery.format.frame_interval())),
    )?;
    sample.SetSyncPoint(true)?;
    sample.SetDiscontinuity(first)?;
    // Preserve S_FALSE (the input pin is flushing) instead of counting it as delivery.
    let hr = (Interface::vtable(&delivery.input).Receive)(delivery.input.as_raw(), sample.as_raw());
    hr.ok()?;
    Ok(hr == S_OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::BOOL;
    use windows::Win32::Media::DirectShow::{IMemInputPin_Impl, ALLOCATOR_PROPERTIES};

    #[implement(IMemInputPin)]
    struct CaptureSink {
        expected: [u8; 6],
        notified: AtomicBool,
        received: AtomicBool,
    }

    impl IMemInputPin_Impl for CaptureSink_Impl {
        fn GetAllocator(&self) -> WinResult<IMemAllocator> {
            Err(VFW_E_NO_ALLOCATOR.into())
        }

        fn NotifyAllocator(
            &self,
            allocator: Ref<IMemAllocator>,
            _read_only: BOOL,
        ) -> WinResult<()> {
            let allocator = allocator.as_ref().ok_or(E_POINTER)?;
            let properties = unsafe { allocator.GetProperties()? };
            if properties.cbBuffer < self.expected.len() as i32 {
                return Err(VFW_E_BUFFER_OVERFLOW.into());
            }
            self.notified.store(true, Ordering::Relaxed);
            Ok(())
        }

        fn GetAllocatorRequirements(&self) -> WinResult<ALLOCATOR_PROPERTIES> {
            Err(E_NOTIMPL.into())
        }

        fn Receive(&self, sample: Ref<IMediaSample>) -> WinResult<()> {
            let sample = sample.as_ref().ok_or(E_POINTER)?;
            let mut start = 0;
            let mut end = 0;
            unsafe {
                if sample.GetActualDataLength() != self.expected.len() as i32 {
                    return Err(E_FAIL.into());
                }
                sample.GetTime(&mut start, &mut end)?;
                let data = std::slice::from_raw_parts(sample.GetPointer()?, self.expected.len());
                if data != self.expected || (start, end) != (42, 42 + FRAME_INTERVAL) {
                    return Err(E_FAIL.into());
                }
            }
            self.received.store(true, Ordering::Relaxed);
            Ok(())
        }

        fn ReceiveMultiple(
            &self,
            _samples: *const Option<IMediaSample>,
            _count: i32,
        ) -> WinResult<i32> {
            Err(E_NOTIMPL.into())
        }

        fn ReceiveCanBlock(&self) -> WinResult<()> {
            Err(S_FALSE.into())
        }
    }

    #[test]
    fn delivers_video_when_capture_sink_does_not_supply_allocator() {
        struct ComGuard;
        impl Drop for ComGuard {
            fn drop(&mut self) {
                unsafe { CoUninitialize() };
            }
        }
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .unwrap();
        let _com = ComGuard;
        let expected = [42, 64, 92, 120, 128, 128];
        let sink = ComObject::new(CaptureSink {
            expected,
            notified: AtomicBool::new(false),
            received: AtomicBool::new(false),
        });
        let input: IMemInputPin = sink.to_interface();
        let format = VideoFormat::new(2, 2, PixelLayout::Nv12);
        unsafe {
            assert_eq!(input.GetAllocator().unwrap_err().code(), VFW_E_NO_ALLOCATOR);
            let allocator = negotiate_allocator(&input, format).unwrap();
            assert!(sink.notified.load(Ordering::Relaxed));
            allocator.Commit().unwrap();
            let delivery = Delivery {
                input,
                allocator,
                format,
            };
            let mut frame = Frame::black(2, 2);
            frame.data.copy_from_slice(&expected);
            assert!(deliver_sample(&delivery, &frame, 42, true).unwrap());
            assert!(sink.received.load(Ordering::Relaxed));
            delivery.allocator.Decommit().unwrap();
        }
    }

    #[test]
    fn xrgb_uses_bgr_order_and_bottom_up_scanlines() {
        let video = VideoFormat::new(2, 2, PixelLayout::Rgb32);
        let mut frame = Frame::black(2, 2);
        frame.data.copy_from_slice(&[16, 16, 110, 110, 80, 200]);
        let mut pixels = [0; 16];
        copy_frame(&frame, video, &mut pixels);
        assert_eq!(video.header().bmiHeader.biBitCount, 32);
        assert!(pixels[2] > pixels[0]);
        assert!(pixels[2] > pixels[10]);
        assert_eq!(pixels[3], 255);
    }

    #[test]
    fn nv12_keeps_pixel_height_and_copies_both_planes() {
        let video = VideoFormat::new(2, 2, PixelLayout::Nv12);
        let mut pixels = [0; 6];
        let frame = Frame::black(2, 2);
        copy_frame(&frame, video, &mut pixels);
        assert_eq!(video.header().bmiHeader.biHeight, 2);
        assert_eq!(pixels.as_slice(), frame.data);
    }
}
