//! 更新链路的端到端测试：真起一个本地 HTTP 服务，走完
//! 「取清单 → 下载 → 校验 → 落地 → 再取清单确认已最新」。
//!
//! # 为什么必须测 HTTP 而不是只测 file://
//!
//! file:// 走 std::fs::read，HTTP 走我们手写的 update::fetch::http。
//! 两条代码路径完全不同，而真实发版走的一定是 HTTP。尤其是三种分帧方式：
//!
//! | 方式 | 谁会用 |
//! | --- | --- |
//! | Content-Length | 静态文件服务器（nginx 默认） |
//! | chunked | 反向代理、动态生成、S3 直传 |
//! | 关闭连接定界 | 老旧的 CGI / 部分 CDN 边缘节点 |
//!
//! 只测一种，另外两种就是「没人知道会不会坏」。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sidelensd::update;

/// 响应分帧方式
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    Length,
    Chunked,
    CloseDelimited,
}

/// 一个只服务固定路由表的最小 HTTP/1.1 服务器
struct TestServer {
    port: u16,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    fn start(routes: Vec<(String, Vec<u8>)>, framing: Framing) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定回环端口");
        let port = listener.local_addr().expect("取端口").port();
        listener.set_nonblocking(true).expect("非阻塞 accept");
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let hits_thread = Arc::clone(&hits);
        let stop_thread = Arc::clone(&stop);
        let routes = Arc::new(routes);

        let thread = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        hits_thread.fetch_add(1, Ordering::Relaxed);
                        let routes = Arc::clone(&routes);
                        std::thread::spawn(move || serve(stream, &routes, framing));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            port,
            hits,
            stop,
            thread: Some(thread),
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // 叫醒卡在 accept 的线程
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(mut stream: TcpStream, routes: &[(String, Vec<u8>)], framing: Framing) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut raw = Vec::new();
    let mut buffer = [0u8; 1024];
    // 读到请求头结束（\r\n\r\n）就够 —— 我们只处理 GET
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return,
            Ok(n) => {
                raw.extend_from_slice(&buffer[..n]);
                if raw.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                if raw.len() > 8192 {
                    return;
                }
            }
            Err(_) => return,
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let path = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .trim_start_matches('/')
        .to_string();

    let Some((_, body)) = routes.iter().find(|(name, _)| name == &path) else {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = stream.flush();
        return;
    };

    match framing {
        Framing::Length => {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(body);
        }
        Framing::Chunked => {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            );
            // 故意用 7 字节的不规则块长，覆盖分块边界处理
            for chunk in body.chunks(7) {
                let _ = stream.write_all(format!("{:x}\r\n", chunk.len()).as_bytes());
                let _ = stream.write_all(chunk);
                let _ = stream.write_all(b"\r\n");
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        }
        Framing::CloseDelimited => {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(body);
        }
    }
    let _ = stream.flush();
}

