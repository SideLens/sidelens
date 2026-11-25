// NV12 -> PNG（零依赖，用 Node 内置 zlib 手写 PNG）
import { readFileSync, writeFileSync } from "node:fs";
import { deflateSync } from "node:zlib";

const [raw, out, wStr, hStr] = process.argv.slice(2);
const width = Number(wStr || 1280);
const height = Number(hStr || 720);
const buf = readFileSync(raw);
const yLen = width * height;
if (buf.length < yLen + (yLen >> 1)) { console.error("NV12 长度不足"); process.exit(1); }

// BT.709 limited range -> RGB
const clamp = (v) => Math.max(0, Math.min(255, Math.round(v)));
const rgba = Buffer.alloc(width * height * 3);
for (let y = 0; y < height; y++) {
  for (let x = 0; x < width; x++) {
    const Y = buf[y * width + x];
    const uvIndex = yLen + ((y >> 1) * width) + ((x >> 1) << 1);
    const U = buf[uvIndex] - 128;
    const V = buf[uvIndex + 1] - 128;
    const C = 1.164383 * (Y - 16);
    const r = C + 1.792741 * V;
    const g = C - 0.213249 * U - 0.532909 * V;
    const b = C + 2.112402 * U;
    const o = (y * width + x) * 3;
    rgba[o] = clamp(r); rgba[o + 1] = clamp(g); rgba[o + 2] = clamp(b);
  }
}

// 组装 PNG：filter byte 0 + RGB 每行
const stride = width * 3;
const rawRows = Buffer.alloc((stride + 1) * height);
for (let y = 0; y < height; y++) {
  rawRows[y * (stride + 1)] = 0;
  rgba.copy(rawRows, y * (stride + 1) + 1, y * stride, (y + 1) * stride);
}

const crcTable = new Uint32Array(256);
for (let n = 0; n < 256; n++) {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  crcTable[n] = c >>> 0;
}
const crc32 = (b) => { let c = 0xffffffff; for (const v of b) c = crcTable[(c ^ v) & 0xff] ^ (c >>> 8); return (c ^ 0xffffffff) >>> 0; };

const chunk = (type, data) => {
  const len = Buffer.alloc(4); len.writeUInt32BE(data.length);
  const td = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4); crc.writeUInt32BE(crc32(td));
  return Buffer.concat([len, td, crc]);
};

const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(width, 0); ihdr.writeUInt32BE(height, 4);
ihdr[8] = 8; ihdr[9] = 2; ihdr[10] = 0; ihdr[11] = 0; ihdr[12] = 0;

const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk("IHDR", ihdr),
  chunk("IDAT", deflateSync(rawRows, { level: 6 })),
  chunk("IEND", Buffer.alloc(0)),
]);
writeFileSync(out, png);
console.log(`PNG ${width}x${height} -> ${out} (${png.length} bytes)`);
