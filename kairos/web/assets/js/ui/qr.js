// QR code 產生器（兩步驟驗證的 otpauth URI；screens.md S23「顯示 QR code 與可手動輸入的金鑰」）。
// 在本機產生（離線環境不能用外部 QR 服務），輸出 SVG。只支援 byte 模式、錯誤更正等級 M、版本 1～15，
// 足夠 otpauth URI（約 100～200 bytes）。演算法依 ISO/IEC 18004（參考 Project Nayuki 的 QR Code generator，MIT）。

import { s } from '../core/dom.js';

// 錯誤更正等級 M：每個版本的「每區塊 ECC 碼字數」與「區塊數」（索引為版本）
const ECC_PER_BLOCK_M = [-1, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26, 30, 22, 22, 24, 24];
const BLOCKS_M = [-1, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5, 5, 8, 9, 9, 10];
const MAX_VERSION = 15;

function rawModules(ver) {
  let r = (16 * ver + 128) * ver + 64;
  if (ver >= 2) {
    const n = Math.floor(ver / 7) + 2;
    r -= (25 * n - 10) * n - 55;
    if (ver >= 7) r -= 36;
  }
  return r;
}

function dataCodewords(ver) {
  return Math.floor(rawModules(ver) / 8) - ECC_PER_BLOCK_M[ver] * BLOCKS_M[ver];
}

// GF(256) 的 Reed–Solomon
function rsMul(x, y) {
  let z = 0;
  for (let i = 7; i >= 0; i -= 1) {
    z = (z << 1) ^ ((z >>> 7) * 0x11d);
    z ^= ((y >>> i) & 1) * x;
  }
  return z & 0xff;
}

function rsDivisor(degree) {
  const result = new Array(degree).fill(0);
  result[degree - 1] = 1;
  let root = 1;
  for (let i = 0; i < degree; i += 1) {
    for (let j = 0; j < result.length; j += 1) {
      result[j] = rsMul(result[j], root);
      if (j + 1 < result.length) result[j] ^= result[j + 1];
    }
    root = rsMul(root, 0x02);
  }
  return result;
}

function rsRemainder(data, divisor) {
  const result = divisor.map(() => 0);
  for (const b of data) {
    const factor = b ^ result.shift();
    result.push(0);
    divisor.forEach((coef, i) => {
      result[i] ^= rsMul(coef, factor);
    });
  }
  return result;
}

function alignmentPositions(ver) {
  if (ver === 1) return [];
  const num = Math.floor(ver / 7) + 2;
  const step = ver === 32 ? 26 : Math.ceil((ver * 4 + 4) / (num * 2 - 2)) * 2;
  const result = [6];
  for (let pos = ver * 4 + 17 - 7; result.length < num; pos -= step) result.splice(1, 0, pos);
  return result;
}

class Qr {
  constructor(ver, codewords) {
    this.ver = ver;
    this.size = ver * 4 + 17;
    this.mod = Array.from({ length: this.size }, () => new Array(this.size).fill(false));
    this.fn = Array.from({ length: this.size }, () => new Array(this.size).fill(false));
    this.drawFunctionPatterns();
    const all = this.addEcc(codewords);
    this.drawCodewords(all);
    let best = 0;
    let bestPenalty = Infinity;
    for (let m = 0; m < 8; m += 1) {
      this.applyMask(m);
      this.drawFormat(m);
      const p = this.penalty();
      if (p < bestPenalty) {
        bestPenalty = p;
        best = m;
      }
      this.applyMask(m);
    }
    this.applyMask(best);
    this.drawFormat(best);
  }

  set(x, y, dark) {
    this.mod[y][x] = dark;
    this.fn[y][x] = true;
  }

  drawFunctionPatterns() {
    const n = this.size;
    for (let i = 0; i < n; i += 1) {
      this.set(6, i, i % 2 === 0);
      this.set(i, 6, i % 2 === 0);
    }
    this.finder(3, 3);
    this.finder(n - 4, 3);
    this.finder(3, n - 4);
    const pos = alignmentPositions(this.ver);
    for (let i = 0; i < pos.length; i += 1) {
      for (let j = 0; j < pos.length; j += 1) {
        if ((i === 0 && j === 0) || (i === 0 && j === pos.length - 1) || (i === pos.length - 1 && j === 0)) continue;
        this.alignment(pos[i], pos[j]);
      }
    }
    this.drawFormat(0);
    this.drawVersion();
  }

