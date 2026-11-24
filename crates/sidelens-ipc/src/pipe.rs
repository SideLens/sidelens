//! 命名管道（消息模式）
//!
//! # 为什么用消息模式而不是字节模式
//!
//! 管道有两种模式：
//!
//! - **字节模式**：对端读到什么边界由读缓冲区大小决定，一次 ReadFile
//!   可能读到半条消息，也可能读到两条半
//! - **消息模式**（PIPE_TYPE_MESSAGE）：内核保留写入边界，
//!   一次 WriteFile 对应一次 ReadFile
//!
//! 帧信号是固定 44 字节的小结构，用消息模式可以彻底免掉
//! 「攒够 44 字节再解析」的粘包状态机 —— 少一个状态机就少一类 bug。
//!
//! # 为什么还要超时
//!
//! ReadFile 在管道上没有数据时会**无限阻塞**。虚拟摄像头跑在
//! FrameServer 的线程池里，线程被卡住会拖垮整个取帧管线。
//! 所以每次读都带 OVERLAPPED + 超时，超时就返回
//! [io::ErrorKind::TimedOut]，由调用方决定是继续等还是放弃。
//!
//! # 实例与并发
//!
//! 一个 PipeServer 用 PIPE_UNLIMITED_INSTANCES 创建，多个客户端
//! 可以连同一个管道名 —— 每个 ConnectNamedPipe 返回独立的连接。
//! 我们的场景只需要一个读者（DLL），但留出余量方便调试工具接入。

#![allow(unsafe_code)]

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED,
    ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, SetNamedPipeHandleState,
    PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// 管道名前缀（Windows 固定写法）
pub const PIPE_PREFIX: &str = "\\\\.\\pipe\\";

/// 默认读写超时
const DEFAULT_TIMEOUT_MS: u32 = 1000;

/// 从 `windows::core::Error` 里取出原始 Win32 错误码
///
/// # 为什么不能直接 `e.code().0`
///
/// `windows` crate 把 Win32 的 DWORD 错误码包成 HRESULT：
/// `HRESULT_FROM_WIN32(997)` = `0x800703E5`。
/// 直接比较 `0x800703E5 == ERROR_IO_PENDING.0 (997)` 永远为假 ——
/// 这会让「挂起的重叠 I/O」被当成真失败。
///
/// 低 16 位才是原始 Win32 码（HRESULT_FROM_WIN32 的构造方式决定了这点）。
fn win32_code(e: &windows::core::Error) -> u32 {
    (e.code().0 & 0xFFFF) as u32
}

/// 把 &str 转成以 NUL 结尾的 UTF-16
fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 拼出完整管道路径
pub fn pipe_path(name: &str) -> String {
    if name.starts_with(PIPE_PREFIX) {
        name.to_string()
    } else {
        format!("{PIPE_PREFIX}{name}")
    }
}

/// 一次重叠 I/O 的上下文
///
/// OVERLAPPED 里的 hEvent 必须由我们自己创建 ——
/// 传 NULL 时内核会用文件句柄本身当信号，多个并发 I/O 会互相唤醒错对象。
struct OverlappedIo {
    ov: OVERLAPPED,
}

