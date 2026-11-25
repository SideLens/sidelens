//! 发布包的取回层：file:// / 本地路径 / http:// / https://。
//!
//! # 为什么自己写 HTTP 客户端
//!
//! 三选一的局面：
//!
//! | 方案 | 代价 |
//! | --- | --- |
//! | reqwest | 拉 tokio-tls 全家桶，release 体积与编译时间都翻倍；本仓库刻意只在 sidelensd 里用 tokio 的 net/time |
//! | ureq | 依赖 rustls + ring，离线环境装不上 |
//! | 手写 | 只需要 GET + Content-Length/chunked + 最多 5 次重定向 |
//!
//! 更新器要下载的东西是固定的几 MB 二进制，不需要连接池、压缩、cookie、
//! HTTP/2。手写一个够用的 GET（http.rs）加上 Windows 自带的 WinHTTP 处理
//! TLS（winhttp.rs），是这里最小的正确解。
//!
//! # 可测性
//!
//! http.rs 是纯 std TcpStream，因此 tests/update_e2e.rs 可以在任何平台上
//! 起一个本地 HTTP 服务，真的走一遍「下载 → 校验 → 替换」，不需要外网。

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum FetchError {
    Unsupported(String),
    Io(String),
    Http { status: u16, reason: String },
    TooLarge { limit: usize },
    RedirectLoop,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unsupported(scheme) => write!(
                f,
                "不支持的地址 \"{scheme}\"（支持 file://、本地路径、http://、https://）"
            ),
            FetchError::Io(e) => write!(f, "取回失败：{e}"),
            FetchError::Http { status, reason } => write!(f, "服务器返回 {status} {reason}"),
            FetchError::TooLarge { limit } => {
                write!(f, "响应超过 {limit} 字节上限，拒绝继续下载")
            }
            FetchError::RedirectLoop => write!(f, "重定向超过 5 次"),
        }
    }
}

impl std::error::Error for FetchError {}

/// 单次取回的字节上限
pub const MAX_FETCH: usize = 512 * 1024 * 1024;

/// 取回一个地址的完整内容。
///
/// base 用于解析相对路径：清单里写的是相对 URL 时，以清单自身的所在位置
/// 为基准 —— 这与浏览器里 a href 的语义一致，运维把几个文件放在同一个
/// 目录就能工作。
pub fn fetch(url: &str, base: Option<&str>) -> Result<Vec<u8>, FetchError> {
    let resolved = resolve(url, base);
    if let Some(rest) = resolved.strip_prefix("file://") {
        return read_file(Path::new(strip_file_prefix(rest)));
    }
    if let Some(rest) = resolved.strip_prefix("http://") {
        return http::get(rest, MAX_FETCH);
    }
    if let Some(rest) = resolved.strip_prefix("https://") {
        return https(rest);
    }
    // 带 scheme 但不是我们支持的那三个 —— 明确拒绝，而不是把它当本地路径
    // 去 open()。否则用户会把 ftp://… 看成一串「文件名语法不正确」的本地错误，
    // 根本猜不到是协议不支持。
    //
    // 判据：'://' 之前的部分必须全是字母且至少两个字符。这样 Windows 盘符
    // 路径 D:\x 里的冒号不会被误判（它后面紧跟反斜杠而不是两个斜杠）。
    if let Some(index) = resolved.find("://") {
        let scheme = &resolved[..index];
        if scheme.len() >= 2 && scheme.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err(FetchError::Unsupported(scheme.to_string()));
        }
    }
    read_file(Path::new(&resolved))
}

fn read_file(path: &Path) -> Result<Vec<u8>, FetchError> {
    let metadata = std::fs::metadata(path).map_err(|e| FetchError::Io(format!("{path:?}: {e}")))?;
    if metadata.len() as usize > MAX_FETCH {
        return Err(FetchError::TooLarge { limit: MAX_FETCH });
    }
    std::fs::read(path).map_err(|e| FetchError::Io(format!("{path:?}: {e}")))
}

/// file:///C:/x 里那个多余的前导斜杠必须去掉。
///
/// 不去的话，Windows 上的 Path::new("/C:/x") 会被当成「当前盘下的 C: 目录」，
/// 解析成一个不存在的路径 —— 现象是「清单明明存在却报找不到」。
fn strip_file_prefix(rest: &str) -> &str {
    let trimmed = rest.trim_start_matches('/');
    if trimmed.len() >= 2 && trimmed.as_bytes()[1] == b':' {
        trimmed
    } else {
        rest
    }
}

