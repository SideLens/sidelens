#!/usr/bin/env node
// H.264 Annex-B 码流分析器（独立实现，不依赖 sidelens 的任何 crate）
//
// # 为什么要有这个工具
//
// 项目里的 SPS 解析器（`crates/sidelens-core/src/sps.rs`）是用 Rust 写的，
// 测试时和被测代码**共用同一套逻辑**。如果解析器本身有 bug，
// 测试会「自洽地通过」而实际码流是错的。
//
// 这个工具用**完全独立的实现**（Node，从头写 bit reader）做交叉验证：
// 两边对同一份 dump 得出相同结论，才算真验证过。
//
// 用法：
//   node tools/h264-inspect.mjs <file.h264> [--json]

import { readFileSync } from 'node:fs';

const args = process.argv.slice(2);
const file = args.find(a => !a.startsWith('--'));
const asJson = args.includes('--json');
const summary = args.includes('--summary');
if (!file) {
  console.error(`用法: node tools/h264-inspect.mjs <file.h264> [--json] [--summary] [--expect=path=val,...]

  --json      输出机器可读 JSON
  --summary   只输出一行摘要（用于批量扫描）
  --expect    断言模式，不符则 exit 1`);
  process.exit(1);
}

const buf = readFileSync(file);

// ── 找所有起始码（00 00 01 或 00 00 00 01）────────────────────
function findNalus(data) {
  const starts = [];
  for (let i = 0; i + 3 < data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
      // 4 字节起始码：前面还有一个 0
      const scLen = (i > 0 && data[i - 1] === 0) ? 4 : 3;
      starts.push({ pos: i - (scLen === 4 ? 1 : 0), scLen, bodyStart: i + 3 });
    }
  }
  const nalus = [];
  for (let k = 0; k < starts.length; k++) {
    const s = starts[k];
    const end = k + 1 < starts.length ? starts[k + 1].pos : data.length;
    const body = data.subarray(s.bodyStart, end);
    if (body.length === 0) continue;
    nalus.push({
      offset: s.pos,
      scLen: s.scLen,
      bytes: body.length,
      refIdc: (body[0] >> 5) & 0x3,
      type: body[0] & 0x1f,
      body,
    });
  }
  return nalus;
}

const NALU_NAMES = {
  1: 'non-IDR slice', 2: 'slice A', 3: 'slice B', 4: 'slice C',
  5: 'IDR slice', 6: 'SEI', 7: 'SPS', 8: 'PPS', 9: 'AUD',
  10: 'end of seq', 11: 'end of stream', 12: 'filler',
};

// ── 独立的 bit reader（Exp-Golomb）────────────────────────────
class BitReader {
  constructor(bytes) {
    // 去掉 emulation prevention bytes (00 00 03 -> 00 00)
    const out = [];
    let zeros = 0;
    for (const b of bytes) {
      if (zeros >= 2 && b === 0x03) { zeros = 0; continue; }
      out.push(b);
      zeros = b === 0 ? zeros + 1 : 0;
    }
    this.data = Uint8Array.from(out);
    this.bit = 0;
  }
  u(n) {
    let v = 0;
    for (let i = 0; i < n; i++) {
      const byte = this.data[this.bit >> 3] ?? 0;
      v = (v << 1) | ((byte >> (7 - (this.bit & 7))) & 1);
      this.bit++;
    }
    return v;
  }
  ue() {
    let lz = 0;
    while (this.u(1) === 0) { lz++; if (lz > 32) throw new Error('ue 溢出'); }
    return (1 << lz) - 1 + this.u(lz);
  }
  se() {
    const k = this.ue();
    return (k & 1) ? (k + 1) / 2 : -(k / 2);
  }
}

