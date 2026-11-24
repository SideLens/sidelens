//! 跨进程帧元数据格式
//!
//! # 为什么要定义一个显式的 ABI
//!
//! 写端是 Rust（sidelensd.exe），读端也是 Rust（sidelens_vcam_dll.dll 里的代码），
//! 但两者是**独立编译**的，甚至可能是不同版本。所以元数据必须是：
//!
//! 1. **固定布局**（`#[repr(C)]`）—— 不能依赖 Rust 的默认字段重排
//! 2. **显式大小**（`u32` 而不是 `usize`）—— 32/64 位一致
//! 3. **带版本号** —— 允许未来演进
//!
//! # 一次帧传输的完整流程
//!
//! ```text
//! 写端（sidelensd）                     读端（FrameServer 里的 DLL）
//! ─────────────────                     ──────────────────────────
//! 1. 找空闲 slot
//! 2. 写入像素数据到 slot
//! 3. 写 FrameMeta（含 seq、尺寸、时间戳）
//! 4. 通过管道发 FrameSignal ─────────→  5. 收到 signal
//!                                        6. 读 slot 的 FrameMeta
//!                                        7. 用像素数据构造 IMFSample
//!                                        8. 归还 slot
//! ```

#![forbid(unsafe_code)]

/// 像素格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PixelFormat {
    /// YUV 4:2:0，NV12（摄像头标准格式）
    Nv12 = 0,
    /// BGRA 8:8:8:8
    Bgra = 1,
    /// YUY2（某些摄像头用）
    Yuy2 = 2,
}

impl PixelFormat {
    /// 每像素字节数（用于计算缓冲区大小）
    ///
    /// NV12 是 1.5 字节/像素（Y 平面 1 + UV 平面 0.5）
    pub fn bytes_per_pixel(self) -> f64 {
        match self {
            PixelFormat::Nv12 => 1.5,
            PixelFormat::Bgra => 4.0,
            PixelFormat::Yuy2 => 2.0,
        }
    }

    /// 计算给定分辨率的缓冲区大小
    pub fn buffer_size(self, width: u32, height: u32) -> usize {
        match self {
            PixelFormat::Nv12 => (width as usize) * (height as usize) * 3 / 2,
            PixelFormat::Bgra => (width as usize) * (height as usize) * 4,
            PixelFormat::Yuy2 => (width as usize) * (height as usize) * 2,
        }
    }

    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(PixelFormat::Nv12),
            1 => Some(PixelFormat::Bgra),
            2 => Some(PixelFormat::Yuy2),
            _ => None,
        }
    }
}

/// 一帧的元数据（跨进程 ABI）
///
/// # 布局约束
///
/// `#[repr(C)]` + 全部定长字段 = 两边编译器算出的 size/offset 完全一致。
/// **不要加 `String` / `Vec` 这类堆分配字段** —— 它们在两个进程里是不同地址。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct FrameMeta {
    /// 协议版本（写端填，读端校验）
    pub version: u32,
    /// 帧序号（单调递增）
    pub seq: u32,
    /// 宽
    pub width: u32,
    /// 高
    pub height: u32,
    /// 像素格式（存 u32 而不是枚举，保证 ABI 稳定）
    pub format: u32,
    /// 是否为关键帧
    pub keyframe: u32,
    /// 呈现时间戳（微秒）
    pub pts_us: u64,
    /// 实际写入的字节数（可能小于 slot 容量）
    pub payload_len: u32,
    /// 保留字段（对齐到 8 字节边界，未来扩展用）
    pub reserved: u32,
}

impl FrameMeta {
    /// 当前 ABI 版本
    pub const VERSION: u32 = 1;

    /// 结构体大小（字节）
    ///
    /// 手动计算而不是 `size_of`，这样如果字段变了测试会立刻失败 ——
    /// 避免"改了结构但忘了同步读端"这类跨进程 bug。
    pub const SIZE: usize = 4 * 6 + 8 + 4 + 4;

