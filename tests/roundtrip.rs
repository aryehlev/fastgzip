mod gen;

use flate2::read::MultiGzDecoder;
use gen::{any_payload, bid_request, Rng};
use fastgzip::Gzip;
use std::io::Read;

/// Decodes with flate2 (miniz_oxide), an implementation that shares no code
/// with this crate, and checks what gzip promises.
fn check(gz: &mut Gzip, data: &[u8]) -> usize {
    let out = gz.compress(data);
    assert_eq!(&out[..10], &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 4, 0xff]);
    let mut back = Vec::with_capacity(data.len());
    MultiGzDecoder::new(out).read_to_end(&mut back).expect("not a gzip stream");
    assert!(back == data, "{} bytes came back as {} other bytes", data.len(), back.len());
    // No larger than storing it: 18 bytes of gzip, 5 per stored block.
    assert!(out.len() <= 18 + data.len() + 24 * (data.len() / 16384 + 1));
    out.len()
}

#[test]
#[cfg_attr(miri, ignore)] // too slow there; see every_path_once
fn empty_and_tiny() {
    let mut gz = Gzip::new();
    assert_eq!(check(&mut gz, b""), 20);
    for n in 0..300usize {
        let data: Vec<u8> = (0..n).map(|i| (i * 7 % 251) as u8).collect();
        check(&mut gz, &data);
        check(&mut gz, &vec![b'x'; n]);
        check(&mut gz, &b"{\"id\":\"abc\",\"n\":12}".repeat(n / 19 + 1)[..n]);
    }
}

#[test]
#[cfg_attr(miri, ignore)] // too slow there; see every_path_once
fn requests_share_codes() {
    let mut rng = Rng(1);
    let mut gz = Gzip::new();
    let mut total = 0;
    for _ in 0..600 {
        let size = 1500 + rng.below(4000);
        total += check(&mut gz, &bid_request(&mut rng, size, 0));
    }
    let s = gz.stats();
    assert!(s.one_pass >= 590, "{s:?}");
    assert_eq!(s.own_codes + s.fixed + s.stored, 0, "{s:?}");
    assert!(s.rebuilds <= 16, "{s:?}");
    assert!(total > 0);
}

#[test]
#[cfg_attr(miri, ignore)] // too slow there; see every_path_once
fn a_change_of_payloads() {
    // Codes made for one kind, then another kind, then bytes that have no
    // codeword at all: each must come back, and the codes must follow.
    let mut rng = Rng(2);
    let mut gz = Gzip::new();
    for round in 0..6 {
        for _ in 0..40 {
            check(&mut gz, &bid_request(&mut rng, 3000, round));
        }
        let binary: Vec<u8> = (0..3000).map(|_| 0x80 | rng.u32() as u8).collect();
        check(&mut gz, &binary); // seven bits of eight: codes of its own
        let noise: Vec<u8> = (0..3000).map(|_| rng.u32() as u8).collect();
        check(&mut gz, &noise); // nothing to gain: stored
        check(&mut gz, &vec![0u8; 5000]);
        let mut mixed = bid_request(&mut rng, 3000, round);
        mixed[1500] = 0xfe; // one byte the codes have never seen
        check(&mut gz, &mixed);
    }
    let s = gz.stats();
    assert!(s.one_pass > 150 && s.stored >= 6 && s.own_codes >= 6, "{s:?}");
}

#[test]
#[cfg_attr(miri, ignore)] // too slow there; see every_path_once
fn every_shape() {
    let mut rng = Rng(3);
    let mut gz = Gzip::new();
    for i in 0..6000 {
        let data = any_payload(&mut rng, if i % 16 == 0 { 70000 } else { 9000 });
        check(&mut gz, &data);
        if i % 500 == 0 {
            gz = Gzip::new();
        }
    }
}

#[test]
#[cfg_attr(miri, ignore)] // too slow there; see every_path_once
fn large_payloads() {
    let mut rng = Rng(4);
    let mut gz = Gzip::new();
    for &n in &[16383usize, 16384, 16385, 32768, 32769, 49152, 100_000, 1 << 20] {
        let mut data = Vec::with_capacity(n + 6000);
        while data.len() < n {
            data.extend_from_slice(&bid_request(&mut rng, 4000, 0));
        }
        data.truncate(n);
        let size = check(&mut gz, &data);
        assert!(size < n / 2, "{n} bytes of requests became {size}");
        // and with nothing to find
        let noise: Vec<u8> = (0..n).map(|_| rng.u32() as u8).collect();
        check(&mut gz, &noise);
    }
}

#[test]
#[cfg_attr(miri, ignore)] // too slow there; see every_path_once
fn block_edges() {
    // Matches that end at, just before and just after the end of the input
    // and of a block, at every length a match can have.
    let mut gz = Gzip::new();
    let mut rng = Rng(5);
    let unit: Vec<u8> = (0..700).map(|_| b'a' + rng.below(26) as u8).collect();
    for n in (1020..1300).chain(16370..16400).chain(32750..32790) {
        let mut data = unit.repeat(n / 700 + 1);
        data.truncate(n);
        check(&mut gz, &data);
        let k = data.len() - 1 - rng.below(12.min(n - 1));
        data[k] ^= 1;
        check(&mut gz, &data);
    }
}