impl OverlappedIo {
    fn new() -> io::Result<Self> {
        // SAFETY: 匿名事件，无名字冲突
        let ev = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|e| io::Error::from_raw_os_error(e.code().0))?;
        Ok(Self {
            ov: OVERLAPPED {
                hEvent: ev,
                ..Default::default()
            },
        })
    }

    fn event(&self) -> HANDLE {
        self.ov.hEvent
    }

    /// 等待重叠操作完成，超时返回 TimedOut
    ///
    /// # 超时必须取消挂起的 I/O
    ///
    /// 重叠 I/O 超时后，那个 OVERLAPPED 和它的事件句柄**仍然被内核引用**。
    /// 如果我们直接返回并让 OverlappedIo 被 Drop（关闭事件句柄），
    /// 内核稍后完成这个 I/O 时会往已关闭的句柄上 SetEvent ——
    /// 句柄被复用给别的对象就会误唤醒，这是个典型的定时炸弹。
    ///
    /// 所以超时路径必须先 CancelIoEx 并等到它真正结束。
    fn wait(&self, handle: HANDLE, timeout: Duration) -> io::Result<usize> {
        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        // SAFETY: event 有效
        let rc = unsafe { WaitForSingleObject(self.event(), ms) };

        if rc != WAIT_OBJECT_0 {
            // SAFETY: handle 与 ov 都有效；取消后必须等内核真正放手
            unsafe {
                let _ = CancelIoEx(handle, Some(&self.ov));
            }
            // 再等一次，让取消完成（事件会被信号）
            // SAFETY: event 有效
            unsafe { WaitForSingleObject(self.event(), 1000) };

            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("管道 I/O 超时（{ms}ms）"),
            ));
        }

        let mut transferred = 0u32;
        // SAFETY: ov 是本对象持有的有效结构；handle 是有效管道句柄
        unsafe { GetOverlappedResult(handle, &self.ov, &mut transferred, false) }.map_err(|e| {
            let code = win32_code(&e);
            match code {
                // 对端关闭 —— 这在「客户端进程退出」时是正常事件，不是异常
                c if c == ERROR_BROKEN_PIPE.0 || c == ERROR_PIPE_NOT_CONNECTED.0 => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "对端已关闭管道")
                }
                // 我们自己取消了它
                c if c == ERROR_OPERATION_ABORTED.0 => {
                    io::Error::new(io::ErrorKind::TimedOut, "管道 I/O 已取消")
                }
                c => io::Error::from_raw_os_error(c as i32),
            }
        })?;
        Ok(transferred as usize)
    }
}

impl Drop for OverlappedIo {
    fn drop(&mut self) {
        if !self.ov.hEvent.is_invalid() {
            // SAFETY: 事件句柄由 CreateEventW 创建，只关一次
            unsafe {
                let _ = CloseHandle(self.ov.hEvent);
            }
        }
    }
}

impl std::fmt::Debug for OverlappedIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlappedIo").finish_non_exhaustive()
    }
}

/// 管道服务端
///
/// 由 sidelensd.exe 创建，等待虚拟摄像头 DLL 连上来。
#[derive(Debug)]
pub struct PipeServer {
    handle: HANDLE,
    path: String,
    timeout: Duration,
}

// SAFETY: 管道句柄是内核对象，跨线程使用是允许的；
// 并发读写由调用方串行化（我们的写端是单线程帧循环）。
unsafe impl Send for PipeServer {}