/// 相对地址解析
pub fn resolve(url: &str, base: Option<&str>) -> String {
    if url.contains("://") || (url.len() >= 2 && url.as_bytes()[1] == b':') {
        return url.to_string();
    }
    let Some(base) = base else {
        return url.to_string();
    };
    if base.contains("://") {
        let cut = base.rfind('/').map(|i| i + 1).unwrap_or(base.len());
        return format!("{}{}", &base[..cut], url);
    }
    let path = PathBuf::from(base);
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(url).display().to_string(),
        _ => url.to_string(),
    }
}

#[cfg(windows)]
fn https(rest: &str) -> Result<Vec<u8>, FetchError> {
    winhttp::get(rest, MAX_FETCH)
}

#[cfg(not(windows))]
fn https(_rest: &str) -> Result<Vec<u8>, FetchError> {
    Err(FetchError::Unsupported(
        "https:// —— 非 Windows 平台没有内置 TLS，请用 http:// 或本地路径".into(),
    ))
}

/// 最小 HTTP/1.1 客户端：GET + Content-Length/chunked + 最多 5 次重定向。
///
/// 刻意不支持：连接复用、压缩、cookie、HTTP/2、认证。这些都是更新器不需要
/// 的东西，每一个都会带来一个必须单独测的失败模式。
pub mod http {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    use super::FetchError;

    const REDIRECT_LIMIT: usize = 5;
    const TIMEOUT: Duration = Duration::from_secs(30);

    pub fn get(url: &str, limit: usize) -> Result<Vec<u8>, FetchError> {
        let mut current = url.to_string();
        for _ in 0..=REDIRECT_LIMIT {
            match request_once(&current, limit)? {
                Response::Body(bytes) => return Ok(bytes),
                Response::Redirect(location) => {
                    // Location 可能是绝对地址，也可能是 /path 相对地址
                    current = if location.starts_with('/') {
                        let host = current.split('/').next().unwrap_or("").to_string();
                        format!("{host}{location}")
                    } else if location.contains("://") {
                        let stripped = location
                            .strip_prefix("http://")
                            .or_else(|| location.strip_prefix("https://"))
                            .unwrap_or(&location);
                        stripped.to_string()
                    } else {
                        super::resolve(&location, Some(&format!("http://{current}")))
                            .trim_start_matches("http://")
                            .to_string()
                    };
                }
            }
        }
        Err(FetchError::RedirectLoop)
    }

    enum Response {
        Body(Vec<u8>),
        Redirect(String),
    }

    fn request_once(target: &str, limit: usize) -> Result<Response, FetchError> {
        let (authority, path) = match target.find('/') {
            Some(index) => (&target[..index], &target[index..]),
            None => (target, "/"),
        };
        let address = if authority.contains(':') {
            authority.to_string()
        } else {
            format!("{authority}:80")
        };
        let socket_address = address
            .to_socket_addrs()
            .map_err(|e| FetchError::Io(format!("解析 {address} 失败：{e}")))?
            .next()
            .ok_or_else(|| FetchError::Io(format!("{address} 没有可用地址")))?;

        let mut stream = TcpStream::connect_timeout(&socket_address, TIMEOUT)
            .map_err(|e| FetchError::Io(format!("连接 {address} 失败：{e}")))?;
        stream
            .set_read_timeout(Some(TIMEOUT))
            .map_err(|e| FetchError::Io(format!("设置超时失败：{e}")))?;

        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: SideLens-update/1\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|e| FetchError::Io(format!("发送请求失败：{e}")))?;

