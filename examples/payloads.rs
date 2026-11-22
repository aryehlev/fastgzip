//! Your own payloads through one compressor, in the order of the file: what
//! they compress to, how they were written, and how long a call takes on one
//! thread with nothing else going on.  Every output is decoded again with
//! flate2 and compared.
//!
//!     cargo run --release --example payloads -- requests.jsonl
//!
//! The file has one payload per line.  With a second argument `records` it is
//! [u32 length][bytes] repeated instead.  The environment variable LEVEL
//! chooses the level (0 without it).
use std::io::Read;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = args.get(1) else {
        eprintln!("usage: payloads <file> [records]");
        std::process::exit(1);
    };
    let data = std::fs::read(path).unwrap();
    let mut payloads: Vec<&[u8]> = Vec::new();
    if args.get(2).map(String::as_str) == Some("records") {
        let mut off = 0;
        while off + 4 <= data.len() {
            let n = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
            payloads.push(&data[off + 4..off + 4 + n]);
            off += 4 + n;
        }
    } else {
        payloads.extend(data.split(|&b| b == b'\n').filter(|l| !l.is_empty()));
    }
    assert!(!payloads.is_empty(), "no payloads in {path}");

    let level: u8 = std::env::var("LEVEL").map_or(0, |l| l.parse().unwrap());
    let mut gz = fastgzip::Gzip::with_level(level);
    let (mut bytes_in, mut bytes_out) = (0usize, 0usize);
    let mut back = Vec::new();
    for p in &payloads {
        let out = gz.compress(p);
        bytes_in += p.len();
        bytes_out += out.len();
        back.clear();
        flate2::read::MultiGzDecoder::new(out).read_to_end(&mut back).unwrap();
        assert!(back == *p, "a payload did not come back");
    }
    let stats = gz.stats();

    // Time: the lowest of several passes over the file, the compressor kept.
    let mut best = f64::MAX;
    for _ in 0..20 {
        let t = Instant::now();
        for p in &payloads {
            std::hint::black_box(gz.compress(p).len());
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    println!("level {level}: {} payloads, {} B on average", payloads.len(), bytes_in / payloads.len());
    println!(
        "output: {} B on average, {:.1}% of the input",
        bytes_out / payloads.len(),
        bytes_out as f64 * 100.0 / bytes_in as f64
    );
    println!("time:   {:.2} us per call", best / payloads.len() as f64 * 1e6);
    println!(
        "written in one pass: {}, searched first: {}, with codes of their own: {}, fixed codes: {}, stored: {}; codes made {} times",
        stats.one_pass, stats.two_pass, stats.own_codes, stats.fixed, stats.stored, stats.rebuilds
    );
}