impl PipeServer {
    /// 创建管道实例
    ///
    /// first_instance 为 true 时加 FILE_FLAG_FIRST_PIPE_INSTANCE，
    /// 这样名字被别的进程占用会立刻失败而不是悄悄共享 ——
    /// 排查「连到了别人的管道」这类问题非常有用。
    pub fn create(name: &str, first_instance: bool) -> io::Result<Self> {
        let path = pipe_path(name);
        let w = wide(&path);

        let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first_instance {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }

        // SAFETY: w 以 NUL 结尾且活得够久
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(w.as_ptr()),
                open_mode,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                DEFAULT_TIMEOUT_MS,
                None,
            )
        };

        if handle == INVALID_HANDLE_VALUE || handle.is_invalid() {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            handle,
            path,
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS as u64),
        })
    }

    /// 完整管道路径
    pub fn path(&self) -> &str {
        &self.path
    }

    /// 设置读写超时
    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t;
    }

    /// 等待客户端连接
    ///
    /// 返回 Ok(true) 表示连上了，Ok(false) 表示超时。
    pub fn accept(&self, timeout: Duration) -> io::Result<bool> {
        let ov = OverlappedIo::new()?;
        // SAFETY: handle 有效，ov 活得够久
        let rc = unsafe { ConnectNamedPipe(self.handle, Some(&ov.ov as *const _ as *mut _)) };

        match rc {
            Ok(()) => Ok(true),
            Err(e) => {
                let code = win32_code(&e);
                if code == ERROR_PIPE_CONNECTED.0 {
                    // 客户端在 ConnectNamedPipe 之前就打开了 —— 已经算连上
                    return Ok(true);
                }
                if code != ERROR_IO_PENDING.0 {
                    return Err(io::Error::from_raw_os_error(code as i32));
                }
                match ov.wait(self.handle, timeout) {
                    Ok(_) => Ok(true),
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                        // 超时后必须取消挂起的连接，否则这个实例不可复用
                        // SAFETY: handle 有效
                        unsafe {
                            let _ = CancelIoEx(self.handle, None);
                        }
                        Ok(false)
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// 阻塞等待连接（无超时上限，用于服务端主循环）
    pub fn accept_blocking(&self) -> io::Result<()> {
        let ov = OverlappedIo::new()?;
        // SAFETY: handle 有效
        let rc = unsafe { ConnectNamedPipe(self.handle, Some(&ov.ov as *const _ as *mut _)) };
        match rc {
            Ok(()) => Ok(()),
            Err(e) => {
                let code = win32_code(&e);
                if code == ERROR_PIPE_CONNECTED.0 {
                    return Ok(());
                }
                if code != ERROR_IO_PENDING.0 {
                    return Err(io::Error::from_raw_os_error(code as i32));
                }
                // 无限等待
                // SAFETY: event 有效
                let rc = unsafe { WaitForSingleObject(ov.event(), u32::MAX) };
                if rc != WAIT_OBJECT_0 {
                    return Err(io::Error::last_os_error());
                }
                let mut transferred = 0u32;
                // SAFETY: 结构有效
                unsafe { GetOverlappedResult(self.handle, &ov.ov, &mut transferred, false) }
                    .map_err(|e| io::Error::from_raw_os_error(e.code().0))?;
                Ok(())
            }
        }
    }

    /// 写一条消息（消息模式下一次 WriteFile 就是一条完整消息）
    pub fn write_message(&self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let ov = OverlappedIo::new()?;
        let mut written = 0u32;
        // SAFETY: data 活得够久；ov 有效
        let rc = unsafe {
            WriteFile(
                self.handle,
                Some(data),
                Some(&mut written),
                Some(&ov.ov as *const _ as *mut _),
            )
        };

        match rc {
            Ok(()) => Ok(()),
            Err(e) => {
                let code = win32_code(&e);
                if code != ERROR_IO_PENDING.0 {
                    return Err(io::Error::from_raw_os_error(code as i32));
                }
                let n = ov.wait(self.handle, self.timeout)?;
                if n != data.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        format!("短写：期望 {} 实际 {n}", data.len()),
                    ));
                }
                Ok(())
            }
        }
    }

    /// 读一条消息，返回实际读到的字节数
    pub fn read_message(&self, buf: &mut [u8]) -> io::Result<usize> {
        let ov = OverlappedIo::new()?;
        let mut read = 0u32;
        // SAFETY: buf 活得够久；ov 有效
        let rc = unsafe {
            ReadFile(
                self.handle,
                Some(buf),
                Some(&mut read),
                Some(&ov.ov as *const _ as *mut _),
            )
        };

        match rc {
            Ok(()) => Ok(read as usize),
            Err(e) => {
                let code = win32_code(&e);
                if code != ERROR_IO_PENDING.0 {
                    return Err(io::Error::from_raw_os_error(code as i32));
                }
                ov.wait(self.handle, self.timeout)
            }
        }
    }

    /// 断开当前客户端，准备接受下一个
    pub fn disconnect(&self) {
        // SAFETY: handle 有效
        unsafe {
            let _ = DisconnectNamedPipe(self.handle);
        }
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: handle 由 CreateNamedPipeW 创建
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

/// 管道客户端
///
/// 由虚拟摄像头 DLL 创建，连到 sidelensd.exe 的管道。
#[derive(Debug)]
pub struct PipeClient {
    handle: HANDLE,
    path: String,
    timeout: Duration,
}

// SAFETY: 同 PipeServer
unsafe impl Send for PipeClient {}

impl PipeClient {
    /// 连接已存在的管道
    ///
    /// 带重试 —— DLL 可能在 sidelensd 还没创建管道时就被 FrameServer 加载。
    pub fn connect(name: &str, retries: u32, interval: Duration) -> io::Result<Self> {
        let path = pipe_path(name);
        let w = wide(&path);

        let mut last_err = None;
        for attempt in 0..=retries {
            // SAFETY: w 以 NUL 结尾且活得够久
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(w.as_ptr()),
                    (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    None,
                )
            };

            match handle {
                Ok(h) if !h.is_invalid() => {
                    return Ok(Self {
                        handle: h,
                        path,
                        timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS as u64),
                    });
                }
                Ok(_) => last_err = Some(io::Error::last_os_error()),
                Err(e) => last_err = Some(io::Error::from_raw_os_error(e.code().0)),
            }

            if attempt < retries {
                std::thread::sleep(interval);
            }
        }

        Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("连接管道失败：{path}"))
        }))
    }

    /// 完整管道路径
    pub fn path(&self) -> &str {
        &self.path
    }

    /// 设置读写超时
    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t;
    }

    /// 写一条消息
    pub fn write_message(&self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let ov = OverlappedIo::new()?;
        let mut written = 0u32;
        // SAFETY: data 活得够久；ov 有效
        let rc = unsafe {
            WriteFile(
                self.handle,
                Some(data),
                Some(&mut written),
                Some(&ov.ov as *const _ as *mut _),
            )
        };
        match rc {
            Ok(()) => Ok(()),
            Err(e) => {
                let code = win32_code(&e);
                if code != ERROR_IO_PENDING.0 {
                    return Err(io::Error::from_raw_os_error(code as i32));
                }
                ov.wait(self.handle, self.timeout)?;
                Ok(())
            }
        }
    }

    /// 读一条消息
    pub fn read_message(&self, buf: &mut [u8]) -> io::Result<usize> {
        let ov = OverlappedIo::new()?;
        let mut read = 0u32;
        // SAFETY: buf 活得够久；ov 有效
        let rc = unsafe {
            ReadFile(
                self.handle,
                Some(buf),
                Some(&mut read),
                Some(&ov.ov as *const _ as *mut _),
            )
        };
        match rc {
            Ok(()) => Ok(read as usize),
            Err(e) => {
                let code = win32_code(&e);
                if code != ERROR_IO_PENDING.0 {
                    return Err(io::Error::from_raw_os_error(code as i32));
                }
                ov.wait(self.handle, self.timeout)
            }
        }
    }

    /// 把管道切到消息读模式
    ///
    /// 服务端建的是消息模式，但客户端默认是字节读模式，
    /// 必须显式切一次才能保证一次 ReadFile 拿到一条完整消息。
    pub fn set_message_mode(&self) -> io::Result<()> {
        let mode = PIPE_READMODE_MESSAGE;
        // SAFETY: handle 有效
        unsafe { SetNamedPipeHandleState(self.handle, Some(&mode), None, None) }
            .map_err(|e| io::Error::from_raw_os_error(e.code().0))
    }
}

