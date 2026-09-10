// Generates src-tauri/icons/icon.ico (+ PNG sizes) without native image deps.
// Minimal PNG encoder: RGBA scanlines, filter 0, zlib deflate, manual CRC32.
import zlib from "node:zlib";
import fs from "node:fs";
import path from "node:path";

const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (const b of buf) c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

function encodePng(size, rgba) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  const stride = size * 4;
  const raw = Buffer.alloc((stride + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (stride + 1)] = 0; // filter: none
    rgba.copy(raw, y * (stride + 1) + 1, y * stride, (y + 1) * stride);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", zlib.deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// ---- drawing ----

function clamp01(v) { return Math.max(0, Math.min(1, v)); }

function smoothEdge(d, w = 1.0) {
  return clamp01(0.5 - d / w);
}

/** rounded-rect signed distance (positive outside) */
function sdRoundRect(px, py, cx, cy, hw, hh, r) {
  const dx = Math.abs(px - cx) - (hw - r);
  const dy = Math.abs(py - cy) - (hh - r);
  const ax = Math.max(dx, 0), ay = Math.max(dy, 0);
  return Math.sqrt(ax * ax + ay * ay) + Math.min(Math.max(dx, dy), 0) - r;
}

/** signed distance to segment ab */
function sdSegment(px, py, ax, ay, bx, by) {
  const abx = bx - ax, aby = by - ay;
  const apx = px - ax, apy = py - ay;
  const t = clamp01((apx * abx + apy * aby) / (abx * abx + aby * aby));
  const qx = ax + abx * t - px, qy = ay + aby * t - py;
  return Math.sqrt(qx * qx + qy * qy);
}

function hex(c) {
  return [(c >> 16) & 255, (c >> 8) & 255, c & 255];
}

const BG_DARK = hex(0x0d121b);
const BG_DARK2 = hex(0x121926);
const ACCENT = hex(0x4cc2ff);
const VIOLET = hex(0xa78bfa);
const BORDER = hex(0x2e3c58);

function drawIcon(size) {
  const rgba = Buffer.alloc(size * size * 4);
  const s = size / 48; // design grid 48x48
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const px = (x + 0.5) / s;
      const py = (y + 0.5) / s;
      const idx = (y * size + x) * 4;

      // background rounded rect (2..46)
      const dBg = sdRoundRect(px, py, 24, 24, 22, 22, 11);
      let alpha = smoothEdge(dBg, 1.2);
      if (alpha <= 0) continue;

      // subtle vertical gradient background
      const t = clamp01((py - 2) / 44);
      let r = BG_DARK[0] + (BG_DARK2[0] - BG_DARK[0]) * t;
      let g = BG_DARK[1] + (BG_DARK2[1] - BG_DARK[1]) * t;
      let b = BG_DARK[2] + (BG_DARK2[2] - BG_DARK[2]) * t;

      // border
      const dBorder = Math.abs(dBg + 1.2);
      if (dBorder < 1.1) {
        r = BORDER[0]; g = BORDER[1]; b = BORDER[2];
      }

      // chevrons: two ">" strokes
      const w = 3.6 * 1.0; // stroke half-width in grid units
      const d1 = Math.min(
        sdSegment(px, py, 13, 15, 23, 24),
        sdSegment(px, py, 23, 24, 13, 33),
      );
      const d2 = Math.min(
        sdSegment(px, py, 26, 15, 36, 24),
        sdSegment(px, py, 36, 24, 26, 33),
      );
      const cov1 = smoothEdge(d1 - w + 0.5, 1.1);
      const cov2 = smoothEdge(d2 - w + 0.5, 1.1);

      // composite accent chevron
      r = r * (1 - cov1) + ACCENT[0] * cov1;
      g = g * (1 - cov1) + ACCENT[1] * cov1;
      b = b * (1 - cov1) + ACCENT[2] * cov1;
      // dim violet chevron
      const vAlpha = cov2 * 0.5;
      r = r * (1 - vAlpha) + VIOLET[0] * vAlpha;
      g = g * (1 - vAlpha) + VIOLET[1] * vAlpha;
      b = b * (1 - vAlpha) + VIOLET[2] * vAlpha;

      rgba[idx] = Math.round(r);
      rgba[idx + 1] = Math.round(g);
      rgba[idx + 2] = Math.round(b);
      rgba[idx + 3] = Math.round(alpha * 255);
    }
  }
  return rgba;
}

// ---- ICO assembly (PNG-embedded entries) ----

function buildIco(sizes) {
  const pngs = sizes.map((sz) => ({ sz, png: encodePng(sz, drawIcon(sz)) }));
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2); // type: icon
  header.writeUInt16LE(pngs.length, 4);
  const dirSize = 16 * pngs.length;
  let offset = 6 + dirSize;
  const entries = [];
  for (const { sz, png } of pngs) {
    const e = Buffer.alloc(16);
    e[0] = sz >= 256 ? 0 : sz; // width
    e[1] = sz >= 256 ? 0 : sz; // height
    e[2] = 0; // palette
    e[3] = 0; // reserved
    e.writeUInt16LE(1, 4); // planes
    e.writeUInt16LE(32, 6); // bpp
    e.writeUInt32LE(png.length, 8);
    e.writeUInt32LE(offset, 12);
    offset += png.length;
    entries.push(e);
  }
  return Buffer.concat([header, ...entries, ...pngs.map((p) => p.png)]);
}

const outDir = path.resolve("src-tauri/icons");
fs.mkdirSync(outDir, { recursive: true });

fs.writeFileSync(path.join(outDir, "icon.ico"), buildIco([16, 32, 48, 256]));

// PNGs for potential other bundle targets / README
for (const sz of [32, 128, 256, 512]) {
  fs.writeFileSync(path.join(outDir, `${sz}x${sz}.png`), encodePng(sz, drawIcon(sz)));
}

console.log("icons written to", outDir);
