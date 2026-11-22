mod gen;

use fastgzip::Gzip;
use flate2::read::MultiGzDecoder;
use gen::{any_payload, request, Rng};
use std::io::Read;

/// Decodes with flate2 (miniz_oxide) and checks what gzip promises.
fn check(gz: &mut Gzip, data: &[u8]) -> usize {
    let level = gz.level();
    let out = gz.compress(data);
    let xfl = match level {
        0 | 1 => 4,
        2..=7 => 0,
        _ => 2,
    };
    assert_eq!(&out[..10], &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, xfl, 0xff], "level {level}");
    let mut back = Vec::with_capacity(data.len());
    MultiGzDecoder::new(out).read_to_end(&mut back).expect("not a gzip stream");
    assert!(back == data, "level {level}: {} bytes came back as {} other bytes", data.len(), back.len());
    // No larger than storing it: 18 bytes of gzip, and a few per block.
    assert!(out.len() <= 18 + data.len() + 6 * (data.len() / 5000 + 1), "level {level}");
    out.len()
}

fn requests(rng: &mut Rng, n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n + 6000);
    while v.len() < n {
        v.extend_from_slice(&request(rng, 3000, 0));
    }
    v.truncate(n);
    v
}

/// `n` bytes of any value, in which much has been seen before: pieces of 3
/// to 40 bytes from up to 40000 back, with up to four new bytes between
/// them.  What machine code is like to a search: every byte in use, and
/// matches of three bytes worth taking.
fn binary(rng: &mut Rng, n: usize) -> Vec<u8> {
    let mut v: Vec<u8> = (0..3000).map(|_| rng.u32() as u8).collect();
    while v.len() < n {
        let most = if rng.below(4) == 0 { 38 } else { 5 };
        let len = 3 + rng.below(most);
        let back = rng.below((v.len() - len).min(40_000));
        let from = v.len() - len - back;
        v.extend_from_within(from..from + len);
        let new = rng.below(5);
        for _ in 0..new {
            v.push(rng.u32() as u8);
        }
    }
    v.truncate(n);
    v
}

#[test]
fn levels() {
    assert_eq!(Gzip::new().level(), 0);
    for level in 0..=9 {
        assert_eq!(Gzip::with_level(level).level(), level);
    }
    assert!(std::panic::catch_unwind(|| Gzip::with_level(10)).is_err());
}

#[test]
#[cfg_attr(miri, ignore)]
fn empty_and_tiny() {
    for level in 1..=9 {
        let mut gz = Gzip::with_level(level);
        assert_eq!(check(&mut gz, b""), 20);
        for n in 0..300usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 7 % 251) as u8).collect();
            check(&mut gz, &data);
            check(&mut gz, &vec![b'x'; n]);
            check(&mut gz, &b"{\"id\":\"abc\",\"n\":12}".repeat(n / 19 + 1)[..n]);
        }
    }
}

#[test]
#[cfg_attr(miri, ignore)]
fn every_shape() {
    for level in 1..=9 {
        let mut rng = Rng(100 + level as u64);
        let mut gz = Gzip::with_level(level);
        for i in 0..700 {
            let data = any_payload(&mut rng, if i % 16 == 0 { 70_000 } else { 9000 });
            check(&mut gz, &data);
        }
    }
}