        let mut reader = BufReader::new(stream);
        let (status, headers) = read_headers(&mut reader)?;
        match status {
            200..=299 => {}
            301 | 302 | 303 | 307 | 308 => {
                let location = headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("location"))
                    .map(|(_, value)| value.clone())
                    .ok_or_else(|| FetchError::Http {
                        status,
                        reason: "重定向响应缺少 Location".into(),
                    })?;
                return Ok(Response::Redirect(location));
            }
            other => {
                return Err(FetchError::Http {
                    status: other,
                    reason: String::new(),
                })
            }
        }

        let chunked = headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
        });
        let length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok());

        // 长度已知时先按声明值挡一道，避免对着一个声称 10 GB 的响应读到 OOM
        if let Some(declared) = length {
            if declared > limit {
                return Err(FetchError::TooLarge { limit });
            }
        }

        let mut body = Vec::new();
        if chunked {
            read_chunked(&mut reader, &mut body, limit)?;
        } else if let Some(declared) = length {
            body.reserve(declared.min(limit));
            let mut taken = reader.take(declared as u64);
            taken
                .read_to_end(&mut body)
                .map_err(|e| FetchError::Io(format!("读响应体失败：{e}")))?;
        } else {
            reader
                .take(limit as u64 + 1)
                .read_to_end(&mut body)
                .map_err(|e| FetchError::Io(format!("读响应体失败：{e}")))?;
            if body.len() > limit {
                return Err(FetchError::TooLarge { limit });
            }
        }
        Ok(Response::Body(body))
    }

    fn read_headers<R: BufRead>(
        reader: &mut R,
    ) -> Result<(u16, Vec<(String, String)>), FetchError> {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| FetchError::Io(format!("读状态行失败：{e}")))?;
        let status = line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| FetchError::Io(format!("状态行无效：{line:?}")))?;
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            let read = reader
                .read_line(&mut header)
                .map_err(|e| FetchError::Io(format!("读响应头失败：{e}")))?;
            if read == 0 || header == "\r\n" || header == "\n" {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                headers.push((name.trim().to_string(), value.trim().to_string()));
            }
            if headers.len() > 200 {
                return Err(FetchError::Io("响应头过多".into()));
            }
        }
        Ok((status, headers))
    }

    fn read_chunked<R: BufRead>(
        reader: &mut R,
        body: &mut Vec<u8>,
        limit: usize,
    ) -> Result<(), FetchError> {
        loop {
            let mut size_line = String::new();
            reader
                .read_line(&mut size_line)
                .map_err(|e| FetchError::Io(format!("读分块长度失败：{e}")))?;
            let size_text = size_line.trim();
            if size_text.is_empty() {
                continue;
            }
            let size = usize::from_str_radix(size_text, 16)
                .map_err(|e| FetchError::Io(format!("分块长度 \"{size_text}\" 无效：{e}")))?;
            if size == 0 {
                loop {
                    let mut trailer = String::new();
                    let read = reader.read_line(&mut trailer).unwrap_or(0);
                    if read == 0 || trailer == "\r\n" || trailer == "\n" {
                        break;
                    }
                }
                return Ok(());
            }
            if body.len() + size > limit {
                return Err(FetchError::TooLarge { limit });
            }
            let mut chunk = vec![0u8; size];
            reader
                .read_exact(&mut chunk)
                .map_err(|e| FetchError::Io(format!("读分块失败：{e}")))?;
            body.extend_from_slice(&chunk);
            let mut crlf = [0u8; 2];
            let _ = reader.read_exact(&mut crlf);
        }
    }
}

#[cfg(windows)]
mod winhttp {
    use std::ffi::c_void;

    use windows::core::PCWSTR;
    use windows::Win32::Networking::WinHttp::{
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
        WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
        WinHttpSendRequest, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
        WINHTTP_OPEN_REQUEST_FLAGS, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
    };

    use super::FetchError;

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// WinHTTP 的句柄是裸指针（HINTERNET 在本 crate 的绑定里没有独立类型），
    /// 所有调用返回 null 表示失败。因此这里不能像其它 Windows API 那样
    /// 期待 Result —— 每个 HANDLE 都要显式判 null。
    struct Handle(*mut c_void);

