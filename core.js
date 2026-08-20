// SHA3-256 (Keccak-f[1600], 0x06 domain padding) over 32-bit lane halves.
// Round constants derived from the LFSR rather than tabled by hand.
const RC_LO = new Int32Array(24), RC_HI = new Int32Array(24);
(function () {
  let lfsr = 1;
  for (let r = 0; r < 24; r++) {
    let lo = 0, hi = 0;
    for (let j = 0; j <= 6; j++) {
      const bit = (lfsr & 1) !== 0;
      lfsr = (lfsr & 0x80) ? ((lfsr << 1) ^ 0x71) & 0xff : (lfsr << 1) & 0xff;
      if (bit) {
        const pos = (1 << j) - 1;            // 0,1,3,7,15,31,63
        if (pos < 32) lo ^= (1 << pos);
        else hi ^= (1 << (pos - 32));
      }
    }
    RC_LO[r] = lo; RC_HI[r] = hi;
  }
})();

const ROT = [0,1,62,28,27,36,44,6,55,20,3,10,43,25,39,41,45,15,21,8,18,2,61,56,14];

function keccakf(s) { // s: Int32Array(50), lane i at [2i]=lo, [2i+1]=hi
  const bcLo = new Int32Array(5), bcHi = new Int32Array(5);
  const tLo = new Int32Array(25), tHi = new Int32Array(25);
  for (let round = 0; round < 24; round++) {
    // theta
    for (let x = 0; x < 5; x++) {
      let lo = 0, hi = 0;
      for (let y = 0; y < 5; y++) { lo ^= s[2*(x+5*y)]; hi ^= s[2*(x+5*y)+1]; }
      bcLo[x] = lo; bcHi[x] = hi;
    }
    for (let x = 0; x < 5; x++) {
      const p = (x + 4) % 5, n = (x + 1) % 5;
      const rLo = (bcLo[n] << 1) | (bcHi[n] >>> 31);
      const rHi = (bcHi[n] << 1) | (bcLo[n] >>> 31);
      const dLo = bcLo[p] ^ rLo, dHi = bcHi[p] ^ rHi;
      for (let y = 0; y < 5; y++) { s[2*(x+5*y)] ^= dLo; s[2*(x+5*y)+1] ^= dHi; }
    }
    // rho + pi
    for (let i = 0; i < 25; i++) {
      const n = ROT[i], lo = s[2*i], hi = s[2*i+1];
      let rl, rh;
      if (n === 0) { rl = lo; rh = hi; }
      else if (n < 32) { rl = (lo << n) | (hi >>> (32 - n)); rh = (hi << n) | (lo >>> (32 - n)); }
      else if (n === 32) { rl = hi; rh = lo; }
      else { const m = n - 32; rl = (hi << m) | (lo >>> (32 - m)); rh = (lo << m) | (hi >>> (32 - m)); }
      const x = i % 5, y = (i / 5) | 0;
      const j = y + 5 * ((2 * x + 3 * y) % 5);   // pi: A'[y, 2x+3y] = A[x, y]
      tLo[j] = rl; tHi[j] = rh;
    }
    // chi
    for (let y = 0; y < 5; y++) {
      for (let x = 0; x < 5; x++) {
        const i = x + 5*y;
        s[2*i]   = tLo[i] ^ ((~tLo[(x+1)%5 + 5*y]) & tLo[(x+2)%5 + 5*y]);
        s[2*i+1] = tHi[i] ^ ((~tHi[(x+1)%5 + 5*y]) & tHi[(x+2)%5 + 5*y]);
      }
    }
    // iota
    s[0] ^= RC_LO[round]; s[1] ^= RC_HI[round];
  }
}

function sha3_256(bytes) {
  const RATE = 136;
  const s = new Int32Array(50);
  const block = new Uint8Array(RATE);
  let off = 0;
  const absorb = () => {
    for (let i = 0; i < RATE / 8; i++) {
      let lo = 0, hi = 0;
      for (let b = 0; b < 4; b++) lo |= block[8*i+b] << (8*b);
      for (let b = 0; b < 4; b++) hi |= block[8*i+4+b] << (8*b);
      s[2*i] ^= lo; s[2*i+1] ^= hi;
    }
    keccakf(s);
  };
  while (off + RATE <= bytes.length) { block.set(bytes.subarray(off, off + RATE)); absorb(); off += RATE; }
  const rem = bytes.length - off;
  block.fill(0);
  block.set(bytes.subarray(off));
  block[rem] ^= 0x06;
  block[RATE - 1] ^= 0x80;
  absorb();
  let hex = '';
  for (let i = 0; i < 4; i++) {
    for (const w of [s[2*i], s[2*i+1]]) {
      for (let b = 0; b < 4; b++) hex += ((w >>> (8*b)) & 0xff).toString(16).padStart(2, '0');
    }
  }
  return hex;
}

// ---- setsum: eight u32 columns, one prime each (ANDON.md §2) --------------
const PRIMES = [4294967291, 4294967279, 4294967231, 4294967197,
                4294967189, 4294967161, 4294967143, 4294967111];

const zero = () => [0,0,0,0,0,0,0,0];

function stateOf(bytes) {                 // sha3, read as 8 LE u32, reduce
  const h = sha3_256(bytes);
  const cols = [];
  for (let i = 0; i < 8; i++) {
    let v = 0;
    for (let b = 3; b >= 0; b--) v = v * 256 + parseInt(h.slice(8*i + 2*b, 8*i + 2*b + 2), 16);
    cols.push(v % PRIMES[i]);
  }
  return cols;
}
const addS = (a, b) => a.map((x, i) => (x + b[i]) % PRIMES[i]);
const negS = (a) => a.map((x, i) => (PRIMES[i] - x) % PRIMES[i]);
const sumHex = (s) => s.map(x => {
  let h = '';
  for (let b = 0; b < 4; b++) h += (Math.floor(x / 2**(8*b)) & 0xff).toString(16).padStart(2, '0');
  return h;
}).join('');

module.exports = { sha3_256, stateOf, addS, negS, zero, sumHex, PRIMES };
