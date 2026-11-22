//! The encoder: finds repetitions in one block of at most `CHUNK` bytes and
//! writes it as DEFLATE bits.
//!
//! `scan` is the function that matters.  With `EMIT` it writes the block in a
//! single pass, using codes that were made before the payload was seen.
//! Without `EMIT` it only records the matches, for `emit_seqs` to write once
//! codes have been chosen: that is the path for a payload the shared codes do
//! not suit, and for small ones.

use crate::codes::*;
use crate::tables::*;
use core::ptr;

/// The most bytes a block covers.  Positions are kept in 16 bits, and the
/// largest table (one entry per position of a block) is 32 KiB.
pub(crate) const CHUNK: usize = 16384;
pub(crate) const MAX_TAB_BITS: u32 = 14;
/// A match is at least four bytes long.
pub(crate) const MAX_SEQS: usize = CHUNK / 4 + 1;

/// The bytes a block of `n` input bytes may write before it is known how
/// large it is: the header, at most 14 bits per byte, and what a flush may
/// write past the last bit.
pub(crate) const fn scratch(n: usize) -> usize {
    HDR_BYTES + 2 * n + 24
}

/// The hash table: for each hash value, the last position whose next five
/// bytes had it.
#[repr(C, align(64))]
pub(crate) struct Tab(pub [u16; 1 << MAX_TAB_BITS]);

impl Tab {
    /// Makes the positions relative to a start that is `CHUNK` bytes later.
    /// What was before the new start now reads as the new start, which is as
    /// good a candidate as any: a candidate is only ever a guess.
    pub(crate) fn rebase(&mut self) {
        for e in self.0.iter_mut() {
            *e = e.saturating_sub(CHUNK as u16); // 8 or 16 entries per instruction
        }
    }
}

/// Symbol counts of one block.  16 bits each: a block has at most 16384
/// symbols, and half the size means half the cache lines to touch.
#[repr(C)]
pub(crate) struct Freq {
    pub ll: [u16; NUM_LL],
    pub off: [u16; NUM_OFF],
}

/// A match and the literals before it: run | length << 16 | distance << 32.
pub(crate) type Seq = u64;

/// Where a block's bits go, and what goes around its symbols.
#[derive(Clone, Copy)]
pub(crate) struct Place {
    /// Write the block header.  The output is then at a byte boundary.
    pub header: bool,
    /// The header says that this is the last block of the stream.
    pub last: bool,
    /// Write the end-of-block symbol.
    pub eob: bool,
    /// Without a header: the bits of the first output byte that are in use
    /// (0 to 7).  The block continues one that was begun before.
    pub bit: u32,
}

impl Place {
    /// A whole block: header, symbols, end of block.
    pub(crate) const fn whole(last: bool) -> Self {
        Place { header: true, last, eob: true, bit: 0 }
    }
}

#[inline(always)]
unsafe fn rd64(p: *const u8, i: usize, n: usize) -> u64 {
    debug_assert!(i + 8 <= n);
    u64::from_le(ptr::read_unaligned(p.add(i) as *const u64))
}

#[inline(always)]
unsafe fn wr64(p: *mut u8, i: usize, cap: usize, v: u64) {
    debug_assert!(i + 8 <= cap);
    ptr::write_unaligned(p.add(i) as *mut u64, v.to_le())
}

/// Hash of the low five bytes of `x`, in `BITS` bits.
///
/// `(x << 24) * K` is `x * (K << 24)`: the shift that drops the upper three
/// bytes is folded into the constant, leaving one multiplication and one
/// shift.  The shift amount is a constant in each copy of the encoder.
#[inline(always)]
fn hash5<const BITS: u32>(x: u64) -> usize {
    const K24: u64 = 0x9E37_79B1_85EB_CA87 << 24;
    (x.wrapping_mul(K24) >> (64 - BITS)) as usize
}

