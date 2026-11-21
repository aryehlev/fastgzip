//! Huffman codes: codeword lengths from symbol counts, and canonical
//! codewords from lengths.
//!
//! None of this runs for an ordinary payload.  Codes are made for the first
//! payload, then after 2, 4, 8 ... and finally every 64 payloads, and for a
//! payload that the shared codes do not fit.  It is still written to be
//! cheap: everything is on the stack in fixed arrays, the sort is one
//! `sort_unstable` of `u32` keys, and the tree is built in place in the sorted
//! array (Moffat and Katajainen), without nodes or a heap.

use crate::tables::NUM_LL;

/// Codeword lengths of at most `max_bits` for the symbols of `freqs`, into
/// `lens`.  A symbol that does not occur gets length 0, except that at least
/// two symbols always get a codeword: DEFLATE decoders want a complete code.
///
/// Every count must be below 2^23, and their sum below 2^32.
pub(crate) fn make_lens(freqs: &[u32], lens: &mut [u8], max_bits: usize) {
    let n = freqs.len();
    debug_assert!((2..=NUM_LL).contains(&n) && lens.len() == n && max_bits <= 15);
    lens.fill(0);

    // The symbols that occur, as count << 9 | symbol: sorting these numbers
    // sorts by count, and puts equal counts in symbol order.
    let mut key = [0u32; NUM_LL];
    let mut k = 0;
    for (sym, &f) in freqs.iter().enumerate() {
        debug_assert!(f < 1 << 23);
        key[k] = f << 9 | sym as u32;
        k += (f != 0) as usize; // no branch: counts of zero are overwritten
    }
    if k < 2 {
        let sym = if k == 1 { (key[0] & 511) as usize } else { 0 };
        lens[sym] = 1;
        lens[(sym == 0) as usize] = 1;
        return;
    }
    let key = &mut key[..k];
    key.sort_unstable();

    // In place: a[] holds counts, then the parent of each internal node,
    // then the depth of each internal node, then the depth of each leaf.
    let mut a = [0u32; NUM_LL];
    let a = &mut a[..k];
    for (w, &x) in a.iter_mut().zip(key.iter()) {
        *w = x >> 9;
    }
    a[0] += a[1];
    let (mut root, mut leaf) = (0, 2);
    for next in 1..k - 1 {
        if leaf >= k || a[root] < a[leaf] {
            a[next] = a[root];
            a[root] = next as u32;
            root += 1;
        } else {
            a[next] = a[leaf];
            leaf += 1;
        }
        if leaf >= k || (root < next && a[root] < a[leaf]) {
            a[next] += a[root];
            a[root] = next as u32;
            root += 1;
        } else {
            a[next] += a[leaf];
            leaf += 1;
        }
    }
    a[k - 2] = 0;
    for next in (0..k - 2).rev() {
        a[next] = a[a[next] as usize] + 1;
    }
    let (mut avail, mut used, mut depth) = (1usize, 0usize, 0u32);
    let (mut root, mut next) = (k as isize - 2, k as isize - 1);
    while avail > 0 {
        while root >= 0 && a[root as usize] == depth {
            used += 1;
            root -= 1;
        }
        while avail > used {
            a[next as usize] = depth;
            next -= 1;
            avail -= 1;
        }
        avail = 2 * used;
        depth += 1;
        used = 0;
    }

    // How many codewords of each length, with lengths above the limit cut
    // down to it.  That makes the code too full; give codewords up until it
    // is exactly full again, each time by lengthening the longest codeword
    // that is shorter than the limit and pairing it with one of the limit's.
    let mut count = [0u32; 16];
    for &d in a.iter() {
        count[(d as usize).min(max_bits)] += 1;
    }
    let mut total: u32 = (1..=max_bits).map(|l| count[l] << (max_bits - l)).sum();
    while total > 1 << max_bits {
        count[max_bits] -= 1;
        for l in (1..max_bits).rev() {
            if count[l] != 0 {
                count[l] -= 1;
                count[l + 1] += 2;
                break;
            }
        }
        total -= 1;
    }
    debug_assert_eq!(total, 1 << max_bits);

    // The rarest symbols come first in key[] and get the longest codewords.
    let mut i = 0;
    for l in (1..=max_bits).rev() {
        for &x in &key[i..i + count[l] as usize] {
            lens[(x & 511) as usize] = l as u8;
        }
        i += count[l] as usize;
    }
    debug_assert_eq!(i, k);
}

