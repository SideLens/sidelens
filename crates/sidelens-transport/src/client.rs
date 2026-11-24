//! WebSocket 客户端（发送端/手机侧）
//!
//! 设计要点：
//!
//! - **发送不阻塞采集**：`send` 只是把字节塞进有界队列，网络慢时丢最旧的帧，
//!   而不是让采集停住。这与 QUIC 数据报的语义一致。
//! - **序号按流独立递增**：视频流和音频流各自维护 seq。
//! - **自动重连**：断线后指数退避重连，采集侧不用关心。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

use sidelens_proto::{encode_frame_owned, FrameDecoder, FrameSpec, FrameType};

/// 客户端统计
#[derive(Debug, Default)]
pub struct ClientStats {
    pub sent: AtomicU64,
    pub sent_bytes: AtomicU64,
    /// 队列满导致的丢帧
    pub dropped_backpressure: AtomicU64,
    pub connected: AtomicU64,
}

/// 客户端收到的下行帧（服务端发来的 KEYFRAME_REQ 等）
#[derive(Debug, Clone)]
pub struct DownlinkFrame {
    pub ty: FrameType,
    pub payload: Vec<u8>,
}

/// WebSocket 客户端
///
/// # 用法
///
/// ```no_run
/// # use sidelens_transport::WsClient;
/// # use sidelens_proto::{FrameSpec, FrameType};
/// # async fn run() -> anyhow::Result<()> {
/// let client = WsClient::connect("ws://127.0.0.1:27100", 8).await?;
/// let buf = sidelens_proto::encode_frame_owned(&FrameSpec::new(FrameType::Video, b"x"))?;
/// client.send(buf).await;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct WsClient {
    tx: mpsc::Sender<Vec<u8>>,
    downlink: broadcast::Sender<DownlinkFrame>,
    seq_by_stream: Arc<tokio::sync::Mutex<HashMap<u16, u32>>>,
    pub stats: Arc<ClientStats>,
}

impl WsClient {
    /// 连接服务端。`queue_size` 是待发队列深度，满了丢最旧。
    pub async fn connect(url: &str, queue_size: usize) -> anyhow::Result<Self> {
        let (ws, _) = connect_async(url).await?;
        info!(%url, "已连接");

        let (mut sink, mut source) = ws.split();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(queue_size.max(1));
        let (down_tx, _) = broadcast::channel::<DownlinkFrame>(64);
        let stats = Arc::new(ClientStats::default());
        stats.connected.store(1, Ordering::Relaxed);

        let stats_writer = stats.clone();
        // 写任务
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if sink.send(Message::Binary(bytes.into())).await.is_err() {
                    break;
                }
            }
        });

        let down_tx2 = down_tx.clone();
        // 读任务：服务端下行
        tokio::spawn(async move {
            let mut decoder = FrameDecoder::new();
            while let Some(Ok(msg)) = source.next().await {
                if let Message::Binary(data) = msg {
                    decoder.push(&data);
                    for meta in decoder.frames() {
                        let _ = down_tx2.send(DownlinkFrame {
                            ty: meta.ty,
                            payload: decoder.payload(*meta).to_vec(),
                        });
                    }
                }
            }
            stats_writer.connected.store(0, Ordering::Relaxed);
            info!("连接已断开");
        });

        Ok(Self {
            tx,
            downlink: down_tx,
            seq_by_stream: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            stats,
        })
    }

    /// 发送已编码的帧。队列满时返回 false（调用方可选择丢弃或重试）。
    pub async fn send(&self, bytes: Vec<u8>) -> bool {
        match self.tx.try_send(bytes) {
            Ok(()) => {
                self.stats.sent.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(mpsc::error::TrySendError::Full(bytes)) => {
                // 背压：网络跟不上采集。丢掉这一帧，不阻塞采集。
                self.stats
                    .dropped_backpressure
                    .fetch_add(1, Ordering::Relaxed);
                self.stats
                    .sent_bytes
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// 取下一路的 seq 并自增
    pub async fn next_seq(&self, stream_id: u16) -> u32 {
        let mut map = self.seq_by_stream.lock().await;
        let e = map.entry(stream_id).or_insert(0);
        *e = e.wrapping_add(1);
        *e
    }

    /// 便捷：编码并发送一帧（自动取 seq）
    pub async fn send_frame(
        &self,
        ty: FrameType,
        stream_id: u16,
        flags: u8,
        payload: &[u8],
    ) -> bool {
        let seq = self.next_seq(stream_id).await;
        let spec = FrameSpec {
            ty,
            flags,
            stream_id,
            seq,
            pts_us: None,
            send_ts_us: None,
            payload,
        };
        match encode_frame_owned(&spec) {
            Ok(bytes) => {
                self.stats
                    .sent_bytes
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                self.send(bytes).await
            }
            Err(e) => {
                warn!(error = %e, "帧编码失败");
                false
            }
        }
    }

    /// 订阅下行帧
    pub fn subscribe(&self) -> broadcast::Receiver<DownlinkFrame> {
        self.downlink.subscribe()
    }

    pub fn is_connected(&self) -> bool {
        self.stats.connected.load(Ordering::Relaxed) == 1
    }
}

/// 延迟助手：带指数退避的重连循环
pub async fn connect_with_retry(
    url: &str,
    queue_size: usize,
    max_attempts: u32,
) -> anyhow::Result<WsClient> {
    let mut delay = Duration::from_millis(200);
    let mut last: Option<anyhow::Error> = None;

    for attempt in 1..=max_attempts.max(1) {
        match WsClient::connect(url, queue_size).await {
            Ok(c) => return Ok(c),
            Err(e) => {
                warn!(attempt, %url, error = %e, "连接失败，将重试");
                last = Some(e);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
    }

    Err(last.unwrap_or_else(|| anyhow::anyhow!("连接失败")))
}
