//! The searches of levels 1 to 9 as they are run.
//!
//! They find what `levels.rs` finds, match for match.  That file is the
//! plain statement of each search, which follows libdeflate's line by line,
//! and what these are tested against; it also takes the payloads of fewer
//! than eight bytes.  Here the same decisions are made with less work:
//!
//! * input is read eight bytes at a time through pointers, and what can be
//!   computed without a branch is;
//! * the tables hold positions in a form that needs no care at the
//!   beginning of a payload, and at level 1 nothing done to them in a
//!   payload of up to 64 KiB and one pass less than libdeflate makes in a
//!   longer one;
//! * the steps that find nothing, which in a payload that does not repeat
//!   itself much are most steps, are loops of their own with nothing else
//!   in them (`literals`, `chain_literals!`).
//!
//! `BIG` marks level 1's search of a payload of more than 32768 bytes:
//! matches reach no further back than that, so the table must forget, and
//! the payload is more than one block if libdeflate would make it more than
//! one.  Its search of a shorter payload is the same code with all of that
//! left out, and the tables of a shorter payload are smaller at every level,
//! in proportion to the payload.  The levels above 1 search a payload of
//! more than 32768 bytes in `big.rs`, and not quite as libdeflate does.

use crate::tables::*;
use crate::write::{token, Token};
use core::ptr;

/// How far back libdeflate's tables reach, and how many positions those
/// here tell apart.
pub(crate) const WINDOW: usize = 32768;

#[inline(always)]
unsafe fn rd32(p: *const u8, i: usize, n: usize) -> u32 {
    debug_assert!(i + 4 <= n);
    u32::from_le(ptr::read_unaligned(p.add(i) as *const u32))
}

/// The four bytes at `base + i`, where `base` itself may lie one byte before
/// what it points into.
#[inline(always)]
pub(crate) unsafe fn at32(base: *const u8, i: usize) -> u32 {
    u32::from_le(ptr::read_unaligned(base.wrapping_add(i) as *const u32))
}

#[inline(always)]
unsafe fn rd64(p: *const u8, i: usize, n: usize) -> u64 {
    debug_assert!(i + 8 <= n);
    u64::from_le(ptr::read_unaligned(p.add(i) as *const u64))
}

/// Asks for the memory at `p`, which is about to be written, to be brought
/// near.  Changes nothing but when it gets there.
#[inline(always)]
#[allow(unused_variables)]
pub(crate) unsafe fn fetch<T>(p: *const T) {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(p.cast());
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    core::arch::asm!("prfm pstl1keep, [{p}]", p = in(reg) p, options(nostack, preserves_flags, readonly));
}

/// libdeflate's hash: the top `bits` bits of a product.
#[inline(always)]
pub(crate) fn lz_hash(seq: u32, bits: u32) -> usize {
    (seq.wrapping_mul(0x1E35_A7BD) >> (32 - bits)) as usize
}

/// How many bytes are equal at `a` and at `b`: at least `len`, which are
/// known to be, and at most `max`.  `a` < `b`, and `max` bytes can be read
/// at `b`.
#[inline(always)]
unsafe fn extend(ip: *const u8, n: usize, a: usize, b: usize, mut len: usize, max: usize) -> usize {
    debug_assert!(a < b && b + max <= n && len <= max);
    while len + 8 <= max {
        let d = rd64(ip, a + len, n) ^ rd64(ip, b + len, n);
        if d != 0 {
            return len + (d.trailing_zeros() >> 3) as usize;
        }
        len += 8;
    }
    while len < max && *ip.add(a + len) == *ip.add(b + len) {
        len += 1;
    }
    len
}

/// Where a search writes down what it finds.  Raw pointers, and the counts
/// of what was written are kept by the search in variables of its own: a
/// store through any of these must not make the compiler think that the
/// search's own state may have changed.
#[derive(Clone, Copy)]
pub(crate) struct Sink {
    /// Symbol counts: `[u32; NUM_LL]` and `[u32; NUM_OFF]`, zeroed.
    pub f_ll: *mut u32,
    pub f_off: *mut u32,
    pub seqs: *mut Token,
    pub max_seqs: usize,
}

/// Counts a match and writes it down.  `$run_start` and `$n_seqs` are the
/// search's variables.
macro_rules! matched {
    ($sink:expr, $run_start:ident, $n_seqs:ident, $pos:expr, $len:expr, $dist:expr) => {{
        let (pos, len, dist): (usize, usize, usize) = ($pos, $len, $dist);
        debug_assert!((MIN_MATCH..=MAX_MATCH).contains(&len) && (1..=32768).contains(&dist));
        debug_assert!(pos >= $run_start && $n_seqs < $sink.max_seqs);
        let (t, slot) = token(pos - $run_start, len, dist);
        *$sink.f_ll.add(FIRST_LEN + *LEN_SLOT.as_ptr().add(len) as usize) += 1;
        *$sink.f_off.add(slot) += 1;
        *$sink.seqs.add($n_seqs) = t;
        $n_seqs += 1;
        $run_start = pos + len;
    }};
}