/// Offset slot, number of extra bits and their value for a distance, given
/// `d` = distance - 1.  Arithmetic on the bit length of `d` instead of
/// libdeflate's 512-byte table: no load, and nothing to fall out of cache.
#[inline(always)]
pub(crate) fn off_slot(d: u32) -> (usize, u32, u32) {
    let bl = 32 - d.leading_zeros();
    let extra = bl.saturating_sub(2);
    let slot = if d < 2 { d } else { 2 * bl - 2 + ((d >> (bl - 2)) & 1) };
    (slot as usize, extra, d & ((1 << extra) - 1))
}

/// The bits of a match of `len` bytes at distance `dist`, and how many.
/// One value of at most 47 bits: the length codeword with its extra bits
/// comes out of one table load, the offset codeword out of another, and the
/// offset's extra bits are arithmetic.  Also the indexes of the two symbols.
#[inline(always)]
unsafe fn match_bits(c: &Codes, len: usize, dist: usize) -> (u64, u32, u32, usize) {
    debug_assert!((MIN_MATCH..=MAX_MATCH).contains(&len) && (1..=32768).contains(&dist));
    let (slot, extra_n, extra) = off_slot(dist as u32 - 1);
    let le = *c.len.as_ptr().add(len);
    let oe = *c.off.as_ptr().add(slot);
    debug_assert!(le != 0 && oe != 0, "a length or offset without a codeword");
    let len_n = (le >> LEN_NBITS_SHIFT) & 31;
    let off_n = oe >> 16;
    let bits = (le & LEN_VALUE_MASK) as u64
        | ((oe & 0xFFFF) as u64) << len_n
        | (extra as u64) << (len_n + off_n);
    (bits, len_n + off_n + extra_n, le >> LEN_SLOT_SHIFT, slot)
}

/// Begins a block at `out` and returns where its bits continue: (bytes
/// written, bit buffer, bits in it).
#[inline(always)]
unsafe fn start_block(c: &Codes, out: *mut u8, cap: usize, place: Place) -> (usize, u64, u32) {
    if !place.header {
        // Continue in the middle of a byte.  Above its bits in use it may
        // hold anything.
        debug_assert!(place.bit < 8 && cap >= 1);
        return (0, *out as u64 & ((1 << place.bit) - 1), place.bit);
    }
    let nb = c.hdr_bits as usize;
    let full = nb >> 3;
    debug_assert!(cap >= HDR_BYTES);
    if full < 128 {
        // The usual header is 60 to 90 bytes.  A copy of a fixed size is a
        // few vector moves; one of `full` bytes is a call that first has to
        // find out how long it is.  What is copied beyond the header is
        // overwritten by the block.
        ptr::copy_nonoverlapping(c.hdr.as_ptr(), out, 128);
    } else {
        ptr::copy_nonoverlapping(c.hdr.as_ptr(), out, full);
    }
    let mut bitbuf = c.hdr[full] as u64 & ((1 << (nb & 7)) - 1);
    if !place.last {
        // The stored header says BFINAL = 1.
        if full == 0 {
            bitbuf &= !1;
        } else {
            *out &= !1;
        }
    }
    (full, bitbuf, (nb & 7) as u32)
}

/// What a flush leaves of the bit count: the bits of the unfinished byte,
/// and everything from bit 15 up, where a missing codeword leaves its mark.
const KEEP: u32 = !0x7FF8;

