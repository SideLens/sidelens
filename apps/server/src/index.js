/**
 * SideLens 电脑端常驻服务
 *
 * 职责（对应架构里的「接收端」）：
 *   1. 监听手机推流（demo 用 WebSocket，生产换 QUIC）
 *   2. 抖动缓冲 + 音画同步
 *   3. 统计延迟 / 码率 / 丢帧
 *   4. 将来：把帧喂给虚拟摄像头（Windows = MFCreateVirtualCamera）
 *
 * 注意：本文件**不做**虚拟设备注入。那是平台原生层的事，
 * 在 demo 里用 stats 证明"我们这一侧的延迟足够低"就达到目的了。
 */

import { SideLensServer, JitterBuffer, RttEstimator, Beacon, Discoverer } from "@sidelens/core";
import { FrameType, encodeFrame, nowUs } from "@sidelens/proto";

export class SideLensDaemon {
  constructor({ port = 27100, advertise = true, deviceName = "SideLens Host" } = {}) {
    this.port = port;
    this.deviceName = deviceName;
    this.server = null;
    this.beacon = null;
    this.discoverer = null;

    /** @type {Map<string, {jb: JitterBuffer, rtt: RttEstimator, lastSeq: number, dropped: number, received: number}>} */
    this.sessions = new Map();

    this.stats = {
      startedAt: null,
      clients: 0,
      frames: 0,          // 视频帧
      audioFrames: 0,     // 音频帧
      bytes: 0,
      latency: [],        // 视频单向延迟样本（ms）
      playable: [],       // 到达后到可播放的耗时（ms）
    };

    this.onFrame = null;   // (sessionId, frame) => void，供预览端挂钩
  }

  async start() {
    this.server = new SideLensServer({
      port: this.port,
      onFrame: (evt) => this.#onFrame(evt),
      onClient: (evt) => this.#onClient(evt),
    });
    const actual = await this.server.listen();
    this.stats.startedAt = Date.now();

    if (this.discoverer) { /* 已启动 */ }
    this.discoverer = new Discoverer();
    await this.discoverer.start();

    return { port: actual };
  }

  #onClient(evt) {
    if (evt.hello) {
      this.stats.clients++;
      this.sessions.set(evt.id, {
        jb: new JitterBuffer({ targetMs: 20, maxMs: 200 }),
        rtt: new RttEstimator(),
        received: 0,
        dropped: 0,
      });
    }
    if (evt.bye) this.sessions.delete(evt.id);
  }

  #onFrame({ id, frame }) {
    if (frame.type === FrameType.AUDIO) {
      this.stats.audioFrames++;
      this.stats.bytes += frame.payload.length;
      return;
    }

    if (frame.type === FrameType.VIDEO) {
      this.stats.frames++;
      this.stats.bytes += frame.payload.length;

      // 单向延迟：同机测试时时钟一致，直接相减即真实值
      const latencyMs = Number(nowUs() - frame.sendTsUs) / 1000;
      this.stats.latency.push(latencyMs);
      if (this.stats.latency.length > 2000) this.stats.latency.shift();

      const s = this.sessions.get(id);
      if (s) {
        s.received++;
        s.rtt.add(latencyMs);
        const { needsKeyframe } = s.jb.push(frame);
        if (needsKeyframe) {
          // 告诉发送端：我这边断链了，下一帧请给关键帧
          this.server.clients.get(id)?.send(
            encodeFrame({ type: FrameType.KEYFRAME_REQ, streamId: 0, payload: { reason: "jitter-overflow" } })
          );
        }
        // 记录"到达 → 可播放"的耗时
        const f = s.jb.pop();
        if (f) {
          const playableMs = Number(nowUs() - frame.sendTsUs) / 1000;
          this.stats.playable.push(playableMs);
          if (this.stats.playable.length > 2000) this.stats.playable.shift();
          this.onFrame?.(id, f);
        }
      }
    }
  }

  summary() {
    const pct = (arr, p) => {
      if (!arr.length) return 0;
      const s = [...arr].sort((a, b) => a - b);
      const i = Math.min(s.length - 1, Math.floor((s.length - 1) * p));
      return s[i];
    };
    const l = this.stats.latency;
    const p = this.stats.playable;
    return {
      uptimeMs: this.stats.startedAt ? Date.now() - this.stats.startedAt : 0,
      clients: this.stats.clients,
      frames: this.stats.frames,
      audioFrames: this.stats.audioFrames,
      bytes: this.stats.bytes,
      latency: {
        samples: l.length,
        p50: +pct(l, 0.5).toFixed(2),
        p95: +pct(l, 0.95).toFixed(2),
        p99: +pct(l, 0.99).toFixed(2),
        max: +Math.max(0, ...l).toFixed(2),
      },
      playable: {
        samples: p.length,
        p50: +pct(p, 0.5).toFixed(2),
        p95: +pct(p, 0.95).toFixed(2),
        p99: +pct(p, 0.99).toFixed(2),
      },
    };
  }

  async stop() {
    this.discoverer?.stop();
    this.beacon?.stop();
    await this.server?.close();
  }
}