    impl Handle {
        fn check(self, what: &str) -> Result<*mut c_void, FetchError> {
            if self.0.is_null() {
                Err(FetchError::Io(format!("{what} 失败（句柄为空）")))
            } else {
                Ok(self.0)
            }
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = WinHttpCloseHandle(self.0);
                }
            }
        }
    }

    /// WinHTTP 走系统 TLS 栈，因此不需要在本仓库里放任何证书/加密库。
    pub fn get(rest: &str, limit: usize) -> Result<Vec<u8>, FetchError> {
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse::<u16>().unwrap_or(443)),
            None => (authority, 443u16),
        };

        unsafe {
            let agent_w = wide("SideLens-update/1");
            let agent = Handle(WinHttpOpen(
                PCWSTR(agent_w.as_ptr()),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ))
            .check("WinHttpOpen")?;

            let host_w = wide(host);
            let connect = Handle(WinHttpConnect(agent, PCWSTR(host_w.as_ptr()), port, 0))
                .check("WinHttpConnect")?;

            let verb_w = wide("GET");
            let path_w = wide(path);
            let request = Handle(WinHttpOpenRequest(
                connect,
                PCWSTR(verb_w.as_ptr()),
                PCWSTR(path_w.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                // 注意：这里固定按 TLS 请求。http:// 走的不是本模块
                // （fetch.rs 里 http:// 交给纯 std 的 http 模块），
                // 所以 https 模块拼上 WINHTTP_FLAG_SECURE 是唯一正确的写法。
                WINHTTP_OPEN_REQUEST_FLAGS(WINHTTP_FLAG_SECURE.0),
            ))
            .check("WinHttpOpenRequest")?;

            WinHttpSendRequest(request, None, None, 0, 0, 0)
                .map_err(|e| FetchError::Io(format!("WinHttpSendRequest 失败：{e}")))?;
            WinHttpReceiveResponse(request, std::ptr::null_mut())
                .map_err(|e| FetchError::Io(format!("WinHttpReceiveResponse 失败：{e}")))?;

            let mut status = 0u32;
            let mut size = std::mem::size_of::<u32>() as u32;
            let _ = WinHttpQueryHeaders(
                request,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut status as *mut u32 as *mut c_void),
                &mut size,
                std::ptr::null_mut(),
            );
            if !(200..=299).contains(&status) {
                return Err(FetchError::Http {
                    status: status as u16,
                    reason: String::new(),
                });
            }

            let mut body = Vec::new();
            loop {
                let mut available = 0u32;
                if WinHttpQueryDataAvailable(request, &mut available).is_err() || available == 0 {
                    break;
                }
                if body.len() + available as usize > limit {
                    return Err(FetchError::TooLarge { limit });
                }
                let mut chunk = vec![0u8; available as usize];
                let mut read = 0u32;
                if WinHttpReadData(
                    request,
                    chunk.as_mut_ptr() as *mut c_void,
                    available,
                    &mut read,
                )
                .is_err()
                    || read == 0
                {
                    break;
                }
                chunk.truncate(read as usize);
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 相对地址按清单所在目录解析() {
        assert_eq!(
            resolve("sidelensd.exe", Some("http://host/build/manifest.json")),
            "http://host/build/sidelensd.exe"
        );
        assert_eq!(
            resolve("https://a/b", Some("http://host/manifest.json")),
            "https://a/b"
        );
        // Windows 盘符路径也算绝对地址
        assert_eq!(
            resolve(r"D:\x\y.exe", Some(r"C:\other\manifest.json")),
            r"D:\x\y.exe"
        );
    }

    #[test]
    fn file_协议读到本地文件() {
        let dir = std::env::temp_dir().join(format!("sidelens-fetch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, b"hello").unwrap();
        let url = format!("file:///{}", path.display().to_string().replace('\\', "/"));
        assert_eq!(fetch(&url, None).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 裸路径也能读() {
        let dir = std::env::temp_dir().join(format!("sidelens-fetch-raw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("b.txt");
        std::fs::write(&path, b"raw").unwrap();
        assert_eq!(fetch(&path.display().to_string(), None).unwrap(), b"raw");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 读不到时错误信息带路径() {
        let error = fetch("/definitely/not/here.json", None).unwrap_err();
        assert!(error.to_string().contains("not/here.json"), "{error}");
    }

    #[test]
    fn 未知协议直接拒绝() {
        let error = fetch("ftp://host/x", None).unwrap_err();
        assert!(matches!(error, FetchError::Unsupported(_)), "{error:?}");
    }

    #[test]
    fn 前导斜杠只在盘符路径时才剥掉() {
        assert_eq!(strip_file_prefix("/C:/x"), "C:/x");
        assert_eq!(strip_file_prefix("/tmp/x"), "/tmp/x");
    }
}
