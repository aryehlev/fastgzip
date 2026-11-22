//! Levels 2 to 9 in a payload of more than 32768 bytes.
//!
//! The search is libdeflate's at the same level: its tables, its number of
//! candidates, the length at which it stops looking, and its way of taking a
//! match or first trying the next position (lib/deflate_compress.c and
//! lib/hc_matchfinder.h; MIT licence, Eric Biggers).  Three things are not:
//!
//! * A block is searched for matches of three bytes only if it looks like
//!   data in which they pay (`levels::first_look`).  libdeflate keeps a
//!   table of three-byte hashes for every payload, enters every position in
//!   it, and uses it whenever a block holds 80 or more different bytes.
//!   Measured on payloads of 45 to 375 KB: in machine code such matches
//!   make the output 1.5 to 3% smaller; in text they change it by less than
//!   0.3%, and looking for them takes a tenth of the time; in JSON they make
//!   it about 1% larger and take a fifth.  Without the table, a position
//!   whose four bytes were not seen within reach is a literal at once, which
//!   is how most searches in such payloads end.
//! * The payload's last seven bytes are not searched: a step reads eight
//!   bytes.
//! * The payload's first position is entered under its own hash, not under
//!   hash 0 as libdeflate enters it.
//!
//! So the output is not libdeflate's, match for match, as it is for payloads
//! of up to 32768 bytes.  It is the same size to within a fraction of a
//! percent, or smaller (see the README for what was measured).
//!
//! `levels.rs` has this search in plain form, and that is what this one is
//! tested against, byte for byte.  What is done here to make it quick:
//!
//! * The loop asks whether the block ends, whether the tables are full and
//!   whether the payload ends only at positions where one of those can be
//!   the case (`stop`); every step before that is a search and nothing
//!   else.
//! * What block splitting looks at, how many literals and matches of which
//!   kind the block has had, is not counted as they come.  It is worked out
//!   from the block's symbol counts, which are kept anyway, when there is
//!   something to decide: once in about 512 literals and matches.
//! * Positions without a candidate are a loop of their own, as in `fast.rs`.
//! * Where chains are followed far, a position holds the four before it
//!   and not one (`HOPS`), as libdeflate has it at its highest levels: one
//!   load in four steps then waits for another.

use crate::fast::{at32, at64, extend_at, fetch, lz_hash, raw_at32, rebase, Resume, Sink, WINDOW};
use crate::levels::{later_min_len, Split, MAX_SEQS};
use crate::tables::*;
use crate::write::token;
use core::ptr;

/// libdeflate's tables, with 16 bits an entry: where a hash of three and of
/// four bytes was last seen (32768 and 65536 entries), and for every position
/// the one before it with the same hash of four bytes (`next`), or with
/// `HOPS` the four before it (`links`), the nearest lowest.  The hash tables
/// have been zeroed, and so has the first of `links`.
///
/// An entry is a position plus 1 less `off`, and 0 stands for nothing.
/// `off` is 0 for as long as entries fit in 16 bits, and is raised by 32768
/// whenever they no longer do, every entry being reduced by as much.  A
/// candidate is within reach if it is at most 32767 back, as in libdeflate,
/// and what an entry of 0 has lost is further back than that.
///
/// Without `H3` the table of three-byte hashes is neither read nor written,
/// and no match is shorter than four bytes.
#[derive(Clone, Copy)]
pub(crate) struct Chain<const H3: bool, const HOPS: bool> {
    pub h3: *mut u16,
    pub h4: *mut u16,
    pub next: *mut u16,
    pub links: *mut u64,
    /// How many candidates to look at, and the length at which to stop.
    pub depth: u32,
    pub nice: usize,
}

impl<const H3: bool, const HOPS: bool> Chain<H3, HOPS> {
    /// The shortest match that is found.
    pub(crate) const SHORTEST: usize = if H3 { 3 } else { 4 };

    /// The hashes of the three and of the four bytes that `seq` begins
    /// with.
    #[inline(always)]
    fn hashes(seq: u32) -> (usize, usize) {
        (if H3 { lz_hash(seq & 0xFF_FFFF, 15) } else { 0 }, lz_hash(seq, 16))
    }

    /// What the tables hold for a position with the hashes `h`.
    #[inline(always)]
    unsafe fn heads(self, h: (usize, usize)) -> (usize, usize) {
        debug_assert!(h.0 < 1 << 15 && h.1 < 1 << 16);
        (if H3 { *self.h3.add(h.0) as usize } else { 0 }, *self.h4.add(h.1) as usize)
    }