/// One block: `win[start..]` (at most `CHUNK` bytes) either written to `out`
/// with the codes `c` (`EMIT`), or recorded in `seqs` (not `EMIT`).  With
/// `COUNT` the symbols are counted in `freq`, which the caller has zeroed.
///
/// `win[..start]` is what came before the block in the same payload, where
/// matches may begin: nothing, or the `CHUNK` bytes before it.  The table
/// then still holds those positions, unless `clear` says to forget them.
/// A block that begins at 0 always begins with an empty table.
///
/// With `EMIT`, returns the number of bits now in use at `out` (counting
/// `place.bit`), or 0 if a byte of the block has no codeword in `c`; `out`
/// must have room for `scratch(block length)` bytes, and `c` must have a
/// codeword for every length and for every offset slot the window allows.
/// Without `EMIT`, returns the number of `seqs`.
///
/// How it searches:
///
///   - One earlier position is kept per hash value, in a table with about
///     one entry per input byte (`1 << BITS`).  The table is zeroed at the
///     start of every payload.  That costs little at this size and walks the
///     table front to back, which fetches it much faster than the scattered
///     first touches of the lookups would when the thread has done other
///     work since its last payload.
///   - A position is an offset into the window, so an empty entry reads as
///     position 0.  The bytes there are compared like those of any other
///     candidate: no entry is tested for being empty, and the only branch in
///     a step is the one taken for a match.
///   - Each step reads eight bytes at the current position once.  The low
///     byte is the literal, the low four bytes are compared with the
///     candidate's, and bytes 1 to 5 are hashed for the next position, so
///     that the next lookup's address is ready before the next step begins.
///   - Eight bytes are read at the candidate too, although four decide.  When
///     they decide for a match, the XOR that was computed for the comparison
///     already says how long a match of up to 7 bytes is.
///   - Inside a match only three positions are entered into the table: the
///     two after its start, whose hashes come out of the word already read,
///     and the last, whose word also gives the hash for the position after
///     the match.  One load per match instead of four.
///
/// How it writes:
///
///   - A literal is one table load and an OR into a 64-bit buffer.  Four
///     literals always fit (see `MAX_LL_BITS`), so the steps come four to a
///     flush, and a flush is one unaligned 8-byte store.
///   - A byte without a codeword is not tested for where it is written: its
///     table entry adds 2^15 to the bit count.  Shifts use the count modulo
///     64 and a flush keeps everything from bit 15 up, so the mark is still
///     there at the end, where it is looked at once.
///   - A match is assembled into one value (`match_bits`) and OR-ed in once.
#[inline(never)]
#[allow(clippy::too_many_arguments, unused_assignments)]
pub(crate) unsafe fn scan<const BITS: u32, const EMIT: bool, const COUNT: bool>(
    c: &Codes,
    tab: &mut Tab,
    freq: &mut Freq,
    seqs: &mut [Seq; MAX_SEQS],
    win: &[u8],
    start: usize,
    clear: bool,
    out: *mut u8,
    cap: usize,
    place: Place,
) -> usize {
    let n = win.len();
    debug_assert!(start <= n && n - start <= CHUNK && n <= 2 * CHUNK && BITS <= MAX_TAB_BITS);
    debug_assert!(start == 0 || (BITS == MAX_TAB_BITS && start == CHUNK));
    debug_assert!(clear || start != 0);
    debug_assert!(!EMIT || cap >= scratch(n - start));
    let ip = win.as_ptr();
    let tp = tab.0.as_mut_ptr();
    let lit_tab = c.lit.as_ptr();
    let f_ll = freq.ll.as_mut_ptr();
    let f_off = freq.off.as_mut_ptr();
    let sp = seqs.as_mut_ptr();

    let (mut o, mut bitbuf, mut bitcount) = (0usize, 0u64, 0u32);
    if EMIT {
        (o, bitbuf, bitcount) = start_block(c, out, cap, place);
    }
    let mut n_seqs = 0usize;
    let mut run_start = start; // where the literals since the last match begin

    macro_rules! flush {
        () => {
            if EMIT {
                wr64(out, o, cap, bitbuf);
                o += ((bitcount >> 3) & 7) as usize;
                bitbuf >>= bitcount & 56;
                bitcount &= KEEP;
            }
        };
    }
    macro_rules! literal {
        ($byte:expr) => {{
            let b = $byte as usize;
            if EMIT {
                let e = *lit_tab.add(b);
                bitbuf |= ((e & 0xFFFF) as u64).wrapping_shl(bitcount);
                bitcount = bitcount.wrapping_add(e >> 16);
            }
            if COUNT {
                *f_ll.add(b) = (*f_ll.add(b)).wrapping_add(1);
            }
        }};
    }
    macro_rules! count_match {
        ($len_slot:expr, $off_slot:expr) => {{
            let (ls, os) = (FIRST_LEN + $len_slot as usize, $off_slot);
            debug_assert!(ls < NUM_LL && os < NUM_OFF);
            *f_ll.add(ls) = (*f_ll.add(ls)).wrapping_add(1);
            *f_off.add(os) = (*f_off.add(os)).wrapping_add(1);
        }};
    }

    if clear {
        ptr::write_bytes(tp, 0, 1 << BITS);
    }

    // Positions from `stop` on are neither looked up nor entered into the
    // table: eight bytes are read at one that is, and at a candidate.
    let stop = n.saturating_sub(9);
    // Before this, four positions in a row are before `stop`: one bound
    // check per four steps.
    let stop4 = n.saturating_sub(12);
    let mut pos = start;

    'tail: {
        if start == 0 {
            if stop <= 1 {
                break 'tail;
            }
            // The first position has nothing before it to match, and it is
            // in the table already: it is what an empty entry says.
            literal!(*ip);
            flush!();
            pos = 1;
        } else if pos >= stop {
            break 'tail;
        }
        let mut next_hash = hash5::<BITS>(rd64(ip, pos, n));

        macro_rules! step {
            ($found:lifetime) => {{
                let w = rd64(ip, pos, n);
                let h = next_hash;
                debug_assert!(h < 1 << BITS);
                let cand = *tp.add(h) as usize;
                *tp.add(h) = pos as u16;
                next_hash = hash5::<BITS>(w >> 8);
                let x = rd64(ip, cand, n) ^ w;
                if x as u32 == 0 {
                    break $found (cand, w, x);
                }
                literal!(w as u8);
                pos += 1;
            }};
        }

        loop {
            // At most 7 bits are in the bit buffer here.
            let (cand, w, x) = 'found: {
                while pos < stop4 {
                    step!('found);
                    step!('found);
                    step!('found);
                    step!('found);
                    flush!();
                }
                while pos < stop {
                    step!('found);
                    flush!();
                }
                break 'tail;
            };

            // A match at `pos` with the earlier position `cand`.
            debug_assert!(cand < pos);
            let len = if x != 0 {
                (x.trailing_zeros() >> 3) as usize // 4 to 7
            } else {
                let max = (n - pos).min(MAX_MATCH);
                let mut len = 8;
                loop {
                    if len + 8 > max {
                        while len < max && *ip.add(pos + len) == *ip.add(cand + len) {
                            len += 1;
                        }
                        break len;
                    }
                    let d = rd64(ip, pos + len, n) ^ rd64(ip, cand + len, n);
                    if d != 0 {
                        break len + (d.trailing_zeros() >> 3) as usize;
                    }
                    len += 8;
                }
            };
            let dist = pos - cand;
            if EMIT {
                let (bits, nbits, len_slot, off_slot) = match_bits(c, len, dist);
                flush!(); // the literals of this group of steps
                bitbuf |= bits.wrapping_shl(bitcount);
                bitcount += nbits;
                flush!();
                if COUNT {
                    count_match!(len_slot, off_slot);
                }
            } else {
                count_match!(LEN_SLOT[len], off_slot(dist as u32 - 1).0);
                debug_assert!(n_seqs < MAX_SEQS);
                *sp.add(n_seqs) =
                    (pos - run_start) as u64 | (len as u64) << 16 | (dist as u64) << 32;
                n_seqs += 1;
                run_start = pos + len;
            }

            // pos + 1 and pos + 2 are before `stop` + 1, so eight bytes can
            // be read there when they come up as candidates.
            *tp.add(next_hash) = (pos + 1) as u16;
            *tp.add(hash5::<BITS>(w >> 16)) = (pos + 2) as u16;
            pos += len;
            if pos >= stop {
                break 'tail;
            }
            let v = rd64(ip, pos - 1, n);
            *tp.add(hash5::<BITS>(v)) = (pos - 1) as u16;
            next_hash = hash5::<BITS>(v >> 8);
        }
    }

    // The last few bytes, where nothing can be hashed.  Nothing is waiting
    // in the bit buffer but the up to 7 bits that every flush leaves.
    while pos + 4 <= n {
        literal!(*ip.add(pos));
        literal!(*ip.add(pos + 1));
        literal!(*ip.add(pos + 2));
        literal!(*ip.add(pos + 3));
        flush!();
        pos += 4;
    }
    while pos < n {
        literal!(*ip.add(pos));
        pos += 1;
    }
    if !EMIT {
        return n_seqs;
    }
    flush!();
    if place.eob {
        bitbuf |= ((c.eob & 0xFFFF) as u64).wrapping_shl(bitcount);
        bitcount += c.eob >> 16;
        flush!();
    }
    // The store of the last flush put the bits that are left at out[o].
    if bitcount > 7 {
        return 0;
    }
    8 * o + bitcount as usize
}

/// Writes the block that `scan` recorded in `seqs`, with the codes `c`, which
/// must have a codeword for every symbol of the block.  Returns the number
/// of bits written.  `out` must have room for `scratch(block length)` bytes.
#[inline(never)]
#[allow(unused_assignments)]
pub(crate) unsafe fn emit_seqs(
    c: &Codes,
    win: &[u8],
    start: usize,
    seqs: &[Seq],
    out: *mut u8,
    cap: usize,
    place: Place,
) -> usize {
    let n = win.len();
    debug_assert!(cap >= scratch(n - start) && c.eob >> 16 != 0);
    let ip = win.as_ptr();
    let lit_tab = c.lit.as_ptr();
    let (mut o, mut bitbuf, mut bitcount) = start_block(c, out, cap, place);
    let mut pos = start;

    macro_rules! flush {
        () => {
            wr64(out, o, cap, bitbuf);
            o += ((bitcount >> 3) & 7) as usize;
            bitbuf >>= bitcount & 56;
            bitcount &= KEEP;
        };
    }
    macro_rules! literal {
        ($i:expr) => {{
            debug_assert!($i < n);
            let e = *lit_tab.add(*ip.add($i) as usize);
            bitbuf |= ((e & 0xFFFF) as u64).wrapping_shl(bitcount);
            bitcount = bitcount.wrapping_add(e >> 16);
        }};
    }
    macro_rules! literals {
        ($end:expr) => {{
            let end = $end;
            while pos + 4 <= end {
                literal!(pos);
                literal!(pos + 1);
                literal!(pos + 2);
                literal!(pos + 3);
                flush!();
                pos += 4;
            }
            // Up to three more: with the 7 bits a flush leaves, 49 at most.
            while pos < end {
                literal!(pos);
                pos += 1;
            }
            flush!();
        }};
    }

    for &s in seqs {
        let run = (s & 0xFFFF) as usize;
        let len = ((s >> 16) & 0xFFFF) as usize;
        let dist = (s >> 32) as usize;
        literals!(pos + run);
        let (bits, nbits, _, _) = match_bits(c, len, dist);
        bitbuf |= bits.wrapping_shl(bitcount);
        bitcount += nbits;
        flush!();
        pos += len;
    }
    literals!(n);
    if place.eob {
        bitbuf |= ((c.eob & 0xFFFF) as u64).wrapping_shl(bitcount);
        bitcount += c.eob >> 16;
        flush!();
    }
    debug_assert!(bitcount <= 7, "a byte without a codeword");
    8 * o + (bitcount & 7) as usize
}

/// A stored block: five bytes and the input as it is.  `out` is at a byte
/// boundary.  Returns the number of bits written.
pub(crate) unsafe fn stored(inp: &[u8], out: *mut u8, cap: usize, last: bool) -> usize {
    let n = inp.len();
    debug_assert!(n <= 0xFFFF && cap >= n + 5);
    let _ = cap;
    *out = last as u8; // BFINAL, BTYPE = 0, and padding to the byte boundary
    ptr::copy_nonoverlapping((n as u16).to_le_bytes().as_ptr(), out.add(1), 2);
    ptr::copy_nonoverlapping((!(n as u16)).to_le_bytes().as_ptr(), out.add(3), 2);
    ptr::copy_nonoverlapping(inp.as_ptr(), out.add(5), n);
    8 * (n + 5)
}

/// `inp` as stored blocks from bit `bitpos` on, which may be inside a byte.
/// A stored block is three bits, zeros up to the byte boundary, four bytes
/// and at most 65535 bytes of input as they are; a longer input takes
/// several.  Returns the new bit position.
pub(crate) unsafe fn stored_at(inp: &[u8], out: *mut u8, cap: usize, mut bitpos: usize, last: bool) -> usize {
    let mut rest = inp;
    loop {
        let n = rest.len().min(0xFFFF);
        // BFINAL and BTYPE = 0.  The bytes after the one they go into are
        // written as zeros, which is the padding.
        let fin = last && n == rest.len();
        let o = append_bits(out, cap, bitpos, fin as u32, 3).div_ceil(8);
        debug_assert!(o + 4 + n <= cap);
        ptr::copy_nonoverlapping((n as u16).to_le_bytes().as_ptr(), out.add(o), 2);
        ptr::copy_nonoverlapping((!(n as u16)).to_le_bytes().as_ptr(), out.add(o + 2), 2);
        ptr::copy_nonoverlapping(rest.as_ptr(), out.add(o + 4), n);
        bitpos = 8 * (o + 4 + n);
        rest = &rest[n..];
        if rest.is_empty() {
            return bitpos;
        }
    }
}

/// Appends `n` bits (at most 24) at bit `bitpos` of `out`, whose byte there
/// may hold anything above the bits in use.  Returns the new bit position.
/// For the few bits between blocks; a block's own bits go through the
/// encoder's buffer.
pub(crate) unsafe fn append_bits(out: *mut u8, cap: usize, bitpos: usize, bits: u32, n: u32) -> usize {
    debug_assert!(n <= 24 && bits >> n == 0);
    let (o, used) = (bitpos >> 3, (bitpos & 7) as u32);
    debug_assert!(o + 4 <= cap);
    let _ = cap;
    let cur = if used == 0 { 0 } else { *out.add(o) as u32 & ((1 << used) - 1) };
    ptr::copy_nonoverlapping((cur | bits << used).to_le_bytes().as_ptr(), out.add(o), 4);
    bitpos + n as usize
}

/// Brings `bitpos` to a byte boundary without ending the stream: an empty
/// stored block is three zero bits, zero bits up to the boundary, and
/// 00 00 FF FF.  A block header is copied in whole bytes, so a block that
/// begins with one has to begin on a boundary.
pub(crate) unsafe fn align(out: *mut u8, cap: usize, bitpos: usize) -> usize {
    if bitpos & 7 == 0 {
        return bitpos;
    }
    let o = (append_bits(out, cap, bitpos, 0, 3) + 7) >> 3;
    debug_assert!(o + 4 <= cap);
    ptr::copy_nonoverlapping([0u8, 0, 0xff, 0xff].as_ptr(), out.add(o), 4);
    8 * (o + 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_slots() {
        // Against the table of RFC 1951, 3.2.5.
        let base: [u32; 30] = [
            1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025,
            1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
        ];
        for dist in 1..=32768u32 {
            let want = (0..30).rev().find(|&s| base[s] <= dist).unwrap();
            let (slot, n, extra) = off_slot(dist - 1);
            assert_eq!(slot, want, "distance {dist}");
            assert_eq!(n, OFF_EXTRA[slot] as u32);
            assert_eq!(extra, dist - base[slot]);
        }
    }

    #[test]
    fn bits_between_blocks() {
        let mut buf = [0xffu8; 16];
        unsafe {
            let p = append_bits(buf.as_mut_ptr(), 16, 0, 0b101, 3);
            assert_eq!(p, 3);
            let p = append_bits(buf.as_mut_ptr(), 16, p, 0x3ff, 10);
            assert_eq!((p, buf[0], buf[1] & 0x1f), (13, 0xfd, 0x1f));
            buf[1] |= 0xe0; // garbage above the bits in use
            let p = align(buf.as_mut_ptr(), 16, p);
            assert_eq!((p, &buf[1..6]), (48, &[0x1f, 0, 0, 0xff, 0xff][..]));
            assert_eq!(align(buf.as_mut_ptr(), 16, p), p);
            // 6 bits in use: the three header bits spill into the next byte
            buf[6] = 0xff;
            buf[7] = 0xff;
            let p = align(buf.as_mut_ptr(), 16, 48 + 6);
            assert_eq!((p, &buf[6..12]), (96, &[0x3f, 0, 0, 0, 0xff, 0xff][..]));
        }
    }
}
