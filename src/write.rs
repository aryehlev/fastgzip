//! Writing a block of levels 1 to 9: the matches a search found, with the
//! literals between them, as DEFLATE bits.
//!
//! The work is the same as `enc::emit_seqs` does for level 0; what differs
//! is what suits a block whose codes were made for it and that may begin in
//! the middle of a byte, and that the search has already worked out, for
//! each match, the symbol and the extra bits of its distance.

use crate::codes::*;
use crate::tables::*;
use core::ptr;

/// A match and the literals before it: the number of literals in the low 32
/// bits, then 9 bits of length, 15 of the distance less 1, and the
/// distance's symbol.
pub(crate) type Token = u64;

const LEN_SHIFT: u32 = 32;
const DIST_SHIFT: u32 = 41;
const SLOT_SHIFT: u32 = 56;

/// The token of a match of `len` bytes, `dist` bytes back, after `run`
/// literals; and the distance's symbol.
#[inline(always)]
pub(crate) fn token(run: usize, len: usize, dist: usize) -> (Token, usize) {
    debug_assert!(run <= u32::MAX as usize);
    debug_assert!((MIN_MATCH..=MAX_MATCH).contains(&len) && (1..=32768).contains(&dist));
    // The symbols come in pairs that halve each power of two: twice the
    // number of the highest bit, and the bit below it.
    let d = dist as u32 - 1;
    let slot = if d < 2 {
        d
    } else {
        let top = 31 - d.leading_zeros();
        2 * top + (d >> (top - 1) & 1)
    };
    let t = run as u64 | (len as u64) << LEN_SHIFT | (d as u64) << DIST_SHIFT | (slot as u64) << SLOT_SHIFT;
    (t, slot as usize)
}

/// Distance symbol -> the number of extra bits that follow its codeword
/// (`OFF_EXTRA`, with an entry for every value five bits can have).
static EXTRA_BITS: [u8; NUM_OFF] = {
    let mut t = [0u8; NUM_OFF];
    let mut s = 0;
    while s < USED_OFF {
        t[s] = OFF_EXTRA[s];
        s += 1;
    }
    t
};

/// Number of literals, 0 to 3 -> what to keep of the table entries of the
/// three bytes at hand: all of an entry, or nothing.
static KEEP: [[u32; 4]; 4] = [[0, 0, 0, 0], [!0, 0, 0, 0], [!0, !0, 0, 0], [!0, !0, !0, 0]];

/// The length of a token's match.
#[inline(always)]
pub(crate) fn token_len(t: Token) -> usize {
    (t >> LEN_SHIFT & 0x1FF) as usize
}

#[inline(always)]
unsafe fn wr64(p: *mut u8, i: usize, cap: usize, v: u64) {
    debug_assert!(i + 8 <= cap);
    let _ = cap;
    ptr::write_unaligned(p.add(i) as *mut u64, v.to_le())
}