  finder(x, y) {
    for (let dy = -4; dy <= 4; dy += 1) {
      for (let dx = -4; dx <= 4; dx += 1) {
        const d = Math.max(Math.abs(dx), Math.abs(dy));
        const xx = x + dx;
        const yy = y + dy;
        if (xx >= 0 && xx < this.size && yy >= 0 && yy < this.size) this.set(xx, yy, d !== 2 && d !== 4);
      }
    }
  }

  alignment(x, y) {
    for (let dy = -2; dy <= 2; dy += 1) {
      for (let dx = -2; dx <= 2; dx += 1) this.set(x + dx, y + dy, Math.max(Math.abs(dx), Math.abs(dy)) !== 1);
    }
  }

  drawFormat(mask) {
    // 等級 M 的格式位元為 00
    const data = (0 << 3) | mask;
    let rem = data;
    for (let i = 0; i < 10; i += 1) rem = (rem << 1) ^ ((rem >>> 9) * 0x537);
    const bits = ((data << 10) | rem) ^ 0x5412;
    const bit = (i) => ((bits >>> i) & 1) !== 0;
    for (let i = 0; i <= 5; i += 1) this.set(8, i, bit(i));
    this.set(8, 7, bit(6));
    this.set(8, 8, bit(7));
    this.set(7, 8, bit(8));
    for (let i = 9; i < 15; i += 1) this.set(14 - i, 8, bit(i));
    const n = this.size;
    for (let i = 0; i < 8; i += 1) this.set(n - 1 - i, 8, bit(i));
    for (let i = 8; i < 15; i += 1) this.set(8, n - 15 + i, bit(i));
    this.set(8, n - 8, true);
  }

  drawVersion() {
    if (this.ver < 7) return;
    let rem = this.ver;
    for (let i = 0; i < 12; i += 1) rem = (rem << 1) ^ ((rem >>> 11) * 0x1f25);
    const bits = (this.ver << 12) | rem;
    for (let i = 0; i < 18; i += 1) {
      const color = ((bits >>> i) & 1) !== 0;
      const a = this.size - 11 + (i % 3);
      const b = Math.floor(i / 3);
      this.set(a, b, color);
      this.set(b, a, color);
    }
  }

  addEcc(data) {
    const ver = this.ver;
    const numBlocks = BLOCKS_M[ver];
    const eccLen = ECC_PER_BLOCK_M[ver];
    const raw = Math.floor(rawModules(ver) / 8);
    const numShort = numBlocks - (raw % numBlocks);
    const shortLen = Math.floor(raw / numBlocks);
    const blocks = [];
    const div = rsDivisor(eccLen);
    for (let i = 0, k = 0; i < numBlocks; i += 1) {
      const dat = data.slice(k, k + shortLen - eccLen + (i < numShort ? 0 : 1));
      k += dat.length;
      const ecc = rsRemainder(dat, div);
      if (i < numShort) dat.push(0);
      blocks.push(dat.concat(ecc));
    }
    const result = [];
    for (let i = 0; i < blocks[0].length; i += 1) {
      blocks.forEach((blk, j) => {
        if (i !== shortLen - eccLen || j >= numShort) result.push(blk[i]);
      });
    }
    return result;
  }

  drawCodewords(data) {
    let i = 0;
    const n = this.size;
    for (let right = n - 1; right >= 1; right -= 2) {
      if (right === 6) right = 5;
      for (let vert = 0; vert < n; vert += 1) {
        for (let j = 0; j < 2; j += 1) {
          const x = right - j;
          const upward = ((right + 1) & 2) === 0;
          const y = upward ? n - 1 - vert : vert;
          if (!this.fn[y][x] && i < data.length * 8) {
            this.mod[y][x] = ((data[i >>> 3] >>> (7 - (i & 7))) & 1) !== 0;
            i += 1;
          }
        }
      }
    }
  }

