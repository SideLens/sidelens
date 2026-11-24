//! WebSocket 服务端
//!
//! 一个 `WsServer` 可以同时接多个客户端（多台手机推流）。
//! 每个连接独立维护一个 `FrameDecoder` —— 因为 TCP 分片边界是每连接独立的。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::{accept_async, tungstenite::Message, WebSocketStream};
use tracing::{debug, info, warn};

use sidelens_proto::{encode_frame_owned, FrameDecoder, FrameMeta, FrameSpec, FrameType};

/// 从某个客户端收到的一帧
#[derive(Debug, Clone)]
pub struct InboundFrame {
    /// 客户端标识（`ip:port`）
    pub client_id: String,
    pub ty: FrameType,
    pub flags: u8,
    pub stream_id: u16,
    pub seq: u32,
    pub pts_us: u64,
    pub send_ts_us: u64,
    pub payload: Vec<u8>,
}

impl InboundFrame {
    fn from_meta(client_id: &str, meta: FrameMeta, payload: &[u8]) -> Self {
        Self {
            client_id: client_id.to_owned(),
            ty: meta.ty,
            flags: meta.flags,
            stream_id: meta.stream_id,
            seq: meta.seq,
            pts_us: meta.pts_us,
            send_ts_us: meta.send_ts_us,
            payload: payload.to_vec(),
        }
    }

    pub fn is_keyframe(&self) -> bool {
        self.flags & sidelens_proto::FLAG_KEYFRAME != 0
    }

    pub fn json(&self) -> Option<serde_json::Value> {
        if !self.ty.is_control() || self.payload.is_empty() {
            return None;
        }
        serde_json::from_slice(&self.payload).ok()
    }
}

/// 服务端统计
#[derive(Debug, Default)]
pub struct ServerStats {
    pub accepted: AtomicU64,
    pub frames_in: AtomicU64,
    pub bytes_in: AtomicU64,
    pub resync_skipped: AtomicU64,
}

impl ServerStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            accepted: self.accepted.load(Ordering::Relaxed),
            frames_in: self.frames_in.load(Ordering::Relaxed),
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            resync_skipped: self.resync_skipped.load(Ordering::Relaxed),
        }
    }
}

/// 统计快照
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub accepted: u64,
    pub frames_in: u64,
    pub bytes_in: u64,
    pub resync_skipped: u64,
}

/// WebSocket 服务端
///
/// # 用法
///
/// ```no_run
/// # use sidelens_transport::WsServer;
/// # async fn run() -> std::io::Result<()> {
/// let server = WsServer::bind(27100).await?;
/// let mut rx = server.subscribe();
/// tokio::spawn(async move {
///     while let Ok(frame) = rx.recv().await {
///         println!("{} {} 字节", frame.ty, frame.payload.len());
///     }
/// });
/// server.serve().await
/// # }
/// ```
#[derive(Debug)]
pub struct WsServer {
    listener: TcpListener,
    port: u16,
    tx: broadcast::Sender<InboundFrame>,
    /// 每个在线客户端的发送端（用于回发关键帧请求）
    peers: Arc<Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>,
    pub stats: Arc<ServerStats>,
}