// ---- level 1 -----------------------------------------------------------
//
// A table of buckets, each two entries of 16 bits, the more recent one in
// the low half.  An entry is a position less `off`.
//
// A match may reach 32767 bytes back.  libdeflate keeps its positions
// relative to the beginning of a window, and moves the window on by 32768
// bytes when a search reaches its end, or just before the positions of a
// match that runs across that end are entered.  What was before the old
// beginning is forgotten then; no later search could have reached it.  The
// window is moved here where it is moved there.
//
// `off` is 0 while the window begins at 0 or at 32768, and from then on
// 32768 less than the window's beginning.  So an entry is the position
// itself in the first 65536 bytes of a payload; the first move of the
// window changes nothing in the table; and at every later one each entry is
// reduced by 32768, or becomes 0 if it is no more than that, 0 being what
// stands for nothing.
//
// An entry of 0 is also what the first position of the payload has.
// libdeflate enters that position under hash 0 whatever its bytes are, so
// it is only ever found again by a position that has hash 0 and the same
// four bytes: only in a payload whose first four bytes have hash 0.  In
// such a payload an entry of 0 is taken to be the first position for as
// long as that position is within reach, which is right for the bucket it
// was entered in and makes no difference in any other, where the four bytes
// cannot be the same.  In every other payload 0 is nothing.

/// Reduces every entry of the tables by 32768; those that stand for what a
/// match can no longer reach become 0.
#[cold]
#[inline(never)]
pub(crate) unsafe fn rebase(tables: &[(*mut u16, usize)]) {
    for &(t, len) in tables {
        for e in core::slice::from_raw_parts_mut(t, len) {
            *e = e.saturating_sub(WINDOW as u16);
        }
    }
}

/// Level 1's steps over literals.  Positions are given as their entries
/// (`ent` is where the byte of entry 0 is): from `e` up to `end`, until a
/// position has a candidate with the same four bytes.  Returns that
/// position (or `end`) and its hash; it has not been entered.
///
/// A step loads the bucket of its position and, without asking whether it
/// holds anything, eight bytes at both of its entries (an entry of 0 reads
/// what is there too).  One branch then tells a literal from the rest.
///
/// A function of its own so that this loop is all the compiler sees: inside
/// the search it shared its registers with everything a match needs, and
/// took a third more instructions for the same work.
#[inline(never)]
unsafe fn literals<const ORDER: u32>(
    ent: *const u8,
    tab: *mut u32,
    f_ll: *mut u32,
    mut e: usize,
    end: usize,
    mut h: usize,
) -> (usize, usize) {
    #[inline(always)]
    unsafe fn at(ent: *const u8, e: usize) -> u64 {
        u64::from_le(ptr::read_unaligned(ent.add(e) as *const u64))
    }
    while e < end {
        let w = at(ent, e);
        debug_assert!(h < 1 << ORDER);
        let bucket = *tab.add(h);
        let x1 = at(ent, (bucket & 0xFFFF) as usize) ^ w;
        let x2 = at(ent, (bucket >> 16) as usize) ^ w;
        if (x1 as u32 == 0) | (x2 as u32 == 0) {
            break;
        }
        *tab.add(h) = bucket << 16 | e as u32;
        h = lz_hash((w >> 8) as u32, ORDER);
        *f_ll.add(w as u8 as usize) += 1;
        e += 1;
    }
    (e, h)
}

