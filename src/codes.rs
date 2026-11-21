//! A pair of Huffman codes in the form the encoder reads them, and the block
//! header that announces them.

use crate::huff::{codewords, make_lens};
use crate::tables::*;

/// Room for the longest possible header of a dynamic block: 17 bits, 19
/// three-bit lengths, and 316 code lengths of at most 14 bits each.
pub(crate) const HDR_BYTES: usize = 576;

/// `Codes::lit` entry of a byte that has no codeword.  As a length (the high
/// half) it sets bit 15 of the encoder's bit count, which is how the encoder
/// notices, without a test per literal.
pub(crate) const LIT_NONE: u32 = 0x8000_0000;

/// Fields of a `Codes::len` entry.
pub(crate) const LEN_VALUE_MASK: u32 = (1 << 19) - 1;
pub(crate) const LEN_NBITS_SHIFT: u32 = 19;
pub(crate) const LEN_SLOT_SHIFT: u32 = 24;

/// Each table is indexed by what the encoder has in hand (a byte, a match
/// length, an offset slot) and gives everything to be written for it in one
/// 32-bit load.  libdeflate keeps codewords and lengths in separate arrays,
/// and looks the length slot up in a third: three loads where this has one.
#[repr(C)]
pub(crate) struct Codes {
    /// Byte -> codeword | number of bits << 16, or `LIT_NONE`.
    pub lit: [u32; 256],
    /// Match length -> the codeword of its length symbol with the extra bits
    /// already appended (at most 14 + 5 bits) | their number << 19 | the index
    /// of the length symbol << 24.  0 if the symbol has no codeword.
    pub len: [u32; MAX_MATCH + 1],
    /// End of block: codeword | number of bits << 16.
    pub eob: u32,
    /// Offset slot -> codeword | number of bits << 16.  0 without a codeword.
    pub off: [u32; NUM_OFF],
    /// The block header (BFINAL = 1, BTYPE, and for a dynamic block the
    /// description of both codes), ready to be copied: `hdr_bits` bits.
    pub hdr_bits: u32,
    pub hdr: [u8; HDR_BYTES],
    pub ll_lens: [u8; NUM_LL],
    pub off_lens: [u8; NUM_OFF],
}

impl Codes {
    /// Codes for the given symbol counts, with their header and tables.
    pub(crate) fn build(&mut self, f_ll: &[u32; NUM_LL], f_off: &[u32; NUM_OFF]) {
        make_lens(&f_ll[..USED_LL], &mut self.ll_lens[..USED_LL], MAX_LL_BITS);
        make_lens(&f_off[..USED_OFF], &mut self.off_lens[..USED_OFF], MAX_OFF_BITS);
        self.write_header();
        self.fill();
    }

    /// `lit`, `len`, `eob` and `off` from `ll_lens` and `off_lens`.
    pub(crate) const fn fill(&mut self) {
        let mut cw = [0u16; NUM_LL];
        codewords(&self.ll_lens, NUM_LL, &mut cw);
        let mut b = 0;
        while b < 256 {
            let l = self.ll_lens[b] as u32;
            self.lit[b] = if l == 0 { LIT_NONE } else { cw[b] as u32 | l << 16 };
            b += 1;
        }
        self.eob = cw[EOB] as u32 | (self.ll_lens[EOB] as u32) << 16;
        let mut len = MIN_MATCH;
        while len <= MAX_MATCH {
            let slot = LEN_SLOT[len] as usize;
            let l = self.ll_lens[FIRST_LEN + slot] as u32;
            let extra = (len - LEN_BASE[slot] as usize) as u32;
            self.len[len] = if l == 0 {
                0
            } else {
                (cw[FIRST_LEN + slot] as u32 | extra << l)
                    | (l + LEN_EXTRA[slot] as u32) << LEN_NBITS_SHIFT
                    | (slot as u32) << LEN_SLOT_SHIFT
            };
            len += 1;
        }
        let mut cw = [0u16; NUM_OFF];
        codewords(&self.off_lens, NUM_OFF, &mut cw);
        let mut s = 0;
        while s < NUM_OFF {
            let l = self.off_lens[s] as u32;
            self.off[s] = if l == 0 { 0 } else { cw[s] as u32 | l << 16 };
            s += 1;
        }
    }