  applyMask(mask) {
    for (let y = 0; y < this.size; y += 1) {
      for (let x = 0; x < this.size; x += 1) {
        let inv;
        switch (mask) {
          case 0: inv = (x + y) % 2 === 0; break;
          case 1: inv = y % 2 === 0; break;
          case 2: inv = x % 3 === 0; break;
          case 3: inv = (x + y) % 3 === 0; break;
          case 4: inv = (Math.floor(x / 3) + Math.floor(y / 2)) % 2 === 0; break;
          case 5: inv = ((x * y) % 2) + ((x * y) % 3) === 0; break;
          case 6: inv = (((x * y) % 2) + ((x * y) % 3)) % 2 === 0; break;
          default: inv = (((x + y) % 2) + ((x * y) % 3)) % 2 === 0;
        }
        if (!this.fn[y][x] && inv) this.mod[y][x] = !this.mod[y][x];
      }
    }
  }

  penalty() {
    const n = this.size;
    const m = this.mod;
    let p = 0;
    // 同色連續與 finder 樣式（簡化：只計連續長度與 2×2 區塊、比例），足以選出合理的遮罩
    for (let y = 0; y < n; y += 1) {
      let run = 1;
      for (let x = 1; x < n; x += 1) {
        if (m[y][x] === m[y][x - 1]) {
          run += 1;
          if (run === 5) p += 3;
          else if (run > 5) p += 1;
        } else run = 1;
      }
    }
    for (let x = 0; x < n; x += 1) {
      let run = 1;
      for (let y = 1; y < n; y += 1) {
        if (m[y][x] === m[y - 1][x]) {
          run += 1;
          if (run === 5) p += 3;
          else if (run > 5) p += 1;
        } else run = 1;
      }
    }
    for (let y = 0; y < n - 1; y += 1) {
      for (let x = 0; x < n - 1; x += 1) {
        const c = m[y][x];
        if (c === m[y][x + 1] && c === m[y + 1][x] && c === m[y + 1][x + 1]) p += 3;
      }
    }
    let dark = 0;
    for (const row of m) for (const c of row) if (c) dark += 1;
    const total = n * n;
    const k = Math.ceil(Math.abs(dark * 20 - total * 10) / total) - 1;
    p += Math.max(0, k) * 10;
    return p;
  }
}

/** 文字 → QR 的模組矩陣（true 為深色）；太長時丟出錯誤 */
export function encode(text) {
  const bytes = Array.from(new TextEncoder().encode(text));
  for (let ver = 1; ver <= MAX_VERSION; ver += 1) {
    const cap = dataCodewords(ver) * 8;
    const countBits = ver <= 9 ? 8 : 16;
    const needed = 4 + countBits + bytes.length * 8;
    if (needed > cap) continue;
    const bits = [];
    const push = (val, len) => {
      for (let i = len - 1; i >= 0; i -= 1) bits.push((val >>> i) & 1);
    };
    push(0x4, 4);
    push(bytes.length, countBits);
    for (const b of bytes) push(b, 8);
    push(0, Math.min(4, cap - bits.length));
    push(0, (8 - (bits.length % 8)) % 8);
    for (let pad = 0xec; bits.length < cap; pad ^= 0xec ^ 0x11) push(pad, 8);
    const codewords = [];
    for (let i = 0; i < bits.length; i += 8) {
      let v = 0;
      for (let j = 0; j < 8; j += 1) v = (v << 1) | bits[i + j];
      codewords.push(v);
    }
    return new Qr(ver, codewords).mod;
  }
  throw new Error('QR: 內容太長');
}

/** QR code 的 SVG（4 個模組的留白）；label 為可存取名稱 */
export function qrSvg(text, label) {
  const m = encode(text);
  const n = m.length;
  const q = 4;
  const svg = s('svg', {
    class: 'k-qr',
    viewBox: `0 0 ${n + q * 2} ${n + q * 2}`,
    role: 'img',
    'aria-label': label,
    'shape-rendering': 'crispEdges',
  });
  svg.appendChild(s('rect', { class: 'k-qr__light', x: 0, y: 0, width: n + q * 2, height: n + q * 2 }));
  let d = '';
  for (let y = 0; y < n; y += 1) {
    for (let x = 0; x < n; x += 1) if (m[y][x]) d += `M${x + q} ${y + q}h1v1h-1z`;
  }
  svg.appendChild(s('path', { class: 'k-qr__dark', d }));
  return svg;
}
