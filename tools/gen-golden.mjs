import { encodeFrame, FrameType } from "../packages/proto/src/index.js";

const cases = [
  { name: "video_empty",     type: FrameType.VIDEO, flags: 0, streamId: 0, seq: 0,   ptsUs: 0n,               sendTsUs: 0n,               payload: Buffer.alloc(0) },
  { name: "video_abc",       type: FrameType.VIDEO, flags: 0, streamId: 0, seq: 1,   ptsUs: 1000n,            sendTsUs: 2000n,            payload: Buffer.from("abc") },
  { name: "video_key",       type: FrameType.VIDEO, flags: 1, streamId: 7, seq: 12345, ptsUs: 1700000000000n, sendTsUs: 1700000000500n,   payload: Buffer.from([1,2,3,4]) },
  { name: "audio_50",        type: FrameType.AUDIO, flags: 0, streamId: 1, seq: 42,  ptsUs: 5000000n,         sendTsUs: 5000100n,         payload: Buffer.alloc(50, 9) },
  { name: "hello_json",      type: FrameType.HELLO, flags: 0, streamId: 0, seq: 0,   ptsUs: 0n,               sendTsUs: 0n,               payload: Buffer.from('{"v":0,"name":"side"}') },
  { name: "stats_json",      type: FrameType.STATS, flags: 0, streamId: 0, seq: 9,   ptsUs: 7000n,            sendTsUs: 7000n,            payload: Buffer.from('{"rtt":3}') },
  { name: "max_seq",         type: FrameType.KEYFRAME_REQ, flags: 0, streamId: 65535, seq: 4294967295, ptsUs: 1n, sendTsUs: 2n,           payload: Buffer.from("x") },
];

const out = [];
for (const c of cases) {
  const buf = encodeFrame({
    type: c.type, flags: c.flags, streamId: c.streamId, seq: c.seq,
    ptsUs: c.ptsUs, sendTsUs: c.sendTsUs, payload: c.payload,
  });
  out.push({ name: c.name, hex: buf.toString("hex"), len: buf.length });
}
console.log(JSON.stringify(out, null, 2));