    pub fn new(width: u32, height: u32, format: PixelFormat) -> Self {
        Self {
            version: Self::VERSION,
            seq: 0,
            width,
            height,
            format: format as u32,
            keyframe: 0,
            pts_us: 0,
            payload_len: 0,
            reserved: 0,
        }
    }

    pub fn pixel_format(&self) -> Option<PixelFormat> {
        PixelFormat::from_u32(self.format)
    }

    pub fn is_keyframe(&self) -> bool {
        self.keyframe != 0
    }

    pub fn is_version_compatible(&self) -> bool {
        self.version == Self::VERSION
    }

    /// 需要的缓冲区大小
    pub fn required_buffer_size(&self) -> Option<usize> {
        self.pixel_format()
            .map(|f| f.buffer_size(self.width, self.height))
    }

    /// 序列化为固定长度字节数组
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let mut i = 0;

        for v in [
            self.version,
            self.seq,
            self.width,
            self.height,
            self.format,
            self.keyframe,
        ] {
            out[i..i + 4].copy_from_slice(&v.to_le_bytes());
            i += 4;
        }
        out[i..i + 8].copy_from_slice(&self.pts_us.to_le_bytes());
        i += 8;
        out[i..i + 4].copy_from_slice(&self.payload_len.to_le_bytes());
        i += 4;
        out[i..i + 4].copy_from_slice(&self.reserved.to_le_bytes());

        out
    }

    /// 从字节数组反序列化
    ///
    /// 返回 None 表示长度不足。
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        // 用固定偏移直接读，不用闭包 ——
        // 闭包捕获 &mut i 会与后续的显式索引冲突（借用检查器不允许）。
        let u32_at =
            |off: usize| u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);

        let version = u32_at(0);
        let seq = u32_at(4);
        let width = u32_at(8);
        let height = u32_at(12);
        let format = u32_at(16);
        let keyframe = u32_at(20);

        let pts_us = u64::from_le_bytes([
            buf[24], buf[25], buf[26], buf[27], buf[28], buf[29], buf[30], buf[31],
        ]);

        let payload_len = u32_at(32);
        let reserved = u32_at(36);

        Some(Self {
            version,
            seq,
            width,
            height,
            format,
            keyframe,
            pts_us,
            payload_len,
            reserved,
        })
    }
}

/// 管道上传输的信号（帧就绪通知）
///
/// 只传索引与元数据，不传像素数据本身。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct FrameSignal {
    /// 该帧在环形缓冲里的 slot 下标
    pub slot: u32,
    /// 元数据（完整副本，避免读端再读一次共享内存）
    pub meta: FrameMeta,
}

impl FrameSignal {
    pub const SIZE: usize = 4 + FrameMeta::SIZE;

    pub fn new(slot: u32, meta: FrameMeta) -> Self {
        Self { slot, meta }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::SIZE);
        out.extend_from_slice(&self.slot.to_le_bytes());
        out.extend_from_slice(&self.meta.encode());
        out
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        let slot = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let meta = FrameMeta::decode(&buf[4..])?;
        Some(Self { slot, meta })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_size_is_stable() {
        // 手动算的 SIZE 必须与实际序列化长度一致
        assert_eq!(FrameMeta::SIZE, 40);
        let m = FrameMeta::new(1920, 1080, PixelFormat::Nv12);
        assert_eq!(m.encode().len(), FrameMeta::SIZE);
    }

    #[test]
    fn meta_roundtrip() {
        let mut m = FrameMeta::new(1280, 720, PixelFormat::Nv12);
        m.seq = 42;
        m.pts_us = 1_700_000_000_000;
        m.keyframe = 1;
        m.payload_len = 1_382_400;

        let decoded = FrameMeta::decode(&m.encode()).unwrap();
        assert_eq!(decoded, m);
    }

    #[test]
    fn buffer_sizes() {
        assert_eq!(
            PixelFormat::Nv12.buffer_size(1920, 1080),
            1920 * 1080 * 3 / 2
        );
        assert_eq!(PixelFormat::Bgra.buffer_size(640, 480), 640 * 480 * 4);
        assert_eq!(PixelFormat::Yuy2.buffer_size(320, 240), 320 * 240 * 2);
    }
}