/// Each search has a plain form, which follows libdeflate line by line, and
/// the fast one that is used.  They must find the same matches: the output
/// is the same byte for byte.  Here for payloads of up to 32 KiB.
#[test]
#[cfg_attr(miri, ignore)]
fn fast_and_plain_searches_agree() {
    for level in 1..=9 {
        let mut rng = Rng(200 + level as u64);
        let mut fast = Gzip::with_level(level);
        let mut plain = Gzip::with_level(level);
        plain.__plain_search(true);
        let mut both = |data: &[u8]| {
            let a = fast.compress(data).to_vec();
            assert!(a == plain.compress(data), "level {level}, {} bytes", data.len());
        };
        for i in 0..400 {
            let size = 1 + rng.below(if i % 8 == 0 { 32_768 } else { 7000 });
            both(&request(&mut rng, size, i % 3));
            both(&any_payload(&mut rng, 6000));
        }
        // Sizes at which the tables change size, and the largest the fast
        // searches take; and payloads long enough for a block to be ended
        // early, with a change in what they hold to end it.
        for n in [255usize, 256, 257, 511, 512, 513, 4095, 4096, 4097, 16_384, 16_385, 32_767, 32_768] {
            both(&requests(&mut rng, n));
            let unit: Vec<u8> = (0..700).map(|_| b'a' + rng.below(26) as u8).collect();
            let mut data = unit.repeat(n / 700 + 1);
            data.truncate(n);
            both(&data);
        }
        for _ in 0..6 {
            let mut data = requests(&mut rng, 14_000);
            data.extend((0..9000).map(|_| rng.u32() as u8));
            data.extend(requests(&mut rng, 9000));
            both(&data);
        }
    }
}

/// Four bytes that libdeflate's hash takes to 0, whatever the table size.
/// libdeflate enters the first position of a payload under hash 0, so a
/// payload that begins with these is one in which that position can be
/// found again.
fn hashed_to_zero() -> [u8; 4] {
    (1u32..).find(|v| v.wrapping_mul(0x1E35_A7BD) >> 16 == 0).unwrap().to_le_bytes()
}

/// The same above 32 KiB, where the tables are searched through a window
/// that moves and a payload is more than one block.
#[test]
#[cfg_attr(miri, ignore)]
fn fast_and_plain_searches_agree_on_large_payloads() {
    for level in 1..=9 {
        let mut rng = Rng(250 + level as u64);
        let mut fast = Gzip::with_level(level);
        let mut plain = Gzip::with_level(level);
        plain.__plain_search(true);
        let mut both = |data: &[u8]| {
            let a = fast.compress(data).to_vec();
            assert!(a == plain.compress(data), "level {level}, {} bytes", data.len());
        };
        // Lengths around every place where a window ends or the tables are
        // counted anew, with requests, and with matches as long as they get
        // across those places.
        for n in [
            32_769usize, 32_773, 40_000, 65_530, 65_535, 65_536, 65_537, 65_541, 70_535, 98_300, 98_304,
            98_310, 131_071, 131_072, 131_080,
        ] {
            both(&requests(&mut rng, n));
            both(&vec![b'z'; n]);
            both(&vec![0u8; n]);
            let unit: Vec<u8> = (0..700).map(|_| b'a' + rng.below(26) as u8).collect();
            let mut data = unit.repeat(n / 700 + 1);
            data.truncate(n);
            both(&data);
        }
        // Other lengths, and a change in what the payload holds, where a
        // block is worth ending.
        for _ in 0..5 {
            let n = 33_000 + rng.below(250_000);
            let mut data = requests(&mut rng, n);
            both(&data);
            let from = n / 3;
            for b in &mut data[from..from + n / 4] {
                *b = rng.u32() as u8;
            }
            both(&data);
        }
        // The first position found again: at once, late in the first
        // window, and not before the second.
        let zero = hashed_to_zero();
        for at in [4usize, 9, 3000, 32_000, 32_764, 32_765, 32_770, 40_000] {
            let mut data = requests(&mut rng, 70_000);
            data[..4].copy_from_slice(&zero);
            for k in [at, at + 9, at + 400] {
                data[k..k + 4].copy_from_slice(&zero);
                data[k + 4..k + 12].copy_from_slice(&[b'#'; 8]);
            }
            data[4..12].copy_from_slice(&[b'#'; 8]);
            both(&data);
            both(&data[..(at + 500).max(600)]);
        }
        // Payloads that are searched for matches of three bytes, as a
        // whole and from the middle on, and one that is so only in the
        // middle.
        for n in [32_769usize, 65_541, 98_310, 150_000] {
            both(&binary(&mut rng, n));
            let mut data = requests(&mut rng, n / 2);
            data.extend_from_slice(&binary(&mut rng, n - n / 2));
            both(&data);
            data.extend_from_slice(&requests(&mut rng, 40_000));
            both(&data);
        }
        // Matches so short and so many that a block ends because the room
        // for them is used up: words of four letters out of 64, a byte of
        // anything between them.
        let words: Vec<[u8; 4]> = (0..64).map(|_| rng.u32().to_le_bytes()).collect();
        let mut data = Vec::with_capacity(400_000);
        while data.len() < 400_000 {
            data.extend_from_slice(&words[rng.below(64)]);
            data.push(rng.u32() as u8);
        }
        both(&data);
        // And more than one block of the greatest length.
        both(&requests(&mut rng, 650_000));
    }
}