/// A block of level 1, from where `st` says: what `levels::Search::fastest`
/// finds.  Returns the number of matches; `st` then says where the next
/// block begins.
///
/// `tab` has `1 << ORDER` buckets and was zeroed before the payload's first
/// block.  `BIG` for a payload of more than 32768 bytes, in which the
/// window moves and a block ends at `soft_end` or after
/// `levels::FAST_MAX_SEQS` matches, as libdeflate's does; any other payload
/// is one block.  The payload has at least 8 bytes.
pub(crate) unsafe fn fastest<const ORDER: u32, const BIG: bool>(
    inp: &[u8],
    tab: *mut u32,
    sink: Sink,
    st: &mut Resume,
    soft_end: usize,
) -> usize {
    use crate::levels::FAST_MAX_SEQS;
    let n = inp.len();
    debug_assert!(n >= 8 && (BIG || (n <= WINDOW && soft_end == n)));
    let ip = inp.as_ptr();
    let f_ll = sink.f_ll;
    let (mut n_seqs, mut run_start) = (0usize, st.pos);
    let mut pos = st.pos;
    let mut h = st.nh.1;
    // Where the window begins, and what follows from it (see above).
    let mut base = st.win;
    let mut off = if BIG { base.saturating_sub(WINDOW) } else { 0 };
    let mut ent = ip.add(off);
    let tables = [(tab.cast::<u16>(), 2usize << ORDER)];
    macro_rules! slide {
        () => {{
            if base != 0 {
                rebase(&tables);
                off += WINDOW;
                ent = ip.add(off);
            }
            base += WINDOW;
        }};
    }
    // The lowest entry that stands for a position within reach of the one
    // whose entry is `$e`: no more than 32767 back, and not an entry of 0
    // unless that is the payload's first position (see above).
    let first = WINDOW - 1 + (lz_hash(rd32(ip, 0, n), ORDER) != 0) as usize;
    macro_rules! lowest {
        ($e:expr) => {
            if BIG {
                $e.max(first) - (WINDOW - 1)
            } else {
                first - (WINDOW - 1)
            }
        };
    }
    // A step reads eight bytes at its position.
    let fast_end = n - 7;
    // Steps are taken without a question up to here: the end of the eight
    // bytes, of the block, or of the window.
    macro_rules! stop_at {
        () => {
            if BIG {
                fast_end.min(soft_end).min(base + WINDOW)
            } else {
                fast_end
            }
        };
    }

    if pos == 0 {
        // Nothing is before the first position: a literal.  It is entered
        // under hash 0, where its entry of 0 already is.
        debug_assert!(h == 0 && *tab == 0);
        let wd = rd64(ip, 0, n);
        h = lz_hash((wd >> 8) as u32, ORDER);
        *f_ll.add(wd as u8 as usize) += 1;
        pos = 1;
    }
    let mut stop = stop_at!();
    let ended = 'block: {
        loop {
            if pos >= stop {
                if BIG && pos >= soft_end {
                    break 'block true;
                }
                if pos >= fast_end {
                    break;
                }
                debug_assert!(BIG && pos == base + WINDOW);
                slide!();
                stop = stop_at!();
            }
            // A step.  First what the literal loop asks too: does either
            // entry of the bucket of `pos` have the same four bytes?
            let mut wd = rd64(ip, pos, n);
            let mut e = pos - off;
            debug_assert!(h < 1 << ORDER && e < 2 * WINDOW);
            let mut bucket = *tab.add(h);
            let mut x1 = rd64(ent, (bucket & 0xFFFF) as usize, n - off) ^ wd;
            let mut x2 = rd64(ent, (bucket >> 16) as usize, n - off) ^ wd;
            if (x1 as u32 != 0) & (x2 as u32 != 0) {
                // A literal, and the literals after it.
                *tab.add(h) = bucket << 16 | e as u32;
                h = lz_hash((wd >> 8) as u32, ORDER);
                *f_ll.add(wd as u8 as usize) += 1;
                debug_assert!(stop + 7 <= n);
                (e, h) = literals::<ORDER>(ent, tab, f_ll, e + 1, stop - off, h);
                pos = e + off;
                if pos == stop {
                    continue;
                }
                wd = rd64(ip, pos, n);
                bucket = *tab.add(h);
                x1 = rd64(ent, (bucket & 0xFFFF) as usize, n - off) ^ wd;
                x2 = rd64(ent, (bucket >> 16) as usize, n - off) ^ wd;
            }
            // One does.  The position is entered, and each candidate is
            // given the number of bytes it has in common with it, up to 8
            // (which stands for 8 or more), or none if it is out of reach.
            debug_assert!(h < 1 << ORDER && e < 2 * WINDOW);
            *tab.add(h) = bucket << 16 | e as u32;
            h = lz_hash((wd >> 8) as u32, ORDER);
            let (s1, s2) = ((bucket & 0xFFFF) as usize, (bucket >> 16) as usize);
            let lo = lowest!(e);
            let l1 = if s1 >= lo { (x1.trailing_zeros() >> 3) as usize } else { 0 };
            let l2 = if s2 >= lo { (x2.trailing_zeros() >> 3) as usize } else { 0 };
            // The longer one; the more recent one if they are equal.
            let (mut len, mut s) = if l2 > l1 { (l2, s2) } else { (l1, s1) };
            if len < 4 {
                // The four bytes were at a position out of reach.
                *f_ll.add(wd as u8 as usize) += 1;
                pos += 1;
                continue;
            }
            if len == 8 {
                let max_len = (n - pos).min(MAX_MATCH);
                let e1 = if l1 == 8 { extend(ip, n, s1 + off, pos, 8, max_len) } else { l1 };
                len = e1;
                s = s1;
                // libdeflate does not look at the second candidate once the
                // first is 32 bytes long.
                if e1 < 32 && l2 == 8 {
                    let e2 = extend(ip, n, s2 + off, pos, 8, max_len);
                    if e2 > e1 {
                        len = e2;
                        s = s2;
                    }
                }
            }
            matched!(sink, run_start, n_seqs, pos, len, e - s);
            // The positions of the match are entered, unless the payload
            // ends within five bytes of it.  If the last of them is beyond
            // the window's end, the window moves first.
            let end = pos + len;
            if n - end >= 5 {
                if BIG && end - 1 - base >= WINDOW {
                    slide!();
                    stop = stop_at!();
                }
                let mut q = pos + 1 - off;
                let q_end = end - off;
                while q < q_end {
                    debug_assert!(h < 1 << ORDER && q < 2 * WINDOW);
                    let b = *tab.add(h);
                    *tab.add(h) = b << 16 | q as u32;
                    h = lz_hash(rd32(ent, q + 1, n - off), ORDER);
                    q += 1;
                }
            }
            pos = end;
            if BIG && n_seqs >= FAST_MAX_SEQS {
                break 'block true;
            }
        }
        // The last seven positions of the payload, a byte at a time.
        while n - pos >= 5 {
            if BIG && pos - base == WINDOW {
                slide!();
            }
            let max_len = n - pos;
            let seq = rd32(ip, pos, n);
            let e = pos - off;
            let bucket = *tab.add(h);
            *tab.add(h) = bucket << 16 | e as u32;
            h = lz_hash(rd32(ip, pos + 1, n), ORDER);
            let (s1, s2) = ((bucket & 0xFFFF) as usize, (bucket >> 16) as usize);
            let lo = lowest!(e);
            let (mut len, mut s) = (0, 0);
            if s1 >= lo {
                if rd32(ent, s1, n - off) == seq {
                    len = extend(ip, n, s1 + off, pos, 4, max_len);
                    s = s1;
                }
                if s2 >= lo && len < max_len && rd32(ent, s2, n - off) == seq {
                    let l2 = extend(ip, n, s2 + off, pos, 4, max_len);
                    if l2 > len {
                        len = l2;
                        s = s2;
                    }
                }
            }
            if len == 0 {
                *f_ll.add(seq as u8 as usize) += 1;
                pos += 1;
            } else {
                // The payload ends within five bytes of this match.
                matched!(sink, run_start, n_seqs, pos, len, e - s);
                pos += len;
            }
            if BIG && (pos >= soft_end || n_seqs >= FAST_MAX_SEQS) {
                break 'block true;
            }
        }
        false
    };
    if !ended {
        // Fewer than five bytes are left: literals, and the payload is done.
        while pos < n {
            *f_ll.add(*ip.add(pos) as usize) += 1;
            pos += 1;
        }
    }
    st.pos = pos;
    st.nh.1 = h;
    st.win = base;
    n_seqs
}

