/**
 * 时钟与 RTT 估计
 *
 * 音画同步的正确做法：**以音频为主时钟**，视频按 PTS 对齐。
 * 原因：音频一断就听得出（爆音、卡顿），视频掉一帧人眼几乎无感。
 */

/** 发送端 → 接收端偏移估计（单向延迟的近似）。取滑动窗口内最小 RTT 样本。 */
export class RttEstimator {
  #samples = [];

  constructor(windowSize = 32) {
    this.windowSize = windowSize;
  }

  add(rttMs) {
    if (!Number.isFinite(rttMs) || rttMs < 0) return;
    this.#samples.push(rttMs);
    if (this.#samples.length > this.windowSize) this.#samples.shift();
  }

  get min() {
    return this.#samples.length ? Math.min(...this.#samples) : 0;
  }

  get median() {
    if (!this.#samples.length) return 0;
    const s = [...this.#samples].sort((a, b) => a - b);
    const mid = s.length >> 1;
    return s.length % 2 ? s[mid] : (s[mid - 1] + s[mid]) / 2;
  }

  get smoothed() {
    return this.median;
  }
}

/**
 * 音画同步器：音频立即播放，视频按"音频已播到哪儿"对齐。
 * 视频 PTS 落后于音频超过 maxDriftMs 就丢帧（追不上就别追了）。
 */
export class AvSync {
  constructor({ maxDriftMs = 40, dropThresholdMs = 120 } = {}) {
    this.maxDriftMs = maxDriftMs;
    this.dropThresholdMs = dropThresholdMs;
    this.audioClockUs = null;
    this.stats = { droppedLate: 0, droppedEarly: 0, synced: 0 };
  }

  /** 音频帧出队时调用，推进主时钟。 */
  onAudio(frame) {
    this.audioClockUs = BigInt(frame.ptsUs) + BigInt(frame.durationUs ?? 0);
  }

  /**
   * 视频帧是否应当播放。
   * @returns {"play"|"drop"|"wait"}
   */
  judge(videoFrame) {
    if (this.audioClockUs === null) return "play"; // 纯视频模式：不做对齐
    const driftUs = Number(videoFrame.ptsUs - this.audioClockUs);
    const driftMs = driftUs / 1000;

    if (driftMs > this.maxDriftMs) return "wait";          // 视频太超前，等一会儿
    if (driftMs < -this.dropThresholdMs) {                  // 视频落后太多，丢了
      this.stats.droppedLate++;
      return "drop";
    }
    this.stats.synced++;
    return "play";
  }

  reset() {
    this.audioClockUs = null;
  }
}