/// At levels 1 to 9 a payload's output depends on nothing but the payload.
#[test]
#[cfg_attr(miri, ignore)]
fn output_does_not_depend_on_what_came_before() {
    let mut rng = Rng(300);
    let payloads: Vec<Vec<u8>> = (0..40)
        .map(|i| {
            let size = 200 + rng.below(9000);
            request(&mut rng, size, i % 4)
        })
        .collect();
    for level in 1..=9 {
        let mut gz = Gzip::with_level(level);
        let alone: Vec<Vec<u8>> =
            payloads.iter().map(|p| Gzip::with_level(level).compress(p).to_vec()).collect();
        for _ in 0..2 {
            for (p, want) in payloads.iter().zip(&alone).rev() {
                assert!(gz.compress(p) == &want[..], "level {level}");
            }
        }
    }
}

#[test]
#[cfg_attr(miri, ignore)]
fn higher_levels_are_not_larger_on_requests() {
    let mut rng = Rng(400);
    let payloads: Vec<Vec<u8>> = (0..60).map(|_| request(&mut rng, 5000, 0)).collect();
    let total = |level: u8| {
        let mut gz = Gzip::with_level(level);
        payloads.iter().map(|p| check(&mut gz, p)).sum::<usize>()
    };
    let (l1, l5, l9) = (total(1), total(5), total(9));
    assert!(l5 <= l1 && l9 <= l5 + l5 / 500, "{l1} {l5} {l9}");
    // and level 0 is within a few percent of level 1
    let mut gz = Gzip::new();
    let l0: usize = (0..4).map(|_| payloads.iter().map(|p| gz.compress(p).len()).sum::<usize>()).last().unwrap();
    assert!(l0 < l1 + l1 / 12, "{l0} {l1}");
}

#[test]
#[cfg_attr(miri, ignore)]
fn large_payloads() {
    for level in [1u8, 2, 4, 5, 7, 8, 9] {
        let mut rng = Rng(500 + level as u64);
        let mut gz = Gzip::with_level(level);
        for &n in &[32_769usize, 40_000, 65_535, 65_536, 70_000, 131_072, 200_000, 1 << 20] {
            let data = requests(&mut rng, n);
            let size = check(&mut gz, &data);
            assert!(size < n / 2, "level {level}: {n} bytes of requests became {size}");
            // nothing to find: stored, in blocks of at most 65535 bytes
            let noise: Vec<u8> = (0..n).map(|_| rng.u32() as u8).collect();
            check(&mut gz, &noise);
            // a change in the middle, where a block is worth ending
            let mut mixed = data;
            let from = n / 3;
            for b in &mut mixed[from..from + n / 3] {
                *b = rng.u32() as u8;
            }
            check(&mut gz, &mixed);
            // long runs, the longest matches there are
            check(&mut gz, &vec![b'z'; n]);
        }
        let s = gz.stats();
        assert!(s.own_codes > 0 && s.stored > 0, "{s:?}");
        assert_eq!(s.one_pass + s.two_pass + s.rebuilds, 0, "{s:?}");
    }
}