impl Drop for PipeClient {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: handle 由 CreateFileW 打开
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn unique(tag: &str) -> String {
        format!(
            "sidelens-pipe-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    }

    #[test]
    fn pipe_path_has_windows_prefix() {
        assert_eq!(pipe_path("abc"), "\\\\.\\pipe\\abc");
        // 已带前缀时不重复加
        assert_eq!(pipe_path("\\\\.\\pipe\\abc"), "\\\\.\\pipe\\abc");
    }

    #[test]
    fn server_client_message_roundtrip() {
        let name = unique("rt");
        let server = PipeServer::create(&name, true).expect("创建服务端");
        assert!(server.path().contains(&name));

        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        let name2 = name.clone();

        let t = std::thread::spawn(move || {
            let client =
                PipeClient::connect(&name2, 20, Duration::from_millis(20)).expect("客户端连接");
            client.set_message_mode().expect("切消息模式");
            client
                .write_message(b"hello-from-client")
                .expect("客户端写");
            done2.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(100));
        });

        assert!(
            server.accept(Duration::from_secs(5)).expect("accept"),
            "应连上"
        );
        let mut buf = [0u8; 256];
        let n = server.read_message(&mut buf).expect("服务端读");
        assert_eq!(&buf[..n], b"hello-from-client");

        t.join().unwrap();
        assert!(done.load(Ordering::SeqCst));
    }

    #[test]
    fn message_boundaries_are_preserved() {
        let name = unique("bound");
        let server = PipeServer::create(&name, true).expect("创建");
        let name2 = name.clone();

        let t = std::thread::spawn(move || {
            let client = PipeClient::connect(&name2, 20, Duration::from_millis(20)).expect("连接");
            client.set_message_mode().expect("消息模式");
            // 连写三条，读端必须一条一条拿到
            client.write_message(b"AAA").unwrap();
            client.write_message(b"BBBBB").unwrap();
            client.write_message(b"C").unwrap();
            // 等读端读完再退出，避免句柄提前关闭
            std::thread::sleep(Duration::from_millis(200));
        });

        server.accept(Duration::from_secs(5)).expect("accept");
        let mut buf = [0u8; 256];

        let n1 = server.read_message(&mut buf).expect("第 1 条");
        assert_eq!(&buf[..n1], b"AAA", "消息模式必须保留写入边界");
        let n2 = server.read_message(&mut buf).expect("第 2 条");
        assert_eq!(&buf[..n2], b"BBBBB");
        let n3 = server.read_message(&mut buf).expect("第 3 条");
        assert_eq!(&buf[..n3], b"C");

        t.join().unwrap();
    }

    #[test]
    fn read_times_out_when_no_data() {
        let name = unique("timeout");
        let mut server = PipeServer::create(&name, true).expect("创建");
        // 超时必须短于客户端存活时间，否则先等到的是「对端关闭」而不是超时
        server.set_timeout(Duration::from_millis(200));
        let name2 = name.clone();

        let t = std::thread::spawn(move || {
            let client = PipeClient::connect(&name2, 20, Duration::from_millis(20)).expect("连接");
            client.set_message_mode().expect("消息模式");
            // 什么都不写，让服务端读超时
            std::thread::sleep(Duration::from_millis(900));
        });

        server.accept(Duration::from_secs(5)).expect("accept");
        let mut buf = [0u8; 64];
        let err = server.read_message(&mut buf).unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::TimedOut,
            "无数据应超时而不是挂死"
        );

        t.join().unwrap();
    }

