/**
 * SideLens 发送端（手机侧模拟）
 *
 * demo 里没有真摄像头，用 FakeCamera 合成帧；
 * 但**其他一切都和真实发送端一致**：
 *   - 单调时钟打 PTS
 *   - 关键帧/非关键帧标记
 *   - 主动广播让电脑能发现
 *   - 响应 KEYFRAME_REQ
 *
 * 换成真摄像头时只需替换 `capture()` 这一处。
 */

import { SideLensClient, Beacon } from "@sidelens/core";
import { FrameType, FLAG_KEYFRAME, nowUs } from "@sidelens/proto";
import { FakeCamera } from "@sidelens/fake-camera";

export class Sender {
  constructor({ url, name = "SideLens Device", fps = 30, width = 320, height = 240, advertise = true } = {}) {
    this.url = url;
    this.name = name;
    this.fps = fps;
    this.camera = new FakeCamera({ width, height, fps });
    this.client = null;
    this.beacon = null;
    this.timer = null;
    this.forceKeyframe = true;
    this.stats = { sent: 0, bytes: 0, keyframes: 0, paused: 0 };
    this.frameIndex = 0;
    this.startTs = null;
  }

  async start() {
    this.client = new SideLensClient({
      url: this.url,
      onOpen: (c) => c.send({ type: FrameType.HELLO, payload: { v: 0, role: "sender", name: this.name } }),
      onFrame: (f) => {
        if (f.type === FrameType.KEYFRAME_REQ) {
          this.forceKeyframe = true;
          this.stats.keyframes++;
        }
      },
    });
    await this.client.connect();

    if (this.beacon !== false) {
      this.beacon = new Beacon({ name: this.name, meta: { fps: this.fps } }).start();
    }

    this.startTs = nowUs();
    this.#loop();
    return this;
  }

  #loop() {
    const intervalMs = 1000 / this.fps;
    const tick = () => {
      const ptsUs = nowUs();
      const frame = this.camera.capture(ptsUs);
      const keyframe = this.forceKeyframe || this.stats.sent % (this.fps * 2) === 0;
      this.forceKeyframe = false;

      this.client.send({
        type: FrameType.VIDEO,
        flags: keyframe ? FLAG_KEYFRAME : 0,
        streamId: 0,
        ptsUs,
        payload: frame.rgba,
      });

      this.stats.sent++;
      this.stats.bytes += frame.rgba.length;
      if (keyframe) this.stats.keyframes++;
    };

    this.timer = setInterval(tick, intervalMs);
  }

  /** 模拟一段音频（Opus 假设 20ms 一帧），用于验证音画同步路径 */
  startFakeAudio({ sampleRate = 48000, frameMs = 20 } = {}) {
    const bytesPerFrame = (sampleRate * frameMs) / 1000 * 2; // 16-bit mono
    this.audioTimer = setInterval(() => {
      this.client.send({
        type: FrameType.AUDIO,
        streamId: 1,
        ptsUs: nowUs(),
        payload: Buffer.alloc(bytesPerFrame),
      });
    }, frameMs);
  }

  stop() {
    clearInterval(this.timer);
    clearInterval(this.audioTimer);
    this.beacon?.stop();
    try { this.client?.send({ type: FrameType.BYE, payload: { reason: "user-stop" } }); } catch {}
    this.client?.close();
  }
}