#[test]
#[cfg_attr(miri, ignore)]
fn block_edges() {
    // Matches that end at, just before and just after the end of the input,
    // at every length a match can have.
    for level in 1..=9 {
        let mut gz = Gzip::with_level(level);
        let mut rng = Rng(600 + level as u64);
        let unit: Vec<u8> = (0..700).map(|_| b'a' + rng.below(26) as u8).collect();
        for n in (1020..1300).chain(32_750..32_790).chain(65_520..65_550) {
            let mut data = unit.repeat(n / 700 + 1);
            data.truncate(n);
            check(&mut gz, &data);
            let k = data.len() - 1 - rng.below(12.min(n - 1));
            data[k] ^= 1;
            check(&mut gz, &data);
        }
    }
}

/// What a payload of more than 32 KiB takes that a smaller one does not: the
/// window of level 1 moving, the tables counted anew, chains walked four
/// steps at a time.  Mostly long runs, so that it is little work: small
/// enough for Miri.
#[test]
fn large_paths_once() {
    let mut rng = Rng(800);
    let mut data = request(&mut rng, 900, 0);
    data.resize(32_600, b'z');
    data.extend_from_slice(&request(&mut rng, 500, 1)); // across the end of the first window
    data.resize(65_300, b'y');
    data.extend_from_within(300..700); // and of the second, where entries are counted anew
    data.extend((0..200).map(|_| rng.u32() as u8));
    data.resize(66_400, b'x');
    // Each search above level 1 exists without and with matches of three
    // bytes, which the second payload has throughout, and with chains of
    // either kind.  Every one of those is run here.  Under Miri, which
    // takes minutes for each, every part of them is run once, and the last
    // on a payload that ends before the tables are full.
    let mut other = binary(&mut rng, 66_400);
    other.extend_from_within(65_000..65_300);
    let runs: &[(u8, bool, usize)] = if cfg!(miri) {
        &[(1, false, 0), (3, false, 0), (9, false, 0), (6, true, 0), (2, true, 33_000)]
    } else {
        &[
            (1, false, 0), (3, false, 0), (4, false, 0), (5, false, 0), (9, false, 0),
            (2, true, 0), (4, true, 0), (6, true, 0), (8, true, 0), (2, true, 33_000),
        ]
    };
    for &(level, with_three, cut) in runs {
        let mut gz = Gzip::with_level(level);
        let payload = if with_three { &other } else { &data };
        check(&mut gz, if cut == 0 { payload } else { &payload[..cut] });
        assert!(gz.stats().own_codes >= 1);
    }
}

/// Both ways of writing a block give the same bytes.
#[test]
#[cfg_attr(miri, ignore)]
fn both_writers_agree() {
    let mut rng = Rng(900);
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    for i in 0..40 {
        payloads.push(any_payload(&mut rng, i));
    }
    for n in [0usize, 1, 9, 300, 2000, 5000, 20_000, 40_000, 70_000] {
        payloads.push(requests(&mut rng, n));
        payloads.push((0..n).map(|i| b"ab,c"[(i * i / 7) % 4]).collect()); // matches without literals between
    }
    for level in [1u8, 2, 5, 9] {
        let (mut a, mut b) = (Gzip::with_level(level), Gzip::with_level(level));
        a.__writer_asks(true);
        b.__writer_asks(false);
        for p in &payloads {
            check(&mut a, p);
            assert!(a.compress(p) == b.compress(p), "level {level}, {} bytes", p.len());
        }
    }
}

/// Small enough for Miri: every level, each of its searches, each way of
/// writing, each kind of block.
#[test]
fn every_path_once() {
    let mut rng = Rng(700);
    for level in 1..=9 {
        let mut gz = Gzip::with_level(level);
        for plain in [false, true] {
            gz.__plain_search(plain);
            gz.__writer_asks(plain);
            check(&mut gz, &request(&mut rng, 1300, 0)); // codes of its own
            let noise: Vec<u8> = (0..600).map(|_| rng.u32() as u8).collect();
            check(&mut gz, &noise); // stored
            check(&mut gz, b"{\"id\":\"7\",\"ok\":true}"); // the fixed codes
            check(&mut gz, b"");
            check(&mut gz, &vec![b'z'; 700]); // the longest matches
        }
        let s = gz.stats();
        assert!(s.own_codes >= 2 && s.stored >= 2 && s.fixed >= 4, "{s:?}");
    }
}