#[test]
#[cfg_attr(miri, ignore)] // too slow there; see every_path_once
fn thread_local() {
    let mut rng = Rng(6);
    for _ in 0..50 {
        let data = bid_request(&mut rng, 4000, 0);
        let back = fastgzip::gzip_with(&data, |gz| {
            let mut back = Vec::new();
            MultiGzDecoder::new(gz).read_to_end(&mut back).unwrap();
            back
        });
        assert!(back == data);
    }
    assert!(fastgzip::local_stats().one_pass >= 45);
}

#[test]
fn send() {
    fn is_send<T: Send>() {}
    is_send::<Gzip>();
}

/// Every way a block can be written, with as few payloads as that takes:
/// small enough to run under Miri.
#[test]
fn every_path_once() {
    let mut rng = Rng(7);
    let mut gz = Gzip::new();
    for _ in 0..18 {
        check(&mut gz, &bid_request(&mut rng, 1300, 0)); // one pass, some counted
    }
    let mut odd = bid_request(&mut rng, 1300, 0);
    odd[700] = 0xf3;
    check(&mut gz, &odd); // a byte without a codeword
    check(&mut gz, &bid_request(&mut rng, 1300, 0));
    let noise: Vec<u8> = (0..1200).map(|_| rng.u32() as u8).collect();
    check(&mut gz, &noise); // stored
    let binary: Vec<u8> = (0..1200).map(|_| 0x80 | rng.u32() as u8 & 0x1f).collect();
    check(&mut gz, &binary); // codes of its own
    check(&mut gz, b"{\"id\":\"7\",\"ok\":true}"); // the fixed codes
    check(&mut gz, b"");
    check(&mut gz, &vec![b'z'; 700]); // the longest matches
    let s = gz.stats();
    assert!(s.one_pass >= 16 && s.two_pass >= 2 && s.own_codes >= 1, "{s:?}");
    assert!(s.fixed >= 2 && s.stored >= 1 && s.rebuilds >= 3, "{s:?}");

    // Payloads of more than one block: 16 KiB and a rest that continues it.
    let mut gz = Gzip::new();
    let mut long = Vec::new();
    for _ in 0..9 {
        long.clear();
        while long.len() < 19_000 {
            long.extend_from_slice(&bid_request(&mut rng, 1300, 0));
        }
        check(&mut gz, &long);
    }
    let s = gz.stats();
    assert!(s.one_pass >= 8 && s.rebuilds >= 4, "{s:?}");
    long[17_000] = 0xf4; // the second block cannot continue the first
    check(&mut gz, &long);
    for b in &mut long[16_384..] {
        *b = rng.u32() as u8; // nor here, and it is stored
    }
    check(&mut gz, &long);
    assert!(gz.stats().stored >= 1, "{:?}", gz.stats());
}

/// Large payloads: what the blocks of one have to get right between them.
#[test]
#[cfg_attr(miri, ignore)]
fn large_payload_paths() {
    let mut rng = Rng(8);
    let request = |rng: &mut Rng, n: usize| {
        let mut v = Vec::with_capacity(n + 6000);
        while v.len() < n {
            v.extend_from_slice(&bid_request(rng, 3000, 0));
        }
        v.truncate(n);
        v
    };
    // As the very first payload: no codes yet when its first block is written.
    let mut gz = Gzip::new();
    let first = request(&mut rng, 70_000);
    let size = check(&mut gz, &first);
    // Matches reach back into the block before: one header, and a second
    // copy of the same request costs next to nothing.
    let twice = [&first[..16_000], &first[..16_000]].concat();
    let once = check(&mut gz, &first[..16_000]);
    assert!(check(&mut gz, &twice) < once + 1500);
    assert!(size < 70_000 * 45 / 100, "{size}");
    // The codes come due in the middle of a payload, at every period.
    let before = gz.stats().rebuilds;
    for _ in 0..40 {
        let n = 30_000 + rng.below(40_000);
        check(&mut gz, &request(&mut rng, n));
    }
    let s = gz.stats();
    assert!(s.rebuilds >= before + 4 && s.one_pass >= 100, "{s:?}");
    // A byte without a codeword, noise, and another kind of text, in every
    // block of a payload in turn.
    for which in 0..5 {
        for kind in 0..3 {
            let mut data = request(&mut rng, 5 * 16384 - 100);
            let (from, to) = (which * 16384, data.len().min((which + 1) * 16384));
            match kind {
                0 => data[from + 3000] = 0xf5 + which as u8,
                1 => data[from..to].iter_mut().for_each(|b| *b = rng.u32() as u8),
                _ => data[from + 2000..to - 2000].iter_mut().for_each(|b| *b = b"QWXZ@#~^"[rng.below(8)]),
            }
            check(&mut gz, &data);
            check(&mut gz, &request(&mut rng, 40_000));
        }
    }
    let s = gz.stats();
    assert!(s.stored >= 5 && s.two_pass + s.own_codes >= 10, "{s:?}");
    // Sizes around the block boundaries, short last blocks included.
    for n in [16385usize, 16390, 16400, 17_000, 32_767, 32_768, 32_769, 32_780, 49_152, 49_153] {
        check(&mut gz, &request(&mut rng, n));
        let mut noise: Vec<u8> = (0..n).map(|_| rng.u32() as u8).collect();
        check(&mut gz, &noise);
        noise[..16_000].copy_from_slice(&first[..16_000]);
        check(&mut gz, &noise);
    }
}
