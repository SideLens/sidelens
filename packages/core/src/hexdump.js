/**
 * 把帧序号画成可读标记，叠在画面左上角。
 *
 * 用途：glass-to-glass 测量时，高速摄影拍下来的每一帧都能读出"这是第几帧"，
 * 从而算出真实延迟。这是阶段 0 最重要的验证手段。
 *
 * 编码方式：帧号低 8 位 → 8 个竖条，条高按位权递减（bit7 最高）。
 * 这种"条形码"比十六进制字符更适合远距离/低分辨率拍摄。
 */

const BAR_W = 4;
const BAR_GAP = 2;
const BITS = 8;

/** 在 RGBA 缓冲上画一块实心矩形 */
function fillRect(rgba, width, height, x0, y0, w, h, v) {
  const x1 = Math.min(x0 + w, width);
  const y1 = Math.min(y0 + h, height);
  for (let y = Math.max(y0, 0); y < y1; y++) {
    for (let x = Math.max(x0, 0); x < x1; x++) {
      const o = (y * width + x) * 4;
      rgba[o] = v; rgba[o + 1] = v; rgba[o + 2] = v; rgba[o + 3] = 255;
    }
  }
}

/**
 * @param {Buffer} rgba 宽*高*4
 * @param {number} width
 * @param {number} height
 * @param {number} value 帧号（只取低 8 位）
 * @param {object} [opts]
 */
export function drawFrameCode(rgba, width, height, value, { originX = 4, originY = 4 } = {}) {
  const v = value & 0xff;
  const maxH = 24;

  // 白色背板：保证无论画面什么颜色，标记都读得出来
  const padW = BITS * (BAR_W + BAR_GAP) + BAR_GAP;
  fillRect(rgba, width, height, originX, originY, padW, maxH + 2 * BAR_GAP, 255);

  for (let b = 0; b < BITS; b++) {
    const on = (v >> (BITS - 1 - b)) & 1;
    // 高度随位权线性递减：最高位 = 满高，最低位 = 1/8 高
    const h = Math.max(2, Math.round((maxH * (b + 1)) / BITS));
    const x = originX + BAR_GAP + b * (BAR_W + BAR_GAP);
    const y = originY + BAR_GAP + (maxH - h);
    fillRect(rgba, width, height, x, y, BAR_W, h, on ? 20 : 235);
  }
  return v;
}

/** 兼容旧名 */
export const drawHexGrid = drawFrameCode;