    /// Is there certainly no match at the entry `e`, given what the tables
    /// hold for it?  With `H3`: its three bytes were not seen within reach,
    /// so its four were not either.
    #[inline(always)]
    fn none(heads: (usize, usize), e: usize) -> bool {
        (if H3 { heads.0 } else { heads.1 }) <= e.saturating_sub(WINDOW)
    }

    /// Makes `e` the head of its chains.  With `HOPS`, returns the four
    /// candidates that come after the old head.
    #[inline(always)]
    unsafe fn store(self, e: usize, heads: (usize, usize), h: (usize, usize)) -> u64 {
        debug_assert!((1..2 * WINDOW).contains(&e));
        if H3 {
            *self.h3.add(h.0) = e as u16;
        }
        *self.h4.add(h.1) = e as u16;
        if HOPS {
            // The link of `e` is the old head, then the first three of the
            // old head's.
            let ahead = *self.links.add(heads.1 & (WINDOW - 1));
            *self.links.add(e & (WINDOW - 1)) = ahead << 16 | heads.1 as u64;
            ahead
        } else {
            *self.next.add(e & (WINDOW - 1)) = heads.1 as u16;
            0
        }
    }

    /// Enters `e`, whose eight bytes are `wd`, and returns the longest
    /// match there that is longer than `longer`, as (length, distance); if
    /// there is none, a length no greater than `longer`.  `h` becomes the
    /// hashes of the entry after `e`.  Looked at are `self.depth >> look`
    /// candidates: `look` is 0, or 1 and 2 for the first and second position
    /// after a match.  `end` is the entry after the payload's last, and at
    /// least eight after `e`.
    ///
    /// libdeflate's `hc_matchfinder_longest_match`.  Nearly all of the walk
    /// along a chain is stepping past candidates that are no better than the
    /// best so far, so that is what a step is made of: one comparison of the
    /// four bytes that would make the candidate longer than the best (its
    /// first four, as long as there is no match of four), and what leads to
    /// the next candidate.  Only a candidate that passes is looked at
    /// further.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    unsafe fn find(
        self,
        ent: *const u8,
        end: usize,
        e: usize,
        wd: u64,
        heads: (usize, usize),
        h: &mut (usize, usize),
        longer: usize,
        look: u32,
    ) -> (usize, usize) {
        debug_assert!(e + 8 <= end);
        let (node3, node4) = heads;
        let mut ahead = self.store(e, heads, *h);
        *h = Self::hashes((wd >> 8) as u32);
        // Tables of this size are not at hand: what the next position will
        // need of them is asked for now.
        if H3 {
            fetch(self.h3.add(h.0));
        }
        fetch(self.h4.add(h.1));
        // The entries above `lim` stand for positions within reach.
        let lim = e.saturating_sub(WINDOW);
        let max_len = (end - e).min(MAX_MATCH);
        let nice_len = self.nice.min(max_len);
        let mut depth = self.depth >> look;
        let (mut best_len, mut best) = (longer, e);
        'out: {
            let seq4 = wd as u32;
            if best_len < 4 {
                if H3 {
                    if node3 <= lim {
                        break 'out;
                    }
                    if best_len < 3 && (at32(ent, node3) ^ seq4) & 0xFF_FFFF == 0 {
                        best_len = 3;
                        best = node3;
                    }
                }
                if node4 <= lim {
                    break 'out;
                }
            } else if node4 <= lim || best_len >= nice_len {
                break 'out;
            }
            let mut tail_at = best_len.max(3) - 3;
            let mut tail = raw_at32(ent, e + tail_at);
            let mut tails = ent.wrapping_add(tail_at);
            macro_rules! passes {
                ($node:expr) => {{
                    let node: usize = $node;
                    debug_assert!(node >= 1 && node < e && node + tail_at + 4 <= end);
                    ptr::read_unaligned(tails.wrapping_add(node) as *const u32) == tail
                }};
            }
            let mut node = node4;
            let mut pass = passes!(node);
            // With `HOPS`: the candidates in hand after `node`, how many
            // they are, and (`ahead`) those that come after them, asked for
            // as soon as it is known where they are.
            let (mut link, mut in_hand) = (0u64, 0u32);
            loop {
                if pass && at32(ent, node) == seq4 {
                    let len = extend_at(ent, end, node, e, 4, max_len);
                    if len > best_len {
                        best_len = len;
                        best = node;
                        if best_len >= nice_len {
                            break 'out;
                        }
                        tail_at = best_len - 3;
                        tail = raw_at32(ent, e + tail_at);
                        tails = ent.wrapping_add(tail_at);
                    }
                }
                depth -= 1;
                if depth == 0 {
                    break 'out;
                }
                // On to the next candidate that passes.
                if HOPS {
                    node = 'found: loop {
                        // What is left of the four in hand.
                        while in_hand != 0 {
                            let nd = (link & 0xFFFF) as usize;
                            link >>= 16;
                            in_hand -= 1;
                            if nd <= lim {
                                break 'out;
                            }
                            if passes!(nd) {
                                break 'found nd;
                            }
                            depth -= 1;
                            if depth == 0 {
                                break 'out;
                            }
                        }
                        // Four at a time, while more than four may still be
                        // looked at: nothing is counted on the way.
                        while depth > 4 {
                            link = ahead;
                            ahead = *self.links.add((link >> 48) as usize & (WINDOW - 1));
                            macro_rules! hop {
                                ($k:literal) => {
                                    let nd = (link & 0xFFFF) as usize;
                                    link >>= 16;
                                    if nd <= lim {
                                        break 'out;
                                    }
                                    if passes!(nd) {
                                        in_hand = 3 - $k;
                                        depth -= $k;
                                        break 'found nd;
                                    }
                                };
                            }
                            hop!(0);
                            hop!(1);
                            hop!(2);
                            hop!(3);
                            depth -= 4;
                        }
                        link = ahead;
                        ahead = *self.links.add((link >> 48) as usize & (WINDOW - 1));
                        in_hand = 4;
                    };
                } else {
                    loop {
                        node = *self.next.add(node & (WINDOW - 1)) as usize;
                        if node <= lim {
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
                }
                pass = true;
            }
        }
        (best_len, e - best)
    }

    /// Enters the `count` entries from `from` on, the last of which is
    /// below `2 * WINDOW` and has eight bytes of payload from it on.
    #[inline(always)]
    unsafe fn enter_run(self, ent: *const u8, h: &mut (usize, usize), from: usize, count: usize) {
        let mut hh = *h;
        let mut q = from;
        let stop = from + count;
        while q < stop {
            self.store(q, (0, *self.h4.add(hh.1) as usize), hh);
            hh = Self::hashes(at32(ent, q + 1));
            q += 1;
        }
        *h = hh;
        if H3 {
            fetch(self.h3.add(hh.0));
        }
        fetch(self.h4.add(hh.1));
    }

    /// Makes room for more entries: reduces every one by 32768.  The table
    /// of three-byte hashes too, whether this block uses it or not: a later
    /// one may.
    #[cold]
    #[inline(never)]
    unsafe fn slide(self) {
        // A link is four entries, whichever way round the machine keeps
        // them.
        let chain = if HOPS { (self.links.cast::<u16>(), 4 * WINDOW) } else { (self.next, WINDOW) };
        rebase(&[(self.h3, 1 << 15), (self.h4, 1 << 16), chain]);
    }

    /// A block of the payload `ip[..n]`, from where `st` says: what
    /// `levels::Search::greedy` (`PARSE` 0) and `lazy` (1, and 2 for looking
    /// two positions ahead) find.  Returns the number of matches; `st` then
    /// says where the next block begins.
    ///
    /// `min_len` is the shortest match worth taking, at least `SHORTEST`;
    /// `soft_end` is where the block ends at the latest; `split` is zeroed,
    /// and with `splitting` the block may end early.
    #[allow(clippy::too_many_arguments, unused_assignments)]
    pub(crate) unsafe fn search<const PARSE: u8>(
        self,
        ip: *const u8,
        n: usize,
        mut min_len: usize,
        sink: Sink,
        st: &mut Resume,
        split: *mut Split,
        splitting: bool,
        soft_end: usize,
    ) -> usize {
        debug_assert!(n > WINDOW && min_len >= Self::SHORTEST && st.pos < soft_end && soft_end <= n);
        let f_ll = sink.f_ll;
        // Everything that says where is an entry, and `ent` is the address
        // that the byte of entry 0 would have: nothing in a step has to do
        // with `off`.
        let mut off = st.win;
        let mut ent = ip.wrapping_add(off).wrapping_sub(1);
        let mut e = st.pos + 1 - off;
        let mut end = n + 1 - off;
        let mut soft = soft_end + 1 - off;
        // Where the block began, and where the literals begin that are not
        // yet part of a token.  Both can lie before entry 0 once `off` has
        // been raised, so they are only ever subtracted from, wrapping.
        let mut begin = e;
        let mut run_start = e;
        // The hashes of `e`.  Worked out here and not carried from the block
        // before, which may have kept only one of them.
        let mut h = if end - e >= 8 { Self::hashes(at32(ent, e)) } else { (0, 0) };
        let mut n_seqs = 0usize;
        // The literals before `run_start`.
        let mut lits = 0usize;
        // Where a search that looks ahead next counts the bytes in use.
        let mut recalc = if PARSE != 0 { e + (end - e).min(10000) } else { usize::MAX };
        // Steps are taken without a question below this entry: it is never
        // too far on, and is put right when it has been reached.
        let mut stop = 0usize;
        // The positions of a match are entered without one if they end at
        // or below this entry.
        let mut enter_end = (2 * WINDOW).min(end - 8);

        macro_rules! slide {
            () => {{
                self.slide();
                off += WINDOW;
                ent = ent.wrapping_add(WINDOW);
                e -= WINDOW;
                end -= WINDOW;
                soft -= WINDOW;
                recalc = recalc.wrapping_sub(WINDOW);
                begin = begin.wrapping_sub(WINDOW);
                run_start = run_start.wrapping_sub(WINDOW);
                // No further on than the entry that made this due.
                stop = stop.saturating_sub(WINDOW);
                enter_end = (2 * WINDOW).min(end - 8);
            }};
        }
        macro_rules! literal {
            ($b:expr) => {
                *f_ll.add($b as usize) += 1
            };
        }
        // Counts a match at the entry `$p` and writes it down.
        macro_rules! matched {
            ($p:expr, $len:expr, $dist:expr) => {{
                let (p, len, dist): (usize, usize, usize) = ($p, $len, $dist);
                debug_assert!((MIN_MATCH..=MAX_MATCH).contains(&len) && (1..WINDOW).contains(&dist));
                debug_assert!(n_seqs < sink.max_seqs);
                let run = p.wrapping_sub(run_start);
                let (t, slot) = token(run, len, dist);
                *f_ll.add(FIRST_LEN + *LEN_SLOT.as_ptr().add(len) as usize) += 1;
                *sink.f_off.add(slot) += 1;
                *sink.seqs.add(n_seqs) = t;
                n_seqs += 1;
                lits += run;
                run_start = p + len;
            }};
        }
        // Moves on past `$count` positions from `e`, entering them unless
        // the payload ends within eight bytes of them.
        macro_rules! skip {
            ($count:expr) => {{
                let count: usize = $count;
                if e + count <= enter_end {
                    self.enter_run(ent, &mut h, e, count);
                    e += count;
                } else if count + 8 > end - e {
                    e += count;
                } else {
                    // As far as entries fit in 16 bits, then the rest.
                    let mut left = count;
                    loop {
                        let now = left.min(2 * WINDOW - e);
                        self.enter_run(ent, &mut h, e, now);
                        e += now;
                        left -= now;
                        if left == 0 {
                            break;
                        }
                        slide!();
                    }
                }
            }};
        }

        'block: loop {
            if e >= stop {
                // Between two steps, where something may be due.  Does the
                // block end here: at `soft`, when the room for its matches
                // is used up, or where what it holds changes?
                let num_now = (lits + e.wrapping_sub(run_start) + n_seqs) as u32;
                let counts = &*(f_ll as *const [u32; NUM_LL]);
                if e >= soft
                    || n_seqs >= MAX_SEQS
                    || (splitting && (*split).ends_at_counted(counts, num_now, e.wrapping_sub(begin), end - e))
                {
                    break 'block;
                }
                if e == 2 * WINDOW {
                    slide!();
                }
                if end - e < 8 {
                    // The payload's last bytes: literals, and it is done.
                    while e < end {
                        literal!(*ent.wrapping_add(e));
                        e += 1;
                    }
                    break 'block;
                }
                if PARSE != 0 && e >= recalc {
                    min_len = later_min_len(counts, self.depth).max(Self::SHORTEST);
                    recalc += (end - recalc).min(e.wrapping_sub(begin));
                }
                // Up to where none of this can be due again: the end of the
                // block as far as it is known, the last position that has
                // eight bytes, the first entry that does not fit in 16 bits,
                // the next count of the bytes in use, a position before
                // which the block cannot have all the matches there is room
                // for, and the first at which it could be ended early.
                stop = soft.min(end - 7).min(2 * WINDOW).min(recalc).min(e + (MAX_SEQS - n_seqs));
                if splitting {
                    if let Some(at) = (*split).may_end_at_counted(num_now, e.wrapping_sub(begin), end - e) {
                        stop = stop.min(begin.wrapping_add(at));
                    }
                }
                debug_assert!(stop > e);
            }
            // A step at `e`.
            let mut wd = at64(ent, e);
            let mut heads = self.heads(h);
            if Self::none(heads, e) {
                // A literal, and the literals after it.
                loop {
                    self.store(e, heads, h);
                    h = Self::hashes((wd >> 8) as u32);
                    literal!(wd as u8);
                    e += 1;
                    if e >= stop {
                        continue 'block;
                    }
                    wd = at64(ent, e);
                    heads = self.heads(h);
                    if !Self::none(heads, e) {
                        break;
                    }
                }
            }
            let (len, dist) = self.find(ent, end, e, wd, heads, &mut h, min_len - 1, 0);
            if PARSE == 0 {
                if len >= min_len && (len > MIN_MATCH || dist <= 4096) {
                    matched!(e, len, dist);
                    e += 1;
                    skip!(len - 1);
                } else {
                    literal!(wd as u8);
                    e += 1;
                }
                continue 'block;
            }
            if len < min_len || (len == MIN_MATCH && dist > 8192) {
                literal!(wd as u8);
                e += 1;
                continue 'block;
            }
            // A match at e - 1.
            let (mut cur_len, mut cur_dist) = (len, dist);
            e += 1;
            // Is a match of `nl` bytes at distance `no` better than one of
            // `cl` at `co` by more than `by`?  A byte counts for four, and
            // so does a distance sixteen times shorter.
            let better = |nl: usize, no: usize, cl: usize, co: usize, by: i32| {
                nl >= cl && 4 * (nl - cl) as i32 + (no.leading_zeros() as i32 - co.leading_zeros() as i32) > by
            };
            // The longest match at `e` that is longer than `$longer`; `e`
            // moves on by one.  None if `e` is among the payload's last
            // seven bytes.
            macro_rules! look_ahead {
                ($longer:expr, $look:expr) => {{
                    if e == 2 * WINDOW {
                        slide!();
                    }
                    if end - e < 8 {
                        e += 1;
                        (0usize, 0usize)
                    } else {
                        let wd = at64(ent, e);
                        let found = self.find(ent, end, e, wd, self.heads(h), &mut h, $longer, $look);
                        e += 1;
                        found
                    }
                }};
            }
            loop {
                // A match of the length that was good enough where it was
                // found is taken as it is.
                if cur_len >= (end - (e - 1)).min(self.nice) {
                    matched!(e - 1, cur_len, cur_dist);
                    skip!(cur_len - 1);
                    break;
                }
                // The next position, with half the candidates.
                let (next_len, next_dist) = look_ahead!(cur_len - 1, 1);
                if better(next_len, next_dist, cur_len, cur_dist, 2) {
                    literal!(*ent.wrapping_add(e - 2));
                    (cur_len, cur_dist) = (next_len, next_dist);
                    continue;
                }
                if PARSE == 2 {
                    // And the one after it, with a quarter.
                    let (next_len, next_dist) = look_ahead!(cur_len - 1, 2);
                    if better(next_len, next_dist, cur_len, cur_dist, 6) {
                        literal!(*ent.wrapping_add(e - 3));
                        literal!(*ent.wrapping_add(e - 2));
                        (cur_len, cur_dist) = (next_len, next_dist);
                        continue;
                    }
                    matched!(e - 3, cur_len, cur_dist);
                    if cur_len > 3 {
                        skip!(cur_len - 3);
                    }
                } else {
                    matched!(e - 2, cur_len, cur_dist);
                    skip!(cur_len - 2);
                }
                break;
            }
        }
        st.pos = e - 1 + off;
        st.win = off;
        n_seqs
    }
}