    /// The header of a final dynamic block with these codes, into `hdr`.
    pub(crate) fn write_header(&mut self) {
        let mut hlit = USED_LL;
        while hlit > 257 && self.ll_lens[hlit - 1] == 0 {
            hlit -= 1;
        }
        let mut hdist = USED_OFF;
        while hdist > 1 && self.off_lens[hdist - 1] == 0 {
            hdist -= 1;
        }
        let mut lens = [0u8; USED_LL + USED_OFF];
        lens[..hlit].copy_from_slice(&self.ll_lens[..hlit]);
        lens[hlit..hlit + hdist].copy_from_slice(&self.off_lens[..hdist]);
        let lens = &lens[..hlit + hdist];

        // Run-length code the lengths: an item is a precode symbol (a length,
        // or 16, 17, 18 for a run) | its extra bits << 8.
        let mut items = [0u16; USED_LL + USED_OFF];
        let mut n_items = 0;
        let mut freq = [0u32; 19];
        let mut put = |sym: usize, extra: usize| {
            items[n_items] = (sym | extra << 8) as u16;
            n_items += 1;
            freq[sym] += 1;
        };
        let mut i = 0;
        while i < lens.len() {
            let l = lens[i] as usize;
            let mut run = 1;
            while i + run < lens.len() && lens[i + run] as usize == l {
                run += 1;
            }
            i += run;
            if l == 0 {
                while run >= 11 {
                    let r = run.min(138);
                    put(18, r - 11);
                    run -= r;
                }
                if run >= 3 {
                    put(17, run - 3);
                    run = 0;
                }
            } else {
                put(l, 0);
                run -= 1;
                while run >= 3 {
                    let r = run.min(6);
                    put(16, r - 3);
                    run -= r;
                }
            }
            for _ in 0..run {
                put(l, 0);
            }
        }

        let mut pre_lens = [0u8; 19];
        make_lens(&freq, &mut pre_lens, MAX_PRE_BITS);
        let mut pre_cw = [0u16; 19];
        codewords(&pre_lens, 19, &mut pre_cw);
        let mut hclen = 19;
        while hclen > 4 && pre_lens[PRE_ORDER[hclen - 1] as usize] == 0 {
            hclen -= 1;
        }

        let mut w = HeaderBits { out: &mut self.hdr, bytes: 0, buf: 0, n: 0 };
        w.put(1, 1); // BFINAL
        w.put(2, 2); // BTYPE: dynamic Huffman codes
        w.put(hlit as u32 - 257, 5);
        w.put(hdist as u32 - 1, 5);
        w.put(hclen as u32 - 4, 4);
        for &sym in &PRE_ORDER[..hclen] {
            w.put(pre_lens[sym as usize] as u32, 3);
        }
        for &item in &items[..n_items] {
            let sym = (item & 0xFF) as usize;
            w.put(pre_cw[sym] as u32, pre_lens[sym] as u32);
            match sym {
                16 => w.put((item >> 8) as u32, 2),
                17 => w.put((item >> 8) as u32, 3),
                18 => w.put((item >> 8) as u32, 7),
                _ => {}
            }
        }
        self.hdr_bits = w.finish();
    }
}

struct HeaderBits<'a> {
    out: &'a mut [u8; HDR_BYTES],
    bytes: usize,
    buf: u32,
    n: u32,
}

impl HeaderBits<'_> {
    fn put(&mut self, bits: u32, n: u32) {
        self.buf |= bits << self.n;
        self.n += n;
        while self.n >= 8 {
            self.out[self.bytes] = self.buf as u8;
            self.bytes += 1;
            self.buf >>= 8;
            self.n -= 8;
        }
    }

    /// The number of bits written.  The last byte is written even when it
    /// holds no bit, so that it can always be read.
    fn finish(self) -> u32 {
        self.out[self.bytes] = self.buf as u8;
        self.bytes as u32 * 8 + self.n
    }
}

/// The fixed codes of the format (BTYPE = 1), which need no description in
/// the block.  For payloads too small to pay for a header.
pub(crate) static FIXED: Codes = {
    let mut c = Codes {
        lit: [0; 256],
        len: [0; MAX_MATCH + 1],
        eob: 0,
        off: [0; NUM_OFF],
        hdr_bits: 3,
        hdr: [0; HDR_BYTES],
        ll_lens: [0; NUM_LL],
        off_lens: [5; NUM_OFF],
    };
    let mut sym = 0;
    while sym < NUM_LL {
        c.ll_lens[sym] = if sym < 144 {
            8
        } else if sym < 256 {
            9
        } else if sym < 280 {
            7
        } else {
            8
        };
        sym += 1;
    }
    c.hdr[0] = 0b011; // BFINAL, BTYPE = 1
    c.fill();
    c
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_code() {
        // RFC 1951, 3.2.6: 0..=143 are 00110000.., 256..=279 are 0000000..
        assert_eq!(FIXED.lit[0], (0b0011_0000u8.reverse_bits() as u32) | 8 << 16);
        assert_eq!(FIXED.lit[144], ((0b1_1001_0000u16.reverse_bits() >> 7) as u32) | 9 << 16);
        assert_eq!(FIXED.eob, 7 << 16);
        // Length 3 is symbol 257: 0000001, no extra bits.
        assert_eq!(FIXED.len[3], 0b100_0000 | 7 << LEN_NBITS_SHIFT);
        // Length 258 is symbol 285: 11000101, no extra bits.
        assert_eq!(
            FIXED.len[258],
            0b1100_0101u8.reverse_bits() as u32 | 8 << LEN_NBITS_SHIFT | 28 << LEN_SLOT_SHIFT
        );
        // Length 12 is symbol 265 (0001001) with one extra bit of 1.
        assert_eq!(
            FIXED.len[12],
            (0b100_1000 | 1 << 7) | 8 << LEN_NBITS_SHIFT | 8 << LEN_SLOT_SHIFT
        );
        assert_eq!(FIXED.off[1], (0b00001u8.reverse_bits() >> 3) as u32 | 5 << 16);
    }
}
