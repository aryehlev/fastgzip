//! Many payloads of many shapes through long-lived compressors, each output
//! decoded with flate2 (miniz_oxide) and compared with its input.
//!
//!     cargo run --profile checked --example stress -- <seed> <payloads> [dump.bin]
//!
//! With a third argument every output is also appended to that file, as
//! [u32 input length][u32 CRC-32 of the input][u32 output length][output],
//! for checking with other decoders.
#[path = "../tests/gen/mod.rs"]
mod gen;

use flate2::read::MultiGzDecoder;
use gen::{any_payload, bid_request, Rng};
use std::io::{Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seed: u64 = args.get(1).map_or(1, |s| s.parse().unwrap());
    let count: usize = args.get(2).map_or(100_000, |s| s.parse().unwrap());
    let mut dump = args.get(3).map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut gz: Vec<fastgzip::Gzip> = (0..4).map(|_| fastgzip::Gzip::new()).collect();
    let mut back = Vec::new();
    let (mut bytes_in, mut bytes_out) = (0u64, 0u64);
    // Phases, as traffic has them: a long stretch of one kind of request,
    // then a mix, then anything at all.
    let mut phase = 0;
    let mut left = 0;
    for i in 0..count {
        if left == 0 {
            phase = rng.below(6);
            left = 1 + rng.below(400);
        }
        left -= 1;
        let data = match phase {
            0 | 1 => {
                let size = 300 + rng.below(6000);
                bid_request(&mut rng, size, phase)
            }
            2 => {
                let kind = rng.below(4);
                let size = 200 + rng.below(9000);
                bid_request(&mut rng, size, kind)
            }
            3 => {
                // requests with a stretch of something else inside
                let size = 1000 + rng.below(5000);
                let mut d = bid_request(&mut rng, size, 0);
                let other = any_payload(&mut rng, 600);
                let at = rng.below(d.len());
                d.splice(at..at, other);
                d
            }
            4 => any_payload(&mut rng, 9000),
            _ => {
                let max = if rng.below(8) == 0 { 200_000 } else { 20_000 };
                any_payload(&mut rng, max)
            }
        };
        let which = if phase == 2 { rng.below(4) } else { 0 };
        let out = gz[which].compress(&data);
        back.clear();
        MultiGzDecoder::new(out).read_to_end(&mut back).unwrap_or_else(|e| panic!("payload {i}: {e}"));
        assert!(back == data, "payload {i} ({} bytes) did not come back", data.len());
        assert!(out.len() <= 18 + data.len() + 24 * (data.len() / 16384 + 1), "payload {i} grew");
        bytes_in += data.len() as u64;
        bytes_out += out.len() as u64;
        if let Some(f) = dump.as_mut() {
            f.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
            f.write_all(&crc(&data).to_le_bytes()).unwrap();
            f.write_all(&(out.len() as u32).to_le_bytes()).unwrap();
            f.write_all(out).unwrap();
        }
        if rng.below(5000) == 0 {
            gz[which] = fastgzip::Gzip::new();
        }
    }
    println!("ok: seed {seed}, {count} payloads, {bytes_in} -> {bytes_out} bytes, {:?}", gz[0].stats());
}

/// CRC-32 the slow, obvious way: not the code the crate uses.
fn crc(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xEDB8_8320 & (c & 1).wrapping_neg());
        }
    }
    !c
}
