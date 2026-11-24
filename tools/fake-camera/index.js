/**
 * 假摄像头：合成"带时间码的测试图"
 *
 * 为什么需要它：真实摄像头要授权、要设备、平台差异大。
 * demo 阶段先用合成图跑通全链路，把延迟数字量出来 —— 这才是阶段 0 的目的。
 *
 * 画面上叠了两层可读信息：
 *   1. 左上角十六进制网格 —— 高速摄影下可直接读出帧号
 *   2. 底部 16 位二进制条 —— 机器可读，便于脚本自动比对
 */

import { encodePNG } from "@sidelens/proto/png";
import { drawFrameCode } from "@sidelens/core/hexdump";

export class FakeCamera {
  constructor({ width = 320, height = 240, fps = 30 } = {}) {
    this.width = width;
    this.height = height;
    this.fps = fps;
    this.frameIndex = 0;
    this.rgba = Buffer.alloc(width * height * 4);
  }

  /** 生成下一帧。带 ptsUs，供上层打时间戳。 */
  capture(ptsUs) {
    const { width, height, rgba } = this;
    const i = this.frameIndex++;

    // 背景：移动的斜向渐变
    for (let y = 0; y < height; y++) {
      for (let x = 0; x < width; x++) {
        const o = (y * width + x) * 4;
        const wave = (x + y + i * 4) & 0xff;
        rgba[o] = wave;
        rgba[o + 1] = (y * 255 / height) | 0;
        rgba[o + 2] = (x * 255 / width) | 0;
        rgba[o + 3] = 255;
      }
    }

    // 中央移动方块：给"是否有运动"一个直观参照
    const size = Math.min(width, height) >> 3;
    const cx = Math.round((width - size) / 2 + Math.sin(i * 0.1) * (width / 3));
    const cy = Math.round((height - size) / 2 + Math.cos(i * 0.13) * (height / 3));
    for (let y = cy; y < cy + size; y++) {
      for (let x = cx; x < cx + size; x++) {
        if (x < 0 || y < 0 || x >= width || y >= height) continue;
        const o = (y * width + x) * 4;
        rgba[o] = 255; rgba[o + 1] = 240; rgba[o + 2] = 0; rgba[o + 3] = 255;
      }
    }

    // 底部 16 位二进制条（机器可读）
    const bits = Math.min(16, Math.floor(width / 8));
    const barH = Math.max(6, height >> 5);
    for (let b = 0; b < bits; b++) {
      const on = (i >> (bits - 1 - b)) & 1;
      const x0 = b * 8 + 2;
      for (let y = height - barH; y < height; y++) {
        for (let x = x0; x < x0 + 6; x++) {
          const o = (y * width + x) * 4;
          const v = on ? 255 : 0;
          rgba[o] = v; rgba[o + 1] = v; rgba[o + 2] = v; rgba[o + 3] = 255;
        }
      }
    }

    // 左上角帧号条码（人眼/高速摄影可读）
    drawFrameCode(rgba, width, height, i);

    return { seq: i, width, height, ptsUs, rgba: Buffer.from(rgba) };
  }

  toPNG(frame) {
    return encodePNG(frame.rgba, frame.width, frame.height);
  }
}