impl WsServer {
    /// 绑定端口。端口被占用时自动向上试最多 20 个，与 JS 版行为一致。
    ///
    /// 传 `0` 表示交给操作系统分配（测试用），实际端口用 [`WsServer::port`] 读取。
    pub async fn bind(port: u16) -> std::io::Result<Self> {
        let mut last_err = None;
        for offset in 0..20u16 {
            let try_port = port.saturating_add(offset);
            match TcpListener::bind(("0.0.0.0", try_port)).await {
                Ok(listener) => {
                    // 必须用 local_addr：传 0 让 OS 分配端口时，
                    // 实际端口不是 try_port（0），只能从这里读出来。
                    let actual_port = listener.local_addr()?.port();
                    let (tx, _) = broadcast::channel(1024);
                    return Ok(Self {
                        listener,
                        port: actual_port,
                        tx,
                        peers: Arc::new(Mutex::new(HashMap::new())),
                        stats: Arc::new(ServerStats::default()),
                    });
                }
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::AddrInUse {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::AddrInUse, "找不到可用端口")
        }))
    }

    /// 实际监听的端口（可能因占用而上移）
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 订阅收到的帧
    pub fn subscribe(&self) -> broadcast::Receiver<InboundFrame> {
        self.tx.subscribe()
    }

    /// 向指定客户端发送一帧（例如 KEYFRAME_REQ）
    pub async fn send_to(&self, client_id: &str, bytes: Vec<u8>) -> bool {
        let peers = self.peers.lock().await;
        peers
            .get(client_id)
            .map(|tx| tx.send(bytes).is_ok())
            .unwrap_or(false)
    }

    /// 向所有客户端广播
    pub async fn broadcast(&self, bytes: Vec<u8>) {
        let peers = self.peers.lock().await;
        for tx in peers.values() {
            let _ = tx.send(bytes.clone());
        }
    }

    /// 接受连接直到进程退出
    pub async fn serve(&self) -> std::io::Result<()> {
        // 保留 io::Result：调用方通常只想打日志，不需要错误分类
        self.serve_with(|_| async {}).await
    }

    /// 接受连接；每个新连接回调一次（用于打印日志、登记设备等）
    pub async fn serve_with<F, Fut>(&self, on_connect: F) -> std::io::Result<()>
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let on_connect = Arc::new(on_connect);
        loop {
            let (stream, addr) = self.listener.accept().await?;
            let client_id = format!("{addr}");
            info!(%client_id, "客户端接入");

            let tx = self.tx.clone();
            let peers = self.peers.clone();
            let stats = self.stats.clone();
            let cb = on_connect.clone();
            let id = client_id.clone();

            tokio::spawn(async move {
                cb(id.clone()).await;
                if let Err(e) = handle_conn(stream, addr, tx, peers, stats).await {
                    warn!(%id, error = %e, "连接结束");
                }
                info!(%id, "客户端断开");
            });
        }
    }
}

async fn handle_conn(
    stream: TcpStream,
    addr: SocketAddr,
    tx: broadcast::Sender<InboundFrame>,
    peers: Arc<Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>,
    stats: Arc<ServerStats>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws: WebSocketStream<TcpStream> = accept_async(stream).await?;
    let client_id = format!("{addr}");

    stats.accepted.fetch_add(1, Ordering::Relaxed);

    let (mut sink, mut source) = ws.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    peers.lock().await.insert(client_id.clone(), out_tx);

    // 写任务：从 out_rx 取字节发出去
    let writer = tokio::spawn(async move {
        while let Some(bytes) = out_rx.recv().await {
            if sink.send(Message::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
    });

    let mut decoder = FrameDecoder::new();

    while let Some(msg) = source.next().await {
        let msg = msg?;
        match msg {
            Message::Binary(data) => {
                stats
                    .bytes_in
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                decoder.push(&data);
                stats
                    .resync_skipped
                    .store(decoder.resync_skipped, Ordering::Relaxed);

                for meta in decoder.frames() {
                    stats.frames_in.fetch_add(1, Ordering::Relaxed);
                    let frame = InboundFrame::from_meta(&client_id, *meta, decoder.payload(*meta));
                    // 没人订阅时 send 会失败，这是正常的（接收端还没起来）
                    let _ = tx.send(frame);
                }
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => break,
            Message::Text(_) | Message::Frame(_) => {
                debug!("忽略非二进制消息");
            }
        }
    }

    writer.abort();
    peers.lock().await.remove(&client_id);
    Ok(())
}

/// 便捷：构造并编码一个控制帧
pub fn control_frame(ty: FrameType, json: &serde_json::Value) -> Vec<u8> {
    let payload = serde_json::to_vec(json).unwrap_or_default();
    encode_frame_owned(&FrameSpec::new(ty, &payload)).expect("控制帧编码不会失败")
}
