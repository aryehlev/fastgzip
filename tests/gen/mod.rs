//! Payload generators shared by the tests and the examples.
#![allow(dead_code)]

pub struct Rng(pub u64);

impl Rng {
    pub fn u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    pub fn below(&mut self, n: usize) -> usize {
        ((self.u32() as u64 * n as u64) >> 31) as usize
    }
    pub fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
    pub fn hex(&mut self, out: &mut Vec<u8>, n: usize) {
        for _ in 0..n {
            out.push(b"0123456789abcdef"[self.below(16)]);
        }
    }
}

const SITES: &[&str] = &["news.example.com", "sport.example.org", "video.example.net", "mail.example.com"];
const UAS: &[&str] = &[
    "Mozilla/5.0 (Linux; Android 14; SM-S918B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
];
const CATS: &[&str] = &["IAB1", "IAB2-3", "IAB9-30", "IAB12", "IAB17-44", "IAB19"];
const GEO: &[&str] = &["USA", "DEU", "GBR", "FRA", "ISR", "BRA", "IND"];

/// Something shaped like an OpenRTB bid request, of about `size` bytes.
/// `kind` changes the vocabulary, as a different exchange would.
pub fn bid_request(rng: &mut Rng, size: usize, kind: usize) -> Vec<u8> {
    let k = ["", "x_", "pub", "zz"][kind % 4];
    let mut v = Vec::with_capacity(size + 512);
    v.extend_from_slice(format!("{{\"{k}id\":\"").as_bytes());
    rng.hex(&mut v, 32);
    v.extend_from_slice(b"\",\"at\":1,\"tmax\":");
    v.extend_from_slice(format!("{}", 80 + rng.below(200)).as_bytes());
    v.extend_from_slice(b",\"cur\":[\"USD\"],\"imp\":[");
    let mut first = true;
    while v.len() + 700 < size || first {
        if !first {
            v.push(b',');
        }
        first = false;
        let (w, h) = [(300, 250), (728, 90), (320, 50), (160, 600)][rng.below(4)];
        v.extend_from_slice(
            format!(
                "{{\"{k}id\":\"{}\",\"tagid\":\"slot-{}\",\"secure\":1,\"bidfloor\":{}.{:02},\"bidfloorcur\":\"USD\",\"banner\":{{\"w\":{w},\"h\":{h},\"pos\":{},\"format\":[{{\"w\":{w},\"h\":{h}}},{{\"w\":{},\"h\":{}}}],\"battr\":[{},{}],\"api\":[3,5,7]}},\"ext\":{{\"{k}gpid\":\"/{}/{}/home\",\"data\":{{\"pbadslot\":\"/{}/top\"}}}}}}",
                rng.below(9) + 1,
                rng.below(5000),
                rng.below(3),
                rng.below(100),
                rng.below(8),
                w / 2,
                h,
                rng.below(17) + 1,
                rng.below(17) + 1,
                rng.below(90000) + 10000,
                rng.pick(SITES),
                rng.below(90000) + 10000,
            )
            .as_bytes(),
        );
    }
    let site = rng.pick(SITES);
    v.extend_from_slice(
        format!(
            "],\"site\":{{\"id\":\"{}\",\"domain\":\"{site}\",\"page\":\"https://{site}/{}/{}-{}.html?utm_source=feed&ref={}\",\"cat\":[\"{}\",\"{}\"],\"publisher\":{{\"id\":\"{}\"}}}},\"device\":{{\"ua\":\"{}\",\"ip\":\"{}.{}.{}.{}\",\"geo\":{{\"country\":\"{}\",\"lat\":{}.{:04},\"lon\":{}.{:04},\"type\":2}},\"devicetype\":{},\"os\":\"{}\",\"language\":\"en\",\"ifa\":\"",
            rng.below(100000),
            rng.pick(&["world", "sport", "tech", "money"]),
            rng.pick(&["article", "story", "live"]),
            rng.below(1000000),
            rng.below(100000),
            rng.pick(CATS),
            rng.pick(CATS),
            rng.below(10000),
            rng.pick(UAS),
            rng.below(223) + 1,
            rng.below(256),
            rng.below(256),
            rng.below(256),
            rng.pick(GEO),
            rng.below(90),
            rng.below(10000),
            rng.below(180),
            rng.below(10000),
            rng.below(5) + 1,
            rng.pick(&["Android", "iOS", "Windows"]),
        )
        .as_bytes(),
    );
    rng.hex(&mut v, 8);
    v.push(b'-');
    rng.hex(&mut v, 4);
    v.push(b'-');
    rng.hex(&mut v, 12);
    v.extend_from_slice(b"\"},\"user\":{\"id\":\"");
    rng.hex(&mut v, 24);
    v.extend_from_slice(b"\",\"buyeruid\":\"");
    rng.hex(&mut v, 36);
    v.extend_from_slice(b"\",\"ext\":{\"consent\":\"");
    for _ in 0..60 + rng.below(80) {
        v.push(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"[rng.below(64)]);
    }
    v.extend_from_slice(b"\"}},\"regs\":{\"ext\":{\"gdpr\":1}},\"source\":{\"tid\":\"");
    rng.hex(&mut v, 32);
    v.extend_from_slice(b"\",\"fd\":1}}");
    v
}

/// A payload of any of several shapes, most of them unlike a request, of
/// at most about `max` bytes.
pub fn any_payload(rng: &mut Rng, max: usize) -> Vec<u8> {
    let n = match rng.below(8) {
        0 => rng.below(12),
        1 => rng.below(64),
        2 => rng.below(1024.min(max) + 1),
        _ => rng.below(max + 1),
    };
    let mut v = Vec::with_capacity(n + 16);
    match rng.below(9) {
        0 => {
            // random bytes: nothing to find
            while v.len() < n {
                v.push(rng.u32() as u8);
            }
        }
        1 => {
            // one byte, or a short period, repeated: the longest matches
            let period = 1 + rng.below(9);
            let unit: Vec<u8> = (0..period).map(|_| rng.u32() as u8).collect();
            while v.len() < n {
                v.push(unit[v.len() % period]);
            }
        }
        2 => {
            // few symbols, no structure
            let alphabet = 2 + rng.below(6);
            while v.len() < n {
                v.push(b'a' + rng.below(alphabet) as u8);
            }
        }
        3 => {
            // random bytes with copies of earlier stretches at all distances
            while v.len() < n {
                if v.len() > 8 && rng.below(3) == 0 {
                    let from = rng.below(v.len() - 4);
                    let len = 3 + rng.below(300);
                    for i in 0..len {
                        let b = v[from + i % (v.len() - from)];
                        v.push(b);
                    }
                } else {
                    for _ in 0..1 + rng.below(20) {
                        v.push(rng.u32() as u8);
                    }
                }
            }
            v.truncate(n);
        }
        4 => {
            // text with every byte value somewhere
            while v.len() < n {
                if rng.below(40) == 0 {
                    v.push(rng.u32() as u8);
                } else {
                    v.extend_from_slice(rng.pick(&["alpha ", "beta ", "gamma,", "\"delta\":", "{", "}", "0", "17", "\n"]).as_bytes());
                }
            }
            v.truncate(n);
        }
        5 => {
            // base64-like tokens
            while v.len() < n {
                v.push(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"[rng.below(64)]);
            }
        }
        _ => {
            let kind = rng.below(4);
            v = bid_request(rng, n, kind);
            v.truncate(n.max(1));
        }
    }
    v
}