// ---- levels 2 to 9, a payload of up to 32768 bytes ---------------------
//
// Two tables that say where a hash of three and of four bytes was last
// seen, and for every position the one before it with the same hash of four
// bytes: a chain to walk.  An entry is a position plus 1, and 0 stands for
// nothing.  In a payload of this length every entry fits in 16 bits and
// every position is within reach of every later one, so nothing is ever
// done to the tables but entering positions.  (A longer payload is searched
// in `big.rs`.)
//
// A search counts in entries throughout: where it is, where the block began
// and where the payload ends (`Run`).  What it reads of the payload it reads
// at `ent` plus an entry, `ent` being the address that the byte of entry 0
// would have.

/// Table sizes of levels 2 to 9 for a payload of up to `1 << LG` bytes:
/// `1 << O3` entries for hashes of three bytes, `1 << O4` for four.  With
/// `LG` = 15 they are libdeflate's.
pub(crate) struct Orders<const LG: u32>;

impl<const LG: u32> Orders<LG> {
    pub(crate) const O3: u32 = crate::levels::chain_orders(LG).0;
    pub(crate) const O4: u32 = crate::levels::chain_orders(LG).1;
}

/// The input and the tables of levels 2 to 9 (see `levels::Tables`).  The
/// hash tables have been zeroed.
#[derive(Clone, Copy)]
pub(crate) struct Chains {
    pub ip: *const u8,
    pub n: usize,
    pub h3: *mut u16,
    pub h4: *mut u16,
    pub next: *mut u16,
    /// How many candidates to look at, and the length at which to stop.
    pub depth: u32,
    pub nice: usize,
}

/// Where a search stopped, to go on from when the block after it is
/// searched.
#[derive(Clone, Copy, Default)]
pub(crate) struct Resume {
    pub pos: usize,
    /// The hashes of `pos`, of three and of four bytes; level 1 has only
    /// the second, and `big.rs` works them out again.
    pub nh: (usize, usize),
    /// In a payload of more than 32768 bytes: at level 1 where the window
    /// begins, at the levels above what entries are counted from
    /// (`big::Chain`).
    pub win: usize,
}

