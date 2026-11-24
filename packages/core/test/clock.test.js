import { test, describe } from "vitest";
import { expect } from "vitest";
import { RttEstimator, AvSync } from "../src/clock.js";

describe("时钟与音画同步", () => {
  test("RttEstimator 用中位数抗离群值", () => {
    const r = new RttEstimator();
    [10, 11, 9, 10, 999].forEach((v) => r.add(v));
    expect(r.median).toBe(10);
    expect(r.min).toBe(9);
  });

  test("空样本不抛异常", () => {
    const r = new RttEstimator();
    expect(r.median).toBe(0);
    expect(r.min).toBe(0);
  });

  test("无音频时视频直接播放", () => {
    const s = new AvSync();
    expect(s.judge({ ptsUs: 123n })).toBe("play");
  });

  test("视频落后过多时丢帧", () => {
    const s = new AvSync({ dropThresholdMs: 100 });
    s.onAudio({ ptsUs: 10_000_000n, durationUs: 20_000 });
    expect(s.judge({ ptsUs: 5_000_000n })).toBe("drop");
    expect(s.stats.droppedLate).toBe(1);
  });

  test("视频超前时等待", () => {
    const s = new AvSync({ maxDriftMs: 40 });
    s.onAudio({ ptsUs: 1_000_000n, durationUs: 20_000 });
    expect(s.judge({ ptsUs: 2_000_000n })).toBe("wait");
  });

  test("对齐窗口内正常播放", () => {
    const s = new AvSync({ maxDriftMs: 40, dropThresholdMs: 120 });
    s.onAudio({ ptsUs: 1_000_000n, durationUs: 20_000 });
    expect(s.judge({ ptsUs: 1_020_000n })).toBe("play");
  });
});
