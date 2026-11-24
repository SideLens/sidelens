/**
 * SideLens 帧协议 v0
 *
 * 设计目标：足够简单，能在一页内读完；足够完整，能承载真实的低延迟需求。
 *
 * 帧布局（小端）:
 *   0  magic      4B   'S','L','N','0'
 *   4  type       1B   0=HELLO 1=CONFIG 2=VIDEO 3=AUDIO 4=KEYFRAME_REQ 5=STATS 6=BYE
 *   5  flags      1B   bit0=keyframe
 *   6  streamId   2B   多路流（0=视频主路, 1=音频）
 *   8  seq        4B   每路独立递增
 *  12  ptsUs      8B   BigInt，发送端单调时钟，微秒
 *  20  sendTsUs   8B   BigInt，同上，用于 RTT / 抖动测量
 *  28  payloadLen 4B
 *  32  payload    N B
 *
 * 合计头长 32 字节。控制帧（HELLO/CONFIG/STATS/BYE）的 payload 是 UTF-8 JSON。
 */

export const MAGIC = Buffer.from([0x53, 0x4c, 0x4e, 0x30]); // "SLN0"
export const HEADER_SIZE = 32;
export const PROTOCOL_VERSION = 0;

export const FrameType = Object.freeze({
  HELLO: 0,
  CONFIG: 1,
  VIDEO: 2,
  AUDIO: 3,
  KEYFRAME_REQ: 4,
  STATS: 5,
  BYE: 6,
});

export const TYPE_NAMES = Object.freeze(
  Object.fromEntries(Object.entries(FrameType).map(([k, v]) => [v, k]))
);

export const FLAG_KEYFRAME = 0x01;

/**
 * 跨进程一致的单调时钟（微秒）。
 *
 * 坑：`performance.now()` 的零点**是进程启动时刻**，两个进程相差的启动时间会
 * 直接变成"延迟"。真实设备上这是致命的 —— 手机和电脑的时钟本来就不同源。
 *
 * 这里的做法：`performance.timeOrigin + performance.now()` 得到 Unix 纪元时间，
 * 既保留了亚毫秒精度，又让同一台机器上的多个进程共享同一个时基。
 * 跨设备时仍需做一次时钟偏移协商（见 docs/protocol.md 的"时钟同步"一节）。
 */
export function nowUs() {
  return BigInt(Math.round((performance.timeOrigin + performance.now()) * 1000));
}

function assertUInt(value, bits, label) {
  const max = bits === 32 ? 0xffffffff : 0xffff;
  if (!Number.isInteger(value) || value < 0 || value > max) {
    throw new RangeError(`${label} 必须是 0..${max} 的整数，收到 ${value}`);
  }
}

/**
 * 编码一帧。payload 可以是 Buffer 或任意可 JSON 序列化的对象。
 */
export function encodeFrame({ type, flags = 0, streamId = 0, seq = 0, ptsUs, sendTsUs, payload = Buffer.alloc(0) }) {
  if (!(type in TYPE_NAMES)) throw new RangeError(`未知帧类型 ${type}`);
  assertUInt(streamId, 16, "streamId");
  assertUInt(seq, 32, "seq");

  const body = Buffer.isBuffer(payload)
    ? payload
    : Buffer.from(JSON.stringify(payload), "utf8");

  const buf = Buffer.allocUnsafe(HEADER_SIZE + body.length);
  MAGIC.copy(buf, 0);
  buf.writeUInt8(type, 4);
  buf.writeUInt8(flags & 0xff, 5);
  buf.writeUInt16LE(streamId, 6);
  buf.writeUInt32LE(seq, 8);
  buf.writeBigUInt64LE(BigInt(ptsUs ?? nowUs()), 12);
  buf.writeBigUInt64LE(BigInt(sendTsUs ?? nowUs()), 20);
  buf.writeUInt32LE(body.length, 28);
  body.copy(buf, HEADER_SIZE);
  return buf;
}

/**
 * 增量解码器：喂任意切分的 chunk，吐出完整的帧对象。
 * UDP 场景下每包就是完整帧，但 TCP/WS 会分片，所以这里做流式拼装。
 */
export class FrameDecoder {
  #buffer = Buffer.alloc(0);

  /** @returns {Array<{type:number,typeName:string,flags:number,streamId:number,seq:number,ptsUs:bigint,sendTsUs:bigint,payload:Buffer,json:any}>} */
  push(chunk) {
    this.#buffer = this.#buffer.length ? Buffer.concat([this.#buffer, chunk]) : chunk;
    const out = [];

    while (this.#buffer.length >= HEADER_SIZE) {
      if (!this.#buffer.subarray(0, 4).equals(MAGIC)) {
        // 流被污染：向后找下一个 magic，避免整条连接作废
        const idx = this.#buffer.indexOf(MAGIC, 1);
        this.#buffer = idx === -1 ? Buffer.alloc(0) : this.#buffer.subarray(idx);
        if (idx === -1) break;
        continue;
      }

      const payloadLen = this.#buffer.readUInt32LE(28);
      const total = HEADER_SIZE + payloadLen;
      if (this.#buffer.length < total) break;

      const frame = this.#buffer.subarray(0, total);
      this.#buffer = this.#buffer.subarray(total);

      const type = frame.readUInt8(4);
      const payload = frame.subarray(HEADER_SIZE, total);
      const isControl = type !== FrameType.VIDEO && type !== FrameType.AUDIO;

      out.push({
        type,
        typeName: TYPE_NAMES[type] ?? "UNKNOWN",
        flags: frame.readUInt8(5),
        streamId: frame.readUInt16LE(6),
        seq: frame.readUInt32LE(8),
        ptsUs: frame.readBigUInt64LE(12),
        sendTsUs: frame.readBigUInt64LE(20),
        payload,
        json: isControl && payload.length ? safeJson(payload) : null,
      });
    }
    return out;
  }

  get pendingBytes() {
    return this.#buffer.length;
  }
}

function safeJson(buf) {
  try {
    return JSON.parse(buf.toString("utf8"));
  } catch {
    return null;
  }
}

/** 单帧解码，用于测试与自检。头部不完整时返回 null。 */
export function decodeOne(buf) {
  return new FrameDecoder().push(buf)[0] ?? null;
}