/// Four bytes as they lie at `base + i`, for comparing with four others.
#[inline(always)]
pub(crate) unsafe fn raw_at32(base: *const u8, i: usize) -> u32 {
    ptr::read_unaligned(base.wrapping_add(i) as *const u32)
}

#[inline(always)]
pub(crate) unsafe fn at64(base: *const u8, i: usize) -> u64 {
    u64::from_le(ptr::read_unaligned(base.wrapping_add(i) as *const u64))
}

/// `extend` for bytes that are addressed by entry: how many are equal at
/// the entries `a` and `b`, at least `len` and at most `max`.  `a` < `b`,
/// and `end` is the entry after the payload's last.
#[inline(always)]
pub(crate) unsafe fn extend_at(ent: *const u8, end: usize, a: usize, b: usize, mut len: usize, max: usize) -> usize {
    debug_assert!(a < b && b + max <= end && len <= max);
    let _ = end;
    while len + 8 <= max {
        let d = at64(ent, a + len) ^ at64(ent, b + len);
        if d != 0 {
            return len + (d.trailing_zeros() >> 3) as usize;
        }
        len += 8;
    }
    while len < max && *ent.wrapping_add(a + len) == *ent.wrapping_add(b + len) {
        len += 1;
    }
    len
}

/// A search of levels 2 to 9 at work on a block.  Everything that says
/// where is an entry (see above).
struct Run {
    /// The address that the byte of entry 0 would have.
    ent: *const u8,
    /// The position to search next, and the entry after the payload's last.
    pos: usize,
    end: usize,
    /// Where the block began, and where the literals begin that are not
    /// yet part of a token.
    begin: usize,
    run_start: usize,
    /// Up to where `chain_literals!` may go (see `chain_stop!`).
    stop: usize,
    /// Where a lazy search next counts the bytes in use; never reached by
    /// a greedy one.
    recalc: usize,
    /// The hashes of `pos`, of three and of four bytes.
    nh: (usize, usize),
    /// How many matches the block has so far.
    n_seqs: usize,
    /// The length that was good enough for the search last made (see
    /// `chain_limits!`).
    nice_len: usize,
}

macro_rules! chain_run {
    ($c:ident, $st:ident) => {{
        debug_assert!($c.n >= 8 && $c.n <= WINDOW);
        let pos = $st.pos + 1;
        Run {
            ent: $c.ip.wrapping_sub(1),
            pos,
            end: $c.n + 1,
            begin: pos,
            run_start: pos,
            stop: 0,
            recalc: usize::MAX,
            nh: $st.nh,
            n_seqs: 0,
            nice_len: 0,
        }
    }};
}
/// The longest match libdeflate allows at `s.pos` and the length it calls
/// good enough there: its settings, cut down where the payload ends
/// sooner.  (Worked out at every step.  Keeping them from step to step, to
/// cut them down only near the end, measured no quicker: two more things
/// for a search to hold on to.)
macro_rules! chain_limits {
    ($c:ident, $s:ident) => {{
        let max_len = ($s.end - $s.pos).min(MAX_MATCH);
        $s.nice_len = $c.nice.min(max_len);
        (max_len, $s.nice_len)
    }};
}