    #[test]
    fn accept_times_out_without_client() {
        let name = unique("noclient");
        let server = PipeServer::create(&name, true).expect("创建");
        let connected = server.accept(Duration::from_millis(150)).expect("accept");
        assert!(!connected, "没有客户端时应返回 false 而不是阻塞");
    }

    #[test]
    fn second_first_instance_is_rejected() {
        let name = unique("dup");
        let _first = PipeServer::create(&name, true).expect("第一个");
        // 同名 + FILE_FLAG_FIRST_PIPE_INSTANCE 必须失败
        assert!(
            PipeServer::create(&name, true).is_err(),
            "同名首实例应创建失败，防止误连别人的管道"
        );
    }

    #[test]
    fn connect_to_missing_pipe_fails_after_retries() {
        let name = unique("nosuch");
        let start = std::time::Instant::now();
        let r = PipeClient::connect(&name, 2, Duration::from_millis(10));
        assert!(r.is_err());
        assert!(start.elapsed() >= Duration::from_millis(20), "应重试过");
    }

    #[test]
    fn frame_signal_survives_pipe() {
        use crate::wire::{FrameMeta, FrameSignal, PixelFormat};

        let name = unique("sig");
        let server = PipeServer::create(&name, true).expect("创建");
        let name2 = name.clone();

        let mut meta = FrameMeta::new(1280, 720, PixelFormat::Nv12);
        meta.seq = 777;
        meta.pts_us = 1_700_000_123_456;
        meta.keyframe = 1;
        meta.payload_len = 1_382_400;
        let sig = FrameSignal::new(3, meta);

        let t = std::thread::spawn(move || {
            let client = PipeClient::connect(&name2, 20, Duration::from_millis(20)).expect("连接");
            client.set_message_mode().expect("消息模式");
            client.write_message(&sig.encode()).expect("写信号");
            std::thread::sleep(Duration::from_millis(150));
        });

        server.accept(Duration::from_secs(5)).expect("accept");
        let mut buf = [0u8; 256];
        let n = server.read_message(&mut buf).expect("读信号");
        let got = FrameSignal::decode(&buf[..n]).expect("解码");
        assert_eq!(got.slot, 3);
        assert_eq!(got.meta.seq, 777);
        assert_eq!(got.meta.pts_us, 1_700_000_123_456);
        assert!(got.meta.is_keyframe());

        t.join().unwrap();
    }
}
