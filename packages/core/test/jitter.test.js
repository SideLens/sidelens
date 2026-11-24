import { test, describe } from "vitest";
import { expect } from "vitest";
import { JitterBuffer } from "../src/jitter.js";
import { FrameType, nowUs } from "@sidelens/proto";

function mkFrame({ seq, ptsUs = nowUs(), type = FrameType.VIDEO } = {}) {
  return { type, seq, ptsUs, sendTsUs: nowUs(), payload: Buffer.alloc(4) };
}

describe("抖动缓冲", () => {
  test("缓冲深度未到目标延迟时不出帧", () => {
    const jb = new JitterBuffer({ targetMs: 50 });
    jb.push(mkFrame({ seq: 1 }));
    expect(jb.pop()).toBeNull();
  });

  test("超过目标延迟后按顺序出帧", () => {
    const jb = new JitterBuffer({ targetMs: 10 });
    for (let i = 1; i <= 3; i++) jb.push(mkFrame({ seq: i }));
    const got = [];
    const later = Math.round(performance.now() * 1000) + 50_000;
    for (let i = 0; i < 3; i++) {
      const f = jb.pop(later);
      if (f) got.push(f.seq);
    }
    expect(got).toEqual([1, 2, 3]);
  });

  test("乱序到达会按 PTS 重排序", () => {
    const jb = new JitterBuffer({ targetMs: 0 });
    const base = nowUs();
    jb.push({ ...mkFrame({ seq: 2 }), ptsUs: base + 2000n });
    jb.push({ ...mkFrame({ seq: 1 }), ptsUs: base + 1000n });
    const later = Math.round(performance.now() * 1000) + 60_000;
    expect(jb.pop(later).seq).toBe(1);
    expect(jb.pop(later).seq).toBe(2);
  });

  test("已播放过的旧帧才被丢弃，未播放的乱序帧正常入队", () => {
    const jb = new JitterBuffer({ targetMs: 0 });
    jb.push(mkFrame({ seq: 5 }));
    const later = Math.round(performance.now() * 1000) + 30_000;
    expect(jb.pop(later).seq).toBe(5);

    const r = jb.push(mkFrame({ seq: 3 }));
    expect(jb.stats.reordered).toBe(1);
    expect(r.needsKeyframe).toBe(false);
    expect(jb.queue.length).toBe(0);
  });

  test("积压过深时清空并请求关键帧", () => {
    const jb = new JitterBuffer({ targetMs: 20, maxMs: 5 });
    jb.push({ ...mkFrame({ seq: 1 }), ptsUs: 1n });
    jb.queue[0].arrivedAtUs = BigInt(Math.round(performance.now() * 1000) - 1_000_000);
    const r = jb.push(mkFrame({ seq: 2 }));
    expect(r.needsKeyframe).toBe(true);
    expect(jb.queue.length).toBe(0);
    expect(jb.stats.keyframeRequests).toBe(1);
  });

  test("shrink 让目标延迟缓慢下降但保底 10ms", () => {
    const jb = new JitterBuffer({ targetMs: 60, shrinkPerSec: 100 });
    jb.shrink(1000);
    expect(jb.targetMs).toBe(10);
  });
});
