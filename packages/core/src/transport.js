/**
 * 传输层（demo 版：WebSocket over TCP）
 *
 * 为什么 demo 不用 QUIC：
 *   QUIC 需要 QUIC 库 + UDP 打洞 + 证书，demo 阶段这些会淹没"能不能跑通"这个核心问题。
 *   但接口按 QUIC 的语义设计（帧 = 不可靠数据报的心态），将来换成真 QUIC 时上层不用改。
 *
 * 生产版差异（已在本文件里用 TODO 标出）：
 *   - 传输换成 quinn（Rust）或 node:quic
 *   - 增加 NACK / FEC
 *   - 增加拥塞控制与码率自适应
 */

import { WebSocketServer, WebSocket } from "ws";
import { encodeFrame, FrameDecoder, FrameType, PROTOCOL_VERSION, TYPE_NAMES } from "@sidelens/proto";

export const DEFAULT_PORT = 27100;

/** 服务端：手机推流，电脑拉流；同一台机器上多个客户端靠 clientId 区分。 */
export class SideLensServer {
  constructor({ port = DEFAULT_PORT, onFrame, onClient } = {}) {
    this.port = port;
    this.onFrame = onFrame ?? (() => {});
    this.onClient = onClient ?? (() => {});
    /** @type {Map<string, WebSocket>} */
    this.clients = new Map();
    this.wss = null;
    this.stats = { accepted: 0, framesIn: 0, bytesIn: 0 };
  }

  /**
   * 监听；端口被占用时自动向上试最多 20 个，避免"(EADDRINUSE)"这类环境问题
   * 把第一次跑 demo 的人挡在门外。
   */
  async listen() {
    let lastErr;
    for (let offset = 0; offset < 20; offset++) {
      const port = this.port + offset;
      const wss = new WebSocketServer({ port, perMessageDeflate: false });
      try {
        await new Promise((resolve, reject) => {
          wss.once("listening", resolve);
          wss.once("error", reject);
        });
        this.wss = wss;
        this.port = port;
        wss.on("connection", (ws, req) => this.#handle(ws, req));
        return port;
      } catch (err) {
        lastErr = err;
        wss.close?.();
        if (err?.code !== "EADDRINUSE") throw err;
      }
    }
    throw lastErr ?? new Error("找不到可用端口");
  }

  #handle(ws, req) {
    const id = `${req.socket.remoteAddress}:${req.socket.remotePort}`;
    this.clients.set(id, ws);
    this.stats.accepted++;

    const decoder = new FrameDecoder();
    ws.binaryType = "nodebuffer";

    ws.on("message", (raw) => {
      const buf = Buffer.isBuffer(raw) ? raw : Buffer.from(raw);
      this.stats.bytesIn += buf.length;
      for (const frame of decoder.push(buf)) {
        this.stats.framesIn++;
        if (frame.type === FrameType.HELLO) {
          this.onClient({ id, hello: frame.json });
          ws.send(encodeFrame({
            type: FrameType.HELLO,
            payload: { v: PROTOCOL_VERSION, role: "server", id },
          }));
          continue;
        }
        this.onFrame({ id, frame });
      }
    });

    const bye = () => {
      this.clients.delete(id);
      this.onClient({ id, bye: true });
    };
    ws.on("close", bye);
    ws.on("error", bye);
  }

  broadcast(buf) {
    for (const ws of this.clients.values()) {
      if (ws.readyState === WebSocket.OPEN) ws.send(buf);
    }
  }

  async close() {
    for (const ws of this.clients.values()) ws.close();
    this.clients.clear();
    if (this.wss) await new Promise((r) => this.wss.close(r));
  }
}

/** 客户端：连上服务端，收帧；也可以主动发帧（用于回传关键帧请求 / 统计）。 */
export class SideLensClient {
  constructor({ url, onFrame, onOpen, onClose } = {}) {
    this.url = url;
    this.onFrame = onFrame ?? (() => {});
    this.onOpen = onOpen ?? (() => {});
    this.onClose = onClose ?? (() => {});
    this.ws = null;
    this.decoder = new FrameDecoder();
    this.seqByStream = new Map();
  }

  async connect() {
    this.ws = new WebSocket(this.url, { perMessageDeflate: false });
    this.ws.binaryType = "nodebuffer";

    await new Promise((resolve, reject) => {
      this.ws.once("open", resolve);
      this.ws.once("error", reject);
    });

    this.ws.on("message", (raw) => {
      const buf = Buffer.isBuffer(raw) ? raw : Buffer.from(raw);
      for (const f of this.decoder.push(buf)) this.onFrame(f);
    });
    this.ws.on("close", () => this.onClose());
    this.onOpen(this);
    return this;
  }

  nextSeq(streamId) {
    const n = (this.seqByStream.get(streamId) ?? 0) + 1;
    this.seqByStream.set(streamId, n);
    return n;
  }

  send({ type, flags, streamId = 0, ptsUs, payload }) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return false;
    this.ws.send(encodeFrame({
      type, flags, streamId,
      seq: this.nextSeq(streamId),
      ptsUs, payload,
    }));
    return true;
  }

  close() {
    this.ws?.close();
  }
}

export { FrameType, TYPE_NAMES };