/// libdeflate's `hc_matchfinder_longest_match` (see `levels.rs`): the
/// longest match at the entry `p` that is longer than `best_len`, among the
/// first `depth` candidates, as (length, distance); `p` is entered into the
/// tables, and `nh` becomes the hashes of the position after it.  `node3`
/// is what the table of hashes of three bytes holds for `p`, and `end` the
/// entry after the payload's last.
///
/// The candidates are a chain: each position leads to the one before it
/// with the same hash (`c.next`).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn hc_longest<const LG: u32>(
    c: Chains,
    ent: *const u8,
    end: usize,
    nh: &mut (usize, usize),
    p: usize,
    node3: usize,
    mut best_len: usize,
    max_len: usize,
    nice_len: usize,
    mut depth: u32,
) -> (usize, usize) {
    if max_len < 5 {
        return (best_len, 0); // four bytes cannot be read at p + 1
    }
    debug_assert!((1..WINDOW).contains(&p) && p + max_len <= end);
    debug_assert!(nh.0 < 1 << Orders::<LG>::O3 && nh.1 < 1 << Orders::<LG>::O4);
    debug_assert!(node3 == *c.h3.add(nh.0) as usize);
    let node4 = *c.h4.add(nh.1) as usize;
    *c.h3.add(nh.0) = p as u16;
    *c.h4.add(nh.1) = p as u16;
    *c.next.add(p) = node4 as u16;
    let next_seq = at32(ent, p + 1);
    *nh = (
        lz_hash(next_seq & 0xFF_FFFF, Orders::<LG>::O3),
        lz_hash(next_seq, Orders::<LG>::O4),
    );
    let mut best = p;
    'out: {
        let seq4 = at32(ent, p);
        if best_len < 4 {
            if node3 == 0 {
                break 'out;
            }
            if best_len < 3 && (at32(ent, node3) ^ seq4) & 0xFF_FFFF == 0 {
                best_len = 3;
                best = node3;
            }
            if node4 == 0 {
                break 'out;
            }
        } else if node4 == 0 || best_len >= nice_len {
            break 'out;
        }

        // The chain, from `node4` on.  Nearly all of this is stepping past
        // candidates that are no better than the best so far, so that is
        // what a step is made of: one comparison of the four bytes that
        // would make the candidate longer than the best (its first four, as
        // long as there is no match of four), and what leads to the next
        // candidate.  Only a candidate that passes is looked at further.
        let mut tail_at = best_len.max(3) - 3;
        let mut tail = raw_at32(ent, p + tail_at);
        let mut tails = ent.wrapping_add(tail_at);
        macro_rules! passes {
            ($node:expr) => {{
                let node: usize = $node;
                debug_assert!(node >= 1 && node < p && node + tail_at + 4 <= end);
                ptr::read_unaligned(tails.wrapping_add(node) as *const u32) == tail
            }};
        }
        let mut node = node4;
        let mut pass = passes!(node);
        loop {
            if pass && at32(ent, node) == seq4 {
                let len = extend_at(ent, end, node, p, 4, max_len);
                if len > best_len {
                    best_len = len;
                    best = node;
                    if best_len >= nice_len {
                        break 'out;
                    }
                    tail_at = best_len - 3;
                    tail = raw_at32(ent, p + tail_at);
                    tails = ent.wrapping_add(tail_at);
                }
            }
            depth -= 1;
            if depth == 0 {
                break 'out;
            }
            // On to the next candidate that passes.
            loop {
                node = *c.next.add(node) as usize;
                if node == 0 {
                    break 'out;
                }
                if passes!(node) {
                    break;
                }
                depth -= 1;
                if depth == 0 {
                    break 'out;
                }
            }
            pass = true;
        }
    }
    (best_len, p - best)
}

/// Enters the `count` positions from the entry `from` on, the last of which
/// has five bytes of payload from it on.
#[inline(always)]
unsafe fn hc_enter<const LG: u32>(c: Chains, ent: *const u8, nh: &mut (usize, usize), from: usize, count: usize) {
    let (mut h3, mut h4) = *nh;
    let mut q = from;
    let stop = from + count;
    while q < stop {
        debug_assert!((1..WINDOW).contains(&q));
        *c.h3.add(h3) = q as u16;
        *c.next.add(q) = *c.h4.add(h4);
        *c.h4.add(h4) = q as u16;
        let s = at32(ent, q + 1);
        h3 = lz_hash(s & 0xFF_FFFF, Orders::<LG>::O3);
        h4 = lz_hash(s, Orders::<LG>::O4);
        q += 1;
    }
    *nh = (h3, h4);
}

