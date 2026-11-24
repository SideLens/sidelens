/**
 * 抖动缓冲（jitter buffer）
 *
 * 目标：用可控的延迟换取平稳的播放节奏。
 * 策略：自适应 —— 正常时缓慢收敛到目标延迟，检测到断帧或乱序时快速扩增。
 *
 * 与消费端（Zoom/Teams）的缓冲不同：这里的缓冲是**我们能控制的那一段**，
 * 应当尽量压到 10–40ms，把预算让给抗抖动能力。
 */

import { FrameType } from "@sidelens/proto";

export class JitterBuffer {
  /**
   * @param {object} opts
   * @param {number} [opts.targetMs=20]   目标延迟
   * @param {number} [opts.maxMs=200]     延迟上限，超过视为"参考帧丢失"，触发关键帧请求
   * @param {number} [opts.shrinkPerSec=2] 每秒允许向下收敛的毫秒数
   */
  constructor({ targetMs = 20, maxMs = 200, shrinkPerSec = 2 } = {}) {
    this.targetMs = targetMs;
    this.maxMs = maxMs;
    this.shrinkPerSec = shrinkPerSec;

    /** @type {Array<{ptsUs: bigint, arrivedAtUs: bigint, frame: object}>} */
    this.queue = [];
    /** 已播放过的最大 seq；只有 <= 它的帧才算真正的迟到帧 */
    this.lastPlayedSeq = null;
    this.lastArrivalUs = null;

    this.stats = {
      received: 0,
      played: 0,
      dropped: 0,
      reordered: 0,
      keyframeRequests: 0,
      maxDepth: 0,
      currentDepthMs: 0,
      avgJitterMs: 0,
    };
    this.#jitterEwma = 0;
  }

  #jitterEwma;

  /**
   * 投递一帧。
   * @returns {{ needsKeyframe: boolean }}
   */
  push(frame) {
    const arrivedAtUs = BigInt(Math.round(performance.now() * 1000));

    if (frame.type !== FrameType.VIDEO && frame.type !== FrameType.AUDIO) {
      return { needsKeyframe: false };
    }

    // 用到达间隔估计网络抖动（RFC 3550 的简化版）
    if (this.lastArrivalUs !== null) {
      const transit = Number(arrivedAtUs - frame.sendTsUs);
      const delta = Math.abs(transit - (Number(this.lastArrivalUs - frame.sendTsUs)));
      if (Number.isFinite(delta) && delta < 1e6) {
        this.#jitterEwma = this.#jitterEwma === 0 ? delta : this.#jitterEwma * 0.9 + delta * 0.1;
        this.stats.avgJitterMs = this.#jitterEwma / 1000;
      }
    }
    this.lastArrivalUs = arrivedAtUs;

    // 只有"已经播放过的那一帧之前"的 seq 才是真迟到。
    // 用 expectedSeq 判断会把正常乱序一并误杀 —— 乱序帧还没播，必须进队列重排。
    if (this.lastPlayedSeq !== null && frame.seq <= this.lastPlayedSeq) {
      this.stats.reordered++;
      return { needsKeyframe: false }; // 迟到帧直接丢，重传已经没有意义
    }

    this.stats.received++;
    this.queue.push({ ptsUs: frame.ptsUs, arrivedAtUs, frame });
    this.queue.sort((a, b) => (a.ptsUs < b.ptsUs ? -1 : a.ptsUs > b.ptsUs ? 1 : 0));

    if (this.queue.length > this.stats.maxDepth) this.stats.maxDepth = this.queue.length;

    const depthMs = Number(this.queue[this.queue.length - 1].arrivedAtUs - this.queue[0].arrivedAtUs) / 1000;
    this.stats.currentDepthMs = depthMs;

    let needsKeyframe = false;
    if (depthMs > this.maxMs) {
      // 积压过深通常意味着丢了一个参考帧，后面的帧全解不出来，直接清空等关键帧
      this.stats.dropped += this.queue.length;
      this.queue.length = 0;
      this.stats.keyframeRequests++;
      needsKeyframe = true;
    }

    return { needsKeyframe };
  }

  /**
   * 取出当前"到期应当播放"的帧。没有到期的返回 null。
   * @param {number} [nowUs]
   */
  pop(nowUs = Math.round(performance.now() * 1000)) {
    if (!this.queue.length) return null;
    const head = this.queue[0];
    const holdUs = this.targetMs * 1000;
    if (nowUs - Number(head.arrivedAtUs) < holdUs) return null;
    this.queue.shift();
    this.stats.played++;
    this.lastPlayedSeq = head.frame.seq;
    return head.frame;
  }

  /** 每秒调用一次：无事发生时让缓冲慢慢收敛回目标值，避免延迟永久漂高。 */
  shrink(elapsedMs) {
    this.targetMs = Math.max(this.targetMs - (this.shrinkPerSec * elapsedMs) / 1000, 10);
  }

  reset() {
    this.queue.length = 0;
    this.lastPlayedSeq = null;
    this.lastArrivalUs = null;
  }
}