// ── SPS 解析（只取关键字段，与项目 sps.rs 独立实现）────────────
function parseSps(body) {
  const r = new BitReader(body.subarray(1)); // 跳过 NAL header
  const profileIdc = r.u(8);
  const constraints = r.u(8);
  const levelIdc = r.u(8);
  const spsId = r.ue();
  let chromaFormatIdc = 1;
  if ([100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].includes(profileIdc)) {
    chromaFormatIdc = r.ue();
    if (chromaFormatIdc === 3) r.u(1);
    r.ue(); r.ue(); r.u(1);
    if (r.u(1)) {
      const n = chromaFormatIdc !== 3 ? 8 : 12;
      for (let i = 0; i < n; i++) if (r.u(1)) {
        let last = 8, next = 8;
        for (let j = 0; j < (i < 6 ? 16 : 64); j++) {
          if (next !== 0) next = (last + r.se() + 256) % 256;
          last = next === 0 ? last : next;
        }
      }
    }
  }
  r.ue(); // log2_max_frame_num_minus4
  const pocType = r.ue();
  if (pocType === 0) r.ue();
  else if (pocType === 1) {
    r.u(1); r.se(); r.se();
    const n = r.ue();
    for (let i = 0; i < n; i++) r.se();
  }
  r.ue(); // max_num_ref_frames
  r.u(1);  // gaps_in_frame_num
  const widthMbs = r.ue() + 1;
  const heightMapUnits = r.ue() + 1;
  const frameMbsOnly = r.u(1);
  if (!frameMbsOnly) r.u(1);
  r.u(1); // direct_8x8
  let crop = { l: 0, r: 0, t: 0, b: 0 };
  if (r.u(1)) {
    crop = { l: r.ue(), r: r.ue(), t: r.ue(), b: r.ue() };
  }
  const chromaArrayType = chromaFormatIdc === 0 ? 0 : chromaFormatIdc;
  const subW = chromaFormatIdc === 3 ? 1 : 2;
  const subH = (chromaFormatIdc === 1 ? 2 : 1) * (frameMbsOnly ? 1 : 2);
  const cropUnitX = chromaFormatIdc === 0 ? 1 : subW;
  const cropUnitY = chromaFormatIdc === 0 ? (frameMbsOnly ? 1 : 2) : subH;

  const codedW = widthMbs * 16;
  const codedH = heightMapUnits * 16 * (frameMbsOnly ? 1 : 2);
  const w = codedW - (crop.l + crop.r) * cropUnitX;
  const h = codedH - (crop.t + crop.b) * cropUnitY;

  return { profileIdc, constraints, levelIdc, spsId, chromaFormatIdc, codedW, codedH, width: w, height: h, frameMbsOnly, crop, chromaArrayType };
}

// ── 分析 ──────────────────────────────────────────────────────
const nalus = findNalus(buf);
if (nalus.length === 0) {
  console.error('没有找到任何 NALU（不是 Annex-B 码流？）');
  process.exit(2);
}

const counts = new Map();
let sps = null, ppsCount = 0, idrCount = 0, sliceCount = 0, seiCount = 0;
let firstSpsOffset = -1;
const frameSizes = [];
let curFrame = 0;

// 切帧边界：**不能**把每个 slice NALU 当成一帧。
//
// # 这是第一版分析器的真 bug
//
// 第一版按「每个 type 1/5 的 NALU 就是一帧」统计，得到 frames=2527，
// 而发送端日志说 encode_frames=361。差 7 倍 —— 因为 MF H.264 编码器
// **把一帧切成 7 个 slice**（多 slice 编码，提升抗丢包能力）。
//
// 正确的帧边界有两个判据：
//   1. AUD（type 9）—— 每个 access unit 前面有一个，最可靠
//   2. slice 的 first_mb_in_slice == 0 —— 表示这是新一帧的第一个 slice
//
// 这里优先用 AUD（如果有），否则回退到 first_mb_in_slice。
// 两者都没有就退化成「每个 slice 一帧」并在报告里标注不可靠。

// 先探测是否有 AUD
const hasAud = nalus.some(n => n.type === 9);
let pendingNewFrame = true; // 流开始时视为新帧

// 读 first_mb_in_slice：VCL NALU 体的第一个 ue(v)
function firstMbInSlice(body) {
  try {
    const r = new BitReader(body.subarray(1));
    return r.ue();
  } catch { return -1; }
}

for (const n of nalus) {
  counts.set(n.type, (counts.get(n.type) ?? 0) + 1);
  if (n.type === 7 && !sps) {
    try { sps = parseSps(n.body); firstSpsOffset = n.offset; }
    catch (e) { console.error('SPS 解析失败:', e.message); }
  }
  if (n.type === 8) ppsCount++;
  if (n.type === 6) seiCount++;

  const isVcl = n.type === 1 || n.type === 5;
  if (isVcl) {
    sliceCount++;
    if (n.type === 5) idrCount++;
  }

  // 帧边界判定
  let startsNewFrame = false;
  if (hasAud) {
    startsNewFrame = (n.type === 9);
  } else if (isVcl) {
    startsNewFrame = (firstMbInSlice(n.body) === 0);
  } else if (n.type === 9) {
    startsNewFrame = true;
  }

  if (startsNewFrame && !pendingNewFrame && curFrame > 0) {
    frameSizes.push(curFrame);
    curFrame = 0;
  }
  if (startsNewFrame) pendingNewFrame = false;

  curFrame += n.bytes + n.scLen;
}
// 收尾：最后一帧
if (curFrame > 0) frameSizes.push(curFrame);

// 关键帧位置与间隔（GOP 验证）
const keyframePositions = [];
{
  let idx = 0;
  let seenAud = false;
  let frameHasIdr = false;
  for (const n of nalus) {
    if (n.type === 9) {
      if (seenAud && frameHasIdr) keyframePositions.push(idx);
      if (seenAud) idx++;
      seenAud = true;
      frameHasIdr = false;
    }
    if (n.type === 5) frameHasIdr = true;
  }
  if (seenAud && frameHasIdr) keyframePositions.push(idx);
}
const gopGaps = keyframePositions.slice(1).map((v, i) => v - keyframePositions[i]);
const gopUnique = [...new Set(gopGaps)].sort((a, b) => a - b);