/// libdeflate's `hc_matchfinder_skip_bytes`: moves on past `count`
/// positions, entering them unless the payload ends within five bytes of
/// them.
macro_rules! chain_skip {
    ($LG:ident, $c:ident, $s:ident, $count:expr) => {{
        let count: usize = $count;
        if count + 5 <= $s.end - $s.pos {
            hc_enter::<$LG>($c, $s.ent, &mut $s.nh, $s.pos, count);
        }
        $s.pos += count;
    }};
}
/// Up to where `chain_literals!` may go: nowhere if the search asks for
/// matches longer than four bytes (see there); else the last position at
/// which five bytes are left, where a lazy search next counts the bytes in
/// use, and where the block could be ended early.
macro_rules! chain_stop {
    ($s:ident, $split:ident, $splitting:ident, $min_len:ident) => {{
        let mut stop = 0;
        if $min_len <= 4 {
            stop = ($s.end - 4).min($s.recalc);
            if $splitting {
                if let Some(at) = (*$split).may_end_at($s.pos - $s.begin, $s.end - $s.pos) {
                    stop = stop.min($s.begin + at);
                }
            }
        }
        stop
    }};
}
/// The positions from `s.pos` on that have no candidate at all, as far as
/// `s.stop` allows: they are literals.  Each is entered and counted.  The
/// first of them is `s.pos` itself, which the caller has found to be one.
/// Evaluates to whether the block goes on.
///
/// A search asks for the longest match longer than some length.  If that
/// length is less than 4, the first thing it looks at is where the three
/// bytes at hand were last seen, and if that is nowhere it has its answer.
/// In a payload that does not repeat itself much that is how most searches
/// end, and this loop is those searches with nothing else in it:
/// `hc_longest` up to that point, and what its caller does with the answer.
/// `s.stop` is where there may be something else to do first; it is never
/// too far on, and is put right when it has been reached.
///
/// The loop is part of the search, not a function as level 1 has it: a
/// search that calls a function from its steps keeps its state where a call
/// leaves it alone, and with as much state as these searches have, every
/// step paid for that.
macro_rules! chain_literals {
    ($LG:ident, $c:ident, $s:ident, $sink:ident, $st:ident, $split:ident, $splitting:ident, $min_len:ident) => {{
        let mut on = true;
        loop {
            let from = $s.pos;
            // Literals, until a position has a candidate (true) or `stop`
            // is reached.
            let found = loop {
                let p = $s.pos;
                debug_assert!((1..WINDOW).contains(&p) && p + 5 <= $s.end);
                *$c.h3.add($s.nh.0) = p as u16;
                *$c.next.add(p) = *$c.h4.add($s.nh.1);
                *$c.h4.add($s.nh.1) = p as u16;
                let seq = at32($s.ent, p + 1);
                $s.nh = (lz_hash(seq & 0xFF_FFFF, Orders::<$LG>::O3), lz_hash(seq, Orders::<$LG>::O4));
                let b = *$s.ent.wrapping_add(p);
                *$sink.f_ll.add(b as usize) += 1;
                if $splitting {
                    let kinds = ptr::addr_of_mut!((*$split).new_obs).cast::<u32>();
                    *kinds.add(crate::levels::Split::kind(b)) += 1;
                }
                $s.pos = p + 1;
                if $s.pos >= $s.stop {
                    break false;
                }
                if *$c.h3.add($s.nh.0) != 0 {
                    break true;
                }
            };
            if $splitting {
                (*$split).num_new += ($s.pos - from) as u32;
            }
            if found {
                break;
            }
            // Something may be due here.
            on = chain_goes_on!($s, $st, $split, $splitting, $sink);
            if !on {
                break;
            }
            $s.stop = chain_stop!($s, $split, $splitting, $min_len);
            if $s.pos >= $s.stop || *$c.h3.add($s.nh.0) != 0 {
                break;
            }
        }
        on
    }};
}
macro_rules! chain_literal {
    ($s:ident, $sink:ident, $split:ident, $splitting:ident, $p:expr) => {{
        let b = *$s.ent.wrapping_add($p);
        *$sink.f_ll.add(b as usize) += 1;
        if $splitting {
            (*$split).literal(b);
        }
    }};
}
/// Counts a match at the entry `$p` and writes it down.
macro_rules! chain_match {
    ($s:ident, $sink:ident, $split:ident, $splitting:ident, $p:expr, $len:expr, $dist:expr) => {{
        let (p, len, dist): (usize, usize, usize) = ($p, $len, $dist);
        debug_assert!((MIN_MATCH..=MAX_MATCH).contains(&len) && (1..=32768).contains(&dist));
        debug_assert!($s.n_seqs < $sink.max_seqs);
        let (t, slot) = token(p - $s.run_start, len, dist);
        *$sink.f_ll.add(FIRST_LEN + *LEN_SLOT.as_ptr().add(len) as usize) += 1;
        *$sink.f_off.add(slot) += 1;
        *$sink.seqs.add($s.n_seqs) = t;
        $s.n_seqs += 1;
        $s.run_start = p + len;
        if $splitting {
            (*$split).matched(len);
        }
    }};
}
/// Does the block go on at `s.pos`?  It ends with the payload, or where
/// what it holds changes.  (A payload of this length has neither a block of
/// the greatest length nor as many matches as a block has room for.)  If it
/// ends, the search's state is written back for the block after it.
macro_rules! chain_goes_on {
    ($s:ident, $st:ident, $split:ident, $splitting:ident, $sink:ident) => {{
        debug_assert!($s.n_seqs < $sink.max_seqs);
        let on = $s.pos < $s.end && !($splitting && (*$split).ends_at($s.pos - $s.begin, $s.end - $s.pos));
        if !on {
            $st.pos = $s.pos - 1;
            $st.nh = $s.nh;
        }
        on
    }};
}

/// A block of levels 2 to 4: what `levels::Search::greedy` finds.  Returns
/// the number of matches.
pub(crate) unsafe fn greedy<const LG: u32>(
    c: Chains,
    min_len: usize,
    sink: Sink,
    st: &mut Resume,
    split: *mut crate::levels::Split,
) -> usize {
    let splitting = !split.is_null();
    let mut s = chain_run!(c, st);
    s.stop = chain_stop!(s, split, splitting, min_len);
    loop {
        let node3 = *c.h3.add(s.nh.0) as usize;
        if s.pos < s.stop && node3 == 0 {
            if !chain_literals!(LG, c, s, sink, st, split, splitting, min_len) {
                break;
            }
            continue;
        }
        let (max_len, nice_len) = chain_limits!(c, s);
        let (len, dist) =
            hc_longest::<LG>(c, s.ent, s.end, &mut s.nh, s.pos, node3, min_len - 1, max_len, nice_len, c.depth);
        if len >= min_len && (len > MIN_MATCH || dist <= 4096) {
            chain_match!(s, sink, split, splitting, s.pos, len, dist);
            s.pos += 1;
            chain_skip!(LG, c, s, len - 1);
        } else {
            chain_literal!(s, sink, split, splitting, s.pos);
            s.pos += 1;
        }
        if !chain_goes_on!(s, st, split, splitting, sink) {
            break;
        }
        if s.pos >= s.stop {
            s.stop = chain_stop!(s, split, splitting, min_len);
        }
    }
    s.n_seqs
}

