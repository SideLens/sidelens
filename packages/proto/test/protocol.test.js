import { test, describe } from "vitest";
import { expect } from "vitest";
import {
  encodeFrame, FrameDecoder, decodeOne, FrameType, HEADER_SIZE, MAGIC, PROTOCOL_VERSION,
} from "../src/index.js";

describe("帧协议", () => {
  test("magic 与协议版本", () => {
    expect(MAGIC.toString("ascii")).toBe("SLN0");
    expect(PROTOCOL_VERSION).toBe(0);
  });

  test("编码后头长固定 32 字节", () => {
    const buf = encodeFrame({ type: FrameType.VIDEO, payload: Buffer.from("abc") });
    expect(buf.length).toBe(HEADER_SIZE + 3);
    expect(buf.readUInt8(4)).toBe(FrameType.VIDEO);
    expect(buf.readUInt32LE(28)).toBe(3);
  });

  test("往返编解码保持字段一致", () => {
    const buf = encodeFrame({
      type: FrameType.VIDEO, flags: 1, streamId: 7, seq: 12345,
      ptsUs: 1_700_000_000_000n, sendTsUs: 1_700_000_000_500n,
      payload: Buffer.from([1, 2, 3, 4]),
    });
    const f = decodeOne(buf);
    expect(f.type).toBe(FrameType.VIDEO);
    expect(f.flags).toBe(1);
    expect(f.streamId).toBe(7);
    expect(f.seq).toBe(12345);
    expect(f.ptsUs).toBe(1_700_000_000_000n);
    expect(f.sendTsUs).toBe(1_700_000_000_500n);
    expect([...f.payload]).toEqual([1, 2, 3, 4]);
  });

  test("控制帧的 payload 自动解析为 JSON", () => {
    const buf = encodeFrame({ type: FrameType.HELLO, payload: { v: 0, name: "side" } });
    expect(decodeOne(buf).json).toEqual({ v: 0, name: "side" });
  });

  test("分片喂入也能拼出完整帧", () => {
    const a = encodeFrame({ type: FrameType.VIDEO, payload: Buffer.alloc(100, 7) });
    const b = encodeFrame({ type: FrameType.AUDIO, payload: Buffer.alloc(50, 9) });
    const all = Buffer.concat([a, b]);

    const dec = new FrameDecoder();
    const got = [];
    for (let i = 0; i < all.length; i += 7) got.push(...dec.push(all.subarray(i, i + 7)));

    expect(got.length).toBe(2);
    expect(got[0].type).toBe(FrameType.VIDEO);
    expect(got[1].type).toBe(FrameType.AUDIO);
    expect(dec.pendingBytes).toBe(0);
  });

  test("流被污染时能重新同步", () => {
    const dec = new FrameDecoder();
    const junk = Buffer.from("这是噪声数据不是帧头");
    const good = encodeFrame({ type: FrameType.STATS, payload: { rtt: 3 } });
    const got = dec.push(Buffer.concat([junk, good]));
    expect(got.length).toBe(1);
    expect(got[0].type).toBe(FrameType.STATS);
  });

  test("非法参数被拒绝", () => {
    expect(() => encodeFrame({ type: 99 })).toThrow(RangeError);
    expect(() => encodeFrame({ type: FrameType.VIDEO, seq: -1 })).toThrow(RangeError);
    expect(() => encodeFrame({ type: FrameType.VIDEO, streamId: 70000 })).toThrow(RangeError);
  });
});