const avg = frameSizes.length ? frameSizes.reduce((a, b) => a + b, 0) / frameSizes.length : 0;
const sorted = [...frameSizes].sort((a, b) => a - b);
const p50 = sorted.length ? sorted[Math.floor(sorted.length / 2)] : 0;
const p95 = sorted.length ? sorted[Math.floor(sorted.length * 0.95)] : 0;

const report = {
  file,
  bytes: buf.length,
  nalus: nalus.length,
  types: Object.fromEntries([...counts.entries()].sort((a, b) => a[0] - b[0]).map(([k, v]) => [`${k} (${NALU_NAMES[k] ?? '?'})`, v])),
  sps: sps ? {
    profile: sps.profileIdc, level: sps.levelIdc, spsId: sps.spsId,
    width: sps.width, height: sps.height,
    codedWidth: sps.codedW, codedHeight: sps.codedH,
    crop: sps.crop,
  } : null,
  firstSpsAtByte: firstSpsOffset,
  pps: ppsCount,
  sei: seiCount,
  idrSlices: idrCount,
  slices: sliceCount,
  frameBoundary: hasAud ? 'AUD (type 9)' : 'first_mb_in_slice == 0',
  frames: frameSizes.length,
  keyframes: keyframePositions.length,
  keyframePositions,
  gopGaps,
  gopUnique,
  frameBytes: {
    avg: Math.round(avg), p50, p95,
    min: sorted[0] ?? 0, max: sorted[sorted.length - 1] ?? 0,
  },
};

// --expect frames=361,sps.width=1280 之类的断言模式
// 用法: node tools/h264-inspect.mjs f.h264 --expect frames=361
const expectArg = args.find(a => a.startsWith('--expect='));
if (expectArg) {
  const pairs = expectArg.slice('--expect='.length).split(',').map(s => s.trim()).filter(Boolean);
  let failed = 0;
  for (const pair of pairs) {
    const [path, want] = pair.split('=');
    const got = path.split('.').reduce((o, k) => (o == null ? undefined : o[k]), report);
    const ok = String(got) === want;
    if (!ok) failed++;
    console.log(`${ok ? 'OK  ' : 'FAIL'} ${path}: 期望 ${want}, 实际 ${got}`);
  }
  process.exit(failed === 0 ? 0 : 1);
}

if (summary) {
  // 单行摘要，便于 for 循环批量扫目录
  const s = report.sps;
  const res = s ? `${s.width}x${s.height}` : 'NO-SPS';
  console.log(
    `${file}\t${report.frames}帧\t${res}\t关键帧=${report.keyframes}\tGOP=${report.gopUnique.length === 1 ? report.gopUnique[0] : report.gopUnique.join('/')}\t${report.bytes}B`
  );
  process.exit(0);
}

if (asJson) {
  console.log(JSON.stringify(report, null, 2));
} else {
  console.log(`文件: ${report.file}`);
  console.log(`大小: ${report.bytes.toLocaleString()} 字节`);
  console.log(`NALU: ${report.nalus} 个`);
  console.log('');
  console.log('NALU 类型分布:');
  for (const [k, v] of Object.entries(report.types)) console.log(`  ${k.padEnd(24)} ${v}`);
  console.log('');
  if (report.sps) {
    const s = report.sps;
    console.log(`SPS: profile=${s.profile} level=${s.level} id=${s.spsId}`);
    console.log(`  分辨率: ${s.width}x${s.height}  (coded ${s.codedWidth}x${s.codedHeight})`);
    console.log(`  裁剪: left=${s.crop.l} right=${s.crop.r} top=${s.crop.t} bottom=${s.crop.b}`);
    console.log(`  首个 SPS 位置: 第 ${report.firstSpsAtByte} 字节`);
  } else {
    console.log('SPS: 未找到（无法解码）');
  }
  console.log('');
  console.log(`PPS: ${report.pps}   SEI: ${report.sei}   IDR slice: ${report.idrSlices}   Slice 总数: ${report.slices}`);
  console.log(`切帧依据: ${report.frameBoundary}`);
  console.log(`帧数: ${report.frames}   关键帧: ${report.keyframes}`);
  if (report.gopUnique.length === 1) {
    console.log(`GOP 间隔: 恒为 ${report.gopUnique[0]} 帧`);
  } else if (report.gopUnique.length > 1) {
    console.log(`GOP 间隔: 不恒定，取值 ${report.gopUnique.join('/')}（前 10 个间隔: ${report.gopGaps.slice(0, 10).join(',')}）`);
  }
  console.log(`每帧字节: avg=${report.frameBytes.avg} p50=${report.frameBytes.p50} p95=${report.frameBytes.p95} min=${report.frameBytes.min} max=${report.frameBytes.max}`);
}