/// The canonical codewords for `lens[..n]`, each with its bits reversed:
/// DEFLATE fills bytes from the low bit and sends a codeword's first bit
/// first, so a reversed codeword can be OR-ed into the bit buffer as it is.
///
/// `const`, so that the fixed code's tables are made by the compiler.
pub(crate) const fn codewords(lens: &[u8], n: usize, out: &mut [u16]) {
    let mut count = [0u16; 16];
    let mut i = 0;
    while i < n {
        count[lens[i] as usize] += 1;
        i += 1;
    }
    count[0] = 0;
    let mut next = [0u16; 16];
    let mut code = 0u16;
    let mut bits = 1;
    while bits < 16 {
        code = (code + count[bits - 1]) << 1;
        next[bits] = code;
        bits += 1;
    }
    i = 0;
    while i < n {
        let l = lens[i] as usize;
        if l != 0 {
            out[i] = next[l].reverse_bits() >> (16 - l);
            next[l] += 1;
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kraft(lens: &[u8], max: usize) -> u32 {
        lens.iter().filter(|&&l| l != 0).map(|&l| 1u32 << (max - l as usize)).sum()
    }

    #[test]
    fn complete_and_limited() {
        let mut seed = 12345u64;
        let mut rnd = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        for case in 0..if cfg!(miri) { 40 } else { 3000 } {
            let n = 2 + (rnd() as usize % (NUM_LL - 1));
            let max = [7usize, 14, 15][case % 3];
            if n > 1 << max {
                continue;
            }
            let shape = rnd() % 5;
            let freqs: Vec<u32> = (0..n)
                .map(|i| match shape {
                    0 => rnd() % 1000,
                    1 => (rnd() % 4 == 0) as u32 * (rnd() % 50),
                    2 => 1u32 << (i % 23).min(22), // far apart: a deep tree
                    3 => (rnd() % 3 == 0) as u32,
                    _ => {
                        // Fibonacci-like counts make the deepest trees
                        let (mut x, mut y) = (1u32, 1u32);
                        for _ in 0..(i % 30) {
                            let z = x + y;
                            x = y;
                            y = z;
                        }
                        x.min((1 << 23) - 1)
                    }
                })
                .collect();
            let mut lens = vec![0u8; n];
            make_lens(&freqs, &mut lens, max);
            let used = freqs.iter().filter(|&&f| f != 0).count();
            assert!(lens.iter().all(|&l| (l as usize) <= max));
            for (sym, &f) in freqs.iter().enumerate() {
                if used >= 2 {
                    assert_eq!(f != 0, lens[sym] != 0, "case {case} symbol {sym}");
                } else if f != 0 {
                    assert_eq!(lens[sym], 1);
                }
            }
            assert_eq!(kraft(&lens, max), 1 << max, "case {case}: not a complete code");
            // A rarer symbol never has a shorter codeword.
            for i in 0..n {
                for j in 0..n {
                    if freqs[i] != 0 && freqs[j] != 0 && freqs[i] < freqs[j] {
                        assert!(lens[i] >= lens[j]);
                    }
                }
            }
            // The codewords are a prefix code.
            let mut cw = vec![0u16; n];
            codewords(&lens, n, &mut cw);
            let msb_first = |sym: usize| cw[sym].reverse_bits() >> (16 - lens[sym] as u32);
            let all: std::collections::HashSet<(u32, u16)> =
                (0..n).filter(|&s| lens[s] != 0).map(|s| (lens[s] as u32, msb_first(s))).collect();
            assert_eq!(all.len(), lens.iter().filter(|&&l| l != 0).count(), "case {case}");
            for &(l, code) in &all {
                for p in 1..l {
                    assert!(!all.contains(&(p, code >> (l - p))), "case {case}: not a prefix code");
                }
            }
        }
    }

    #[test]
    fn optimal_without_a_limit() {
        // Against the entropy bound: a Huffman code needs less than one bit
        // per symbol more than the entropy.
        let freqs: Vec<u32> = (1..=40).map(|i| i * i).collect();
        let mut lens = vec![0u8; 40];
        make_lens(&freqs, &mut lens, 15);
        let total: f64 = freqs.iter().map(|&f| f as f64).sum();
        let entropy: f64 = freqs.iter().map(|&f| f as f64 * (total / f as f64).log2()).sum();
        let cost: f64 = freqs.iter().zip(&lens).map(|(&f, &l)| f as f64 * l as f64).sum();
        assert!(cost >= entropy && cost < entropy + total);
    }
}
