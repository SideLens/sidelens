//! 共享内存映射
//!
//! # 为什么必须零拷贝
//!
//! 1080p NV12 一帧 3 MB，60fps 就是 180 MB/s。如果走管道传，
//! 写端拷贝一次、内核缓冲一次、读端再拷贝一次 —— 三次内存带宽全烧在
//! 搬运上，还会把帧的到达时间拖到毫秒级。
//!
//! 共享内存让两个进程映射同一段物理页：写端写进去，读端直接读，
//! 中间没有一次 memcpy。
//!
//! # 命名空间陷阱（真实踩坑）
//!
//! Windows 的内核对象命名空间分三种：
//!
//! | 前缀 | 可见范围 | 谁能创建 |
//! |------|----------|----------|
//! | （无） | 当前会话 | 任何人 |
//! | Local | 当前会话 | 任何人 |
//! | Global | 全机器所有会话 | 需要 SeCreateGlobalPrivilege |
//!
//! 麻烦在于：FrameServer 跑在 session 0（svchost.exe -k Camera），
//! 而 sidelensd.exe 跑在用户会话（session 1+）。Local 命名的东西
//! 两边互相看不见，必须用 Global。但普通用户默认没有
//! SeCreateGlobalPrivilege。
//!
//! 所以这里的策略是：先试 Global，失败退回 Local，
//! 并把实际用到的名字记下来交给对端。同会话场景（用户直接跑
//! 桌面程序取流）用 Local 就够；跨会话场景需要 sidelensd 以
//! 服务身份运行，那时 Global 自然可用。

#![allow(unsafe_code)]

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

/// 全局命名空间前缀
const NS_GLOBAL: &str = "Global\\";
/// 会话命名空间前缀
const NS_LOCAL: &str = "Local\\";

/// 把 &str 转成以 NUL 结尾的 UTF-16
fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 按 prefer_global 生成候选对象名
fn candidates(name: &str, prefer_global: bool) -> Vec<String> {
    if prefer_global {
        vec![format!("{NS_GLOBAL}{name}"), format!("{NS_LOCAL}{name}")]
    } else {
        vec![format!("{NS_LOCAL}{name}")]
    }
}

/// 一段跨进程共享的内存映射
///
/// # 生命周期
///
/// - create 创建并持有内核对象；进程退出时内核对象自动回收
/// - open 打开已存在的对象
/// - Drop 会 UnmapViewOfFile + CloseHandle
///
/// # 线程安全
///
/// 映射本身可以在任意线程访问（同一进程内所有线程共享地址空间）。
/// 但并发读写的同步不在这层 —— 那是 [crate::ring] 的
/// SlotIndex 与管道的职责。
#[derive(Debug)]
pub struct ShmMapping {
    handle: HANDLE,
    base: *mut u8,
    size: usize,
    /// 实际使用的完整对象名（可能带 Global 或 Local 前缀）
    name: String,
    /// 是否由本对象创建
    owned: bool,
}

// SAFETY: 映射区是两个进程共享的裸内存，本来就没有 Rust 的所有权语义。
// 跨线程传递句柄与基址是安全的 —— 真正的同步由环形缓冲的序号协议保证。
unsafe impl Send for ShmMapping {}
unsafe impl Sync for ShmMapping {}

impl ShmMapping {
    /// 创建共享内存（大小向上取整到页边界由系统完成）
    ///
    /// 同名对象已存在时直接打开它 —— 这让「先启动的一方创建」的
    /// 竞态自然解决。
    pub fn create(name: &str, size: usize) -> io::Result<Self> {
        Self::create_with_prefix(name, size, true)
    }