/// A block of levels 5 to 9: what `levels::Search::lazy` finds.  Returns the
/// number of matches.
pub(crate) unsafe fn lazy<const LG: u32>(
    c: Chains,
    mut min_len: usize,
    two: bool,
    sink: Sink,
    st: &mut Resume,
    split: *mut crate::levels::Split,
) -> usize {
    let splitting = !split.is_null();
    let mut s = chain_run!(c, st);
    s.recalc = s.pos + (s.end - s.pos).min(10000);
    s.stop = chain_stop!(s, split, splitting, min_len);
    let better = |nl: usize, no: usize, cl: usize, co: usize, by: i32| {
        nl >= cl
            && 4 * (nl - cl) as i32 + (no.leading_zeros() as i32 - co.leading_zeros() as i32) > by
    };
    // The longest match at `s.pos` that is longer than `$longer`, among
    // `$depth` candidates; `s.pos` moves on by one.
    macro_rules! search {
        ($node3:expr, $longer:expr, $depth:expr) => {{
            let node3: usize = $node3;
            let (max_len, nice_len) = chain_limits!(c, s);
            let found =
                hc_longest::<LG>(c, s.ent, s.end, &mut s.nh, s.pos, node3, $longer, max_len, nice_len, $depth);
            s.pos += 1;
            found
        }};
    }
    loop {
        let node3 = *c.h3.add(s.nh.0) as usize;
        if s.pos < s.stop && node3 == 0 {
            if !chain_literals!(LG, c, s, sink, st, split, splitting, min_len) {
                break;
            }
            continue;
        }
        'step: {
            if s.pos >= s.recalc {
                min_len = crate::levels::later_min_len(&*(sink.f_ll as *const [u32; NUM_LL]), c.depth);
                s.recalc += (s.end - s.recalc).min(s.pos - s.begin);
            }
            let (mut cur_len, mut cur_dist) = search!(node3, min_len - 1, c.depth);
            if cur_len < min_len || (cur_len == MIN_MATCH && cur_dist > 8192) {
                chain_literal!(s, sink, split, splitting, s.pos - 1);
                break 'step;
            }
            // A match at pos - 1, and the length that was good enough
            // there.
            let mut cur_nice = s.nice_len;
            loop {
                if cur_len >= cur_nice {
                    chain_match!(s, sink, split, splitting, s.pos - 1, cur_len, cur_dist);
                    chain_skip!(LG, c, s, cur_len - 1);
                    break 'step;
                }
                // The next position, with half the candidates.
                let (next_len, next_dist) = search!(*c.h3.add(s.nh.0) as usize, cur_len - 1, c.depth >> 1);
                if better(next_len, next_dist, cur_len, cur_dist, 2) {
                    chain_literal!(s, sink, split, splitting, s.pos - 2);
                    (cur_len, cur_dist, cur_nice) = (next_len, next_dist, s.nice_len);
                    continue;
                }
                if two {
                    // And the one after it, with a quarter.
                    let (next_len, next_dist) = search!(*c.h3.add(s.nh.0) as usize, cur_len - 1, c.depth >> 2);
                    if better(next_len, next_dist, cur_len, cur_dist, 6) {
                        chain_literal!(s, sink, split, splitting, s.pos - 3);
                        chain_literal!(s, sink, split, splitting, s.pos - 2);
                        (cur_len, cur_dist, cur_nice) = (next_len, next_dist, s.nice_len);
                        continue;
                    }
                    chain_match!(s, sink, split, splitting, s.pos - 3, cur_len, cur_dist);
                    if cur_len > 3 {
                        chain_skip!(LG, c, s, cur_len - 3);
                    }
                } else {
                    chain_match!(s, sink, split, splitting, s.pos - 2, cur_len, cur_dist);
                    chain_skip!(LG, c, s, cur_len - 2);
                }
                break 'step;
            }
        }
        if !chain_goes_on!(s, st, split, splitting, sink) {
            break;
        }
        if s.pos >= s.stop {
            s.stop = chain_stop!(s, split, splitting, min_len);
        }
    }
    s.n_seqs
}