fn scratch(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "sidelens-update-http-{}-{}-{name}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

/// 造一份发布包的路由表 + 落在临时目录里的产品副本（供 file:// 对照）
fn release(
    files: &[(&str, &[u8])],
    version: &str,
) -> (sidelensd::release::Manifest, Vec<(String, Vec<u8>)>) {
    use sidelensd::release::hash::sha256_bytes;
    use sidelensd::release::manifest::{Manifest, ManifestFile, PhoneManifest};
    use sidelensd::release::Version;

    let manifest = Manifest {
        schema: 1,
        product: "SideLens".into(),
        channel: "stable".into(),
        version: Version::parse(version).unwrap(),
        released: "2026-11-26T00:00:00Z".into(),
        min_upgradable_version: Version::new(0, 0, 0),
        files: files
            .iter()
            .map(|(path, bytes)| ManifestFile {
                path: (*path).to_string(),
                size: bytes.len() as u64,
                sha256: sha256_bytes(bytes),
            })
            .collect(),
        phone: PhoneManifest {
            min_supported_version_code: 1,
            latest_version_code: 2,
            latest_version_name: version.into(),
            min_supported_protocol: 1,
        },
        notes: "http e2e".into(),
    };
    let mut routes: Vec<(String, Vec<u8>)> = files
        .iter()
        .map(|(path, bytes)| ((*path).to_string(), bytes.to_vec()))
        .collect();
    routes.push(("manifest.json".into(), manifest.to_json().into_bytes()));
    (manifest, routes)
}

#[test]
fn 三种分帧方式都能取回并解析清单() {
    let (expected, routes) = release(&[("sidelensd.exe", b"binary-payload")], "0.2.0");
    for framing in [Framing::Length, Framing::Chunked, Framing::CloseDelimited] {
        let server = TestServer::start(routes.clone(), framing);
        let url = format!("{}manifest.json", server.base());
        let outcome = update::check(&url, "0.1.0")
            .unwrap_or_else(|e| panic!("{framing:?} 下取清单失败：{e}"));
        assert_eq!(outcome.manifest.version, expected.version, "{framing:?}");
        assert!(outcome.decision.is_update(), "{framing:?} 应判定为有更新");
        assert_eq!(server.hits(), 1, "{framing:?} 应只请求一次");
    }
}

#[test]
fn 走一遍完整更新并在最后确认已最新() {
    let (_, routes) = release(
        &[
            ("sidelensd.exe", b"v2-binary"),
            ("sidelens_dshow.dll", b"v2-dshow-dll"),
        ],
        "0.2.0",
    );
    let server = TestServer::start(routes, Framing::Chunked);
    let base = server.base();
    let manifest_url = format!("{base}manifest.json");

    let dir = scratch("flow");
    let layout = sidelensd::release::Layout::new(dir.join("install"));
    std::fs::create_dir_all(layout.root()).unwrap();
    std::fs::write(layout.exe(), b"v1-binary").unwrap();

    // 1. 检查
    let outcome = update::check(&manifest_url, "0.1.0").expect("应能取到清单");
    assert!(outcome.decision.is_update());

    // 2. 下载 + 校验
    let mut log = Vec::new();
    let staged = update::download(&layout, &outcome, &mut |line| log.push(line.to_string()))
        .expect("下载校验应成功");
    assert_eq!(staged.files.len(), 2);
    assert!(!log.is_empty());

    // 3. 落地
    let summary = update::apply_staged(
        &layout,
        &outcome,
        &staged,
        Some(sidelensd::release::Version::new(0, 1, 0)),
    )
    .expect("落地应成功");
    assert!(!summary.is_empty());
    assert_eq!(std::fs::read(layout.exe()).unwrap(), b"v2-binary");
    assert_eq!(
        std::fs::read(layout.join_relative("sidelens_dshow.dll")).unwrap(),
        b"v2-dshow-dll"
    );

    // 4. 备份里有旧版本，可回滚
    let backup = layout
        .backup_dir(&sidelensd::release::Version::new(0, 1, 0))
        .join(sidelensd::release::layout::exe_name());
    assert_eq!(std::fs::read(&backup).unwrap(), b"v1-binary");

    // 5. 回写记录后再检查，必须结论为「已最新」
    update::refresh_installed(&layout, outcome.manifest.version, "2026-11-26T00:00:00Z")
        .expect("写 installed.json");
    let again = update::check(&manifest_url, "0.2.0").expect("再检查");
    assert!(
        !again.decision.is_update(),
        "装完之后不该还提示有更新：{}",
        again.decision.message()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 服务器返回_404_时给出可读错误而不是空内容() {
    let server = TestServer::start(vec![], Framing::Length);
    let error = update::check(&format!("{}manifest.json", server.base()), "0.1.0").unwrap_err();
    let text = error.to_string();
    assert!(text.contains("404"), "错误信息应包含状态码：{text}");
}

#[test]
fn 产物在传输中被改坏时下载阶段就失败() {
    let (manifest, mut routes) = release(&[("sidelensd.exe", b"correct-bytes")], "0.2.0");
    // 把产物换成等长但不同的内容
    for (name, body) in routes.iter_mut() {
        if name == "sidelensd.exe" {
            *body = b"corrupt-bytez".to_vec();
        }
    }
    assert_eq!(
        b"correct-bytes".len(),
        b"corrupt-bytez".len(),
        "长度必须相同，才能确认是哈希而不是长度挡住的"
    );

    let server = TestServer::start(routes, Framing::Length);
    let dir = scratch("corrupt");
    let layout = sidelensd::release::Layout::new(dir.join("install"));

    let outcome = update::check(&format!("{}manifest.json", server.base()), "0.1.0").unwrap();
    assert_eq!(outcome.manifest.files[0].sha256, manifest.files[0].sha256);
    let error = update::download(&layout, &outcome, &mut |_| {}).unwrap_err();
    assert!(
        matches!(error, update::UpdateError::Corrupt { .. }),
        "应报校验失败：{error:?}"
    );
    // 暂存目录必须被清空，不能留下半套文件
    assert!(!layout.updates_dir().join("0.2.0").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 清单里写了服务器上不存在的文件时指出是哪一个() {
    let (_, mut routes) = release(
        &[("sidelensd.exe", b"x"), ("sidelens_dshow.dll", b"y")],
        "0.2.0",
    );
    routes.retain(|(name, _)| name != "sidelens_dshow.dll");

    let server = TestServer::start(routes, Framing::Length);
    let dir = scratch("missing-file");
    let layout = sidelensd::release::Layout::new(dir.join("install"));
    let outcome = update::check(&format!("{}manifest.json", server.base()), "0.1.0").unwrap();
    let error = update::download(&layout, &outcome, &mut |_| {}).unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("sidelens_dshow.dll") && text.contains("404"),
        "错误信息应同时指出文件与状态码：{text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 服务器不可达时报连接错误而不是挂死() {
    // 绑一个端口再立刻释放，得到"确定没人监听"的端口
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let started = std::time::Instant::now();
    let error =
        update::check(&format!("http://127.0.0.1:{port}/manifest.json"), "0.1.0").unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "不该等到超时才返回"
    );
    assert!(error.to_string().contains("取回失败"), "{error}");
}