    /// 创建，prefer_global 决定是否先尝试全局命名空间
    pub fn create_with_prefix(name: &str, size: usize, prefer_global: bool) -> io::Result<Self> {
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "共享内存大小不能为 0",
            ));
        }

        let mut last_err = None;
        for full in candidates(name, prefer_global) {
            let w = wide(&full);
            // SAFETY: INVALID_HANDLE_VALUE 表示用页面文件后备；w 以 NUL 结尾且活得够久
            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    None,
                    PAGE_READWRITE,
                    (size >> 32) as u32,
                    (size & 0xFFFF_FFFF) as u32,
                    PCWSTR(w.as_ptr()),
                )
            };

            match handle {
                Ok(h) => {
                    // SAFETY: h 是刚创建/打开的映射对象
                    let view = unsafe { MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, size) };
                    if !view.Value.is_null() {
                        return Ok(Self {
                            handle: h,
                            base: view.Value as *mut u8,
                            size,
                            name: full,
                            owned: true,
                        });
                    }
                    // SAFETY: h 有效且尚未交给别人
                    unsafe {
                        let _ = CloseHandle(h);
                    }
                    last_err = Some(io::Error::last_os_error());
                }
                Err(e) => last_err = Some(io::Error::from_raw_os_error(e.code().0)),
            }
        }

        Err(last_err.unwrap_or_else(|| io::Error::other("创建共享内存失败：无可用命名空间")))
    }

    /// 打开已存在的共享内存
    ///
    /// size 用于映射视图；传 0 表示映射整个对象。
    pub fn open(name: &str, size: usize) -> io::Result<Self> {
        Self::open_with_prefix(name, size, true)
    }

    /// 打开，prefer_global 决定先试哪个命名空间
    pub fn open_with_prefix(name: &str, size: usize, prefer_global: bool) -> io::Result<Self> {
        let mut last_err = None;
        for full in candidates(name, prefer_global) {
            let w = wide(&full);
            // SAFETY: w 以 NUL 结尾且活得够久
            let handle =
                unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(w.as_ptr())) };
            match handle {
                Ok(h) => {
                    // SAFETY: h 是刚打开的映射对象
                    let view = unsafe { MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, size) };
                    if !view.Value.is_null() {
                        return Ok(Self {
                            handle: h,
                            base: view.Value as *mut u8,
                            size,
                            name: full,
                            owned: false,
                        });
                    }
                    // SAFETY: h 有效
                    unsafe {
                        let _ = CloseHandle(h);
                    }
                    last_err = Some(io::Error::last_os_error());
                }
                Err(e) => last_err = Some(io::Error::from_raw_os_error(e.code().0)),
            }
        }

        Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "打开共享内存失败：对象不存在")
        }))
    }

    /// 实际使用的完整对象名
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 映射大小（字节）
    pub fn size(&self) -> usize {
        self.size
    }

    /// 是否由本对象创建
    pub fn is_owner(&self) -> bool {
        self.owned
    }

    /// 基址（裸指针）
    pub fn as_ptr(&self) -> *mut u8 {
        self.base
    }

    /// 只读视图
    ///
    /// # Safety
    ///
    /// 调用方必须保证读的区间内没有并发的写。跨进程同步靠
    /// [crate::ring::SlotIndex] 的序号协议，不是靠 Rust 的借用检查。
    pub unsafe fn as_slice(&self) -> &[u8] {
        // SAFETY: base 指向至少 size 字节的有效映射
        std::slice::from_raw_parts(self.base, self.size)
    }

    /// 可写视图
    ///
    /// # Safety
    ///
    /// 调用方必须保证写区间不与任何并发读重叠。
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn as_mut_slice(&self) -> &mut [u8] {
        // SAFETY: base 指向至少 size 字节的有效映射
        std::slice::from_raw_parts_mut(self.base, self.size)
    }

    /// 在指定偏移写一段数据
    ///
    /// # Safety
    ///
    /// offset + data.len() 必须不超过映射大小，且该区间没有并发读。
    pub unsafe fn write_at(&self, offset: usize, data: &[u8]) -> io::Result<()> {
        if offset.saturating_add(data.len()) > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "写入越界：offset={offset} len={} size={}",
                    data.len(),
                    self.size
                ),
            ));
        }
        // SAFETY: 上面已校验边界
        std::ptr::copy_nonoverlapping(data.as_ptr(), self.base.add(offset), data.len());
        Ok(())
    }

    /// 从指定偏移读一段数据
    ///
    /// # Safety
    ///
    /// offset + len 必须不超过映射大小，且该区间没有并发写。
    pub unsafe fn read_at(&self, offset: usize, len: usize) -> io::Result<&[u8]> {
        if offset.saturating_add(len) > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("读取越界：offset={offset} len={len} size={}", self.size),
            ));
        }
        // SAFETY: 上面已校验边界
        Ok(std::slice::from_raw_parts(self.base.add(offset), len))
    }
}

impl Drop for ShmMapping {
    fn drop(&mut self) {
        if !self.base.is_null() {
            // SAFETY: base 来自 MapViewOfFile，且只释放一次
            unsafe {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.base as *mut _,
                });
            }
            self.base = std::ptr::null_mut();
        }
        if !self.handle.is_invalid() {
            // SAFETY: handle 来自 CreateFileMappingW / OpenFileMappingW
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用唯一名字，避免并行测试互撞
    fn unique(tag: &str) -> String {
        format!(
            "sidelens-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    }

    #[test]
    fn create_map_write_read_roundtrip() {
        let name = unique("rw");
        let shm = ShmMapping::create(&name, 4096).expect("创建共享内存");
        assert_eq!(shm.size(), 4096);
        assert!(shm.is_owner());

        let payload = b"SideLens zero-copy frame";
        // SAFETY: 单线程，无并发访问
        unsafe {
            shm.write_at(128, payload).unwrap();
            let got = shm.read_at(128, payload.len()).unwrap();
            assert_eq!(got, payload);
        }
    }

    #[test]
    fn second_view_sees_same_bytes() {
        let name = unique("shared");
        let writer = ShmMapping::create(&name, 8192).expect("创建");

        // 同一进程内再开一次，等价于另一个进程打开同一对象
        let reader = ShmMapping::open(&name, 8192).expect("打开");
        assert!(!reader.is_owner());
        assert_eq!(reader.size(), 8192);

        // SAFETY: 写入完成后才读，无并发
        unsafe {
            writer.write_at(0, &[1, 2, 3, 4, 5]).unwrap();
            assert_eq!(reader.read_at(0, 5).unwrap(), &[1, 2, 3, 4, 5]);
        }
    }

    #[test]
    fn out_of_bounds_write_is_rejected() {
        let name = unique("oob");
        let shm = ShmMapping::create(&name, 1024).expect("创建");
        // SAFETY: 越界写入会被函数自己拒绝，不会真的碰内存
        let err = unsafe { shm.write_at(1020, &[0u8; 8]) }.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let err = unsafe { shm.read_at(1000, 100) }.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn zero_size_is_rejected() {
        let name = unique("zero");
        assert!(ShmMapping::create(&name, 0).is_err());
    }

    #[test]
    fn opening_missing_object_fails() {
        let name = unique("missing");
        assert!(ShmMapping::open(&name, 1024).is_err());
    }
}