/// Writes the block `ip[..n]`, whose matches are `tokens`, at bit `bitpos`
/// of `out`, with the codes `c`: header, symbols, end of block.  Returns the
/// new bit position.
///
/// `c` has a codeword for every symbol of the block.  `out` has room for
/// the bits and for 136 bytes after where they begin: the header is copied
/// 128 bytes at a time, and bits are stored eight bytes at a time.
///
/// The literals before a match are written four at a time, which leaves up
/// to three.  With `ASKING` a match is first asked whether it has literals
/// before it at all; without, nothing is asked, and what is not a literal
/// is masked out.  Which is quicker depends on the machine and on the
/// payload (see `levels::asking`).
#[inline(never)]
#[allow(clippy::too_many_arguments, unused_assignments)]
pub(crate) unsafe fn write_block<const ASKING: bool>(
    c: &Codes,
    ip: *const u8,
    n: usize,
    tokens: &[Token],
    out: *mut u8,
    cap: usize,
    bitpos: usize,
    last: bool,
) -> usize {
    let lit = c.lit.as_ptr();
    let mut o = bitpos >> 3;
    let used = (bitpos & 7) as u32;
    let (mut bitbuf, mut bitcount): (u64, u32);

    // After a flush fewer than eight bits are left in the buffer.
    macro_rules! flush {
        () => {
            wr64(out, o, cap, bitbuf);
            o += (bitcount >> 3) as usize;
            bitbuf >>= bitcount & 56;
            bitcount &= 7;
        };
    }
    macro_rules! literal {
        ($i:expr) => {{
            debug_assert!($i < n);
            let e = *lit.add(*ip.add($i) as usize);
            bitbuf |= ((e & 0xFFFF) as u64) << bitcount;
            bitcount += e >> 16;
        }};
    }

    let nb = c.hdr_bits as usize;
    if used == 0 {
        // The header as it was stored: whole bytes, a fixed number of them
        // where that is enough (it is copied faster), then the bits of the
        // last one.  What is copied beyond the header is overwritten.
        let full = nb >> 3;
        debug_assert!(o + full.max(128) <= cap);
        if full < 128 {
            ptr::copy_nonoverlapping(c.hdr.as_ptr(), out.add(o), 128);
        } else {
            ptr::copy_nonoverlapping(c.hdr.as_ptr(), out.add(o), full);
        }
        bitbuf = c.hdr[full] as u64 & ((1 << (nb & 7)) - 1);
        bitcount = (nb & 7) as u32;
        if !last {
            // The stored header says BFINAL = 1.
            if full == 0 {
                bitbuf &= !1;
            } else {
                *out.add(o) &= !1;
            }
        }
        o += full;
    } else {
        // In the middle of a byte: a byte of the header at a time.  Only a
        // payload of more than one block comes here.
        bitbuf = *out.add(o) as u64 & ((1 << used) - 1);
        bitcount = used;
        let mut i = 0;
        while 8 * i < nb {
            let take = (nb - 8 * i).min(8) as u32;
            let mut b = c.hdr[i] as u64 & ((1 << take) - 1);
            if i == 0 && !last {
                b &= !1;
            }
            bitbuf |= b << bitcount;
            bitcount += take;
            flush!();
            i += 1;
        }
    }

    let mut p = 0usize;
    for &t in tokens {
        let run = t as u32 as usize;
        let len = token_len(t);
        debug_assert!(p + run + len <= n);
        // Most matches of a long payload come straight after another; in a
        // short one few do.  Either way there is something to predict.
        if !ASKING {
            let end = p + run;
            while p + 4 <= end {
                literal!(p);
                literal!(p + 1);
                literal!(p + 2);
                literal!(p + 3);
                flush!();
                p += 4;
            }
            let left = end - p;
            let e0 = *lit.add(*ip.add(p) as usize) & ((left > 0) as u32).wrapping_neg();
            let e1 = *lit.add(*ip.add(p + 1) as usize) & ((left > 1) as u32).wrapping_neg();
            let e2 = *lit.add(*ip.add(p + 2) as usize) & ((left > 2) as u32).wrapping_neg();
            bitbuf |= ((e0 & 0xFFFF) as u64) << bitcount;
            bitcount += e0 >> 16;
            bitbuf |= ((e1 & 0xFFFF) as u64) << bitcount;
            bitcount += e1 >> 16;
            bitbuf |= ((e2 & 0xFFFF) as u64) << bitcount;
            bitcount += e2 >> 16;
            flush!();
            p = end;
        } else if run != 0 {
            let end = p + run;
            // Four literals fit in the buffer with what a flush leaves.
            while p + 4 <= end {
                literal!(p);
                literal!(p + 1);
                literal!(p + 2);
                literal!(p + 3);
                flush!();
                p += 4;
            }
            // Up to three more.  How many is hard to predict, so there is
            // no branch on it: three bytes are looked up (they are there,
            // the match is at least that long), and those that are not
            // literals add no bits.  What keeps them out is loaded from a
            // table: a comparison here the compiler turns into the branch.
            let keep = KEEP.as_ptr().add(end - p) as *const u32;
            let e0 = *lit.add(*ip.add(p) as usize) & *keep;
            let e1 = *lit.add(*ip.add(p + 1) as usize) & *keep.add(1);
            let e2 = *lit.add(*ip.add(p + 2) as usize) & *keep.add(2);
            bitbuf |= ((e0 & 0xFFFF) as u64) << bitcount;
            bitcount += e0 >> 16;
            bitbuf |= ((e1 & 0xFFFF) as u64) << bitcount;
            bitcount += e1 >> 16;
            bitbuf |= ((e2 & 0xFFFF) as u64) << bitcount;
            bitcount += e2 >> 16;
            flush!();
            p = end;
        }

        // The match: at most 14 + 5 bits of length and 15 + 13 of distance.
        // The extra bits of a distance are its low ones.
        let slot = (t >> SLOT_SHIFT) as usize;
        let le = *c.len.as_ptr().add(len);
        let oe = *c.off.as_ptr().add(slot);
        debug_assert!(le != 0 && oe >> 16 != 0, "a length or distance without a codeword");
        let len_n = le >> LEN_NBITS_SHIFT & 31;
        let off_n = oe >> 16;
        let extra_n = *EXTRA_BITS.as_ptr().add(slot) as u32;
        let extra = (t >> DIST_SHIFT) & !(!0u64 << extra_n);
        bitbuf |= ((le & LEN_VALUE_MASK) as u64
            | ((oe & 0xFFFF) as u64) << len_n
            | extra << (len_n + off_n))
            << bitcount;
        bitcount += len_n + off_n + extra_n;
        flush!();
        p += len;
    }
    // The literals after the last match.
    while p + 4 <= n {
        literal!(p);
        literal!(p + 1);
        literal!(p + 2);
        literal!(p + 3);
        flush!();
        p += 4;
    }
    while p < n {
        literal!(p);
        p += 1;
    }
    flush!();
    debug_assert!(c.eob >> 16 != 0);
    bitbuf |= ((c.eob & 0xFFFF) as u64) << bitcount;
    bitcount += c.eob >> 16;
    flush!();
    8 * o + bitcount as usize
}
