//! How long does one gzip call take inside a tokio server?
//!
//!   tokio_bench <file> <payload_bytes|0|lines> <requests> <workers> <other_work_bytes> <mode> [group]
//!
//! <file> is cut into payloads of <payload_bytes>; with 0 it is a file of
//! records ([u32 length][bytes] repeated); with "lines" every line of it is a
//! payload (your own requests, one per line).
//!
//! Every "request" does some unrelated work on the worker thread (walks
//! `other_work_bytes` of that thread's scratch memory, as request parsing and
//! business logic would).  Then, for each of its [group] payloads (default
//! 1), it makes a fresh copy of the payload, as a serializer would, compresses
//! it and times only that:
//!
//!   local     fastgzip::gzip_with on the worker thread
//!   fresh     a new fastgzip::Gzip for every payload (what not to do)
//!   blocking  gzip_with moved to tokio's blocking pool with spawn_blocking

use std::cell::RefCell;
use std::time::Instant;

thread_local! {
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn other_work(bytes: usize) -> u32 {
    if bytes == 0 {
        return 0;
    }
    SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        if s.len() != bytes {
            s.resize(bytes, 1);
        }
        let mut sum = 0u32;
        let mut i = 0;
        while i < bytes {
            s[i] = s[i].wrapping_add(1);
            sum = sum.wrapping_add(s[i] as u32);
            i += 64;
        }
        sum
    })
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 7 {
        eprintln!("usage: tokio_bench <file> <payload_bytes|0|lines> <requests> <workers> <other_work_bytes> <local|fresh|blocking> [group]");
        std::process::exit(1);
    }
    let data: &'static [u8] = Box::leak(std::fs::read(&a[1]).unwrap().into_boxed_slice());
    let lines = a[2] == "lines";
    let size: usize = if lines { 0 } else { a[2].parse().unwrap() };
    let requests: usize = a[3].parse().unwrap();
    let workers: usize = a[4].parse().unwrap();
    let other: usize = a[5].parse().unwrap();
    let mode: &'static str = Box::leak(a[6].clone().into_boxed_str());
    let group: usize = a.get(7).map_or(1, |g| g.parse().unwrap());
    let mut list: Vec<&'static [u8]> = Vec::new();
    if lines {
        list.extend(data.split(|&b| b == b'\n').filter(|l| !l.is_empty()));
    } else if size == 0 {
        let mut off = 0;
        while off + 4 <= data.len() {
            let n = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
            list.push(&data[off + 4..off + 4 + n]);
            off += 4 + n;
        }
    } else {
        list.extend(data.chunks_exact(size));
    }
    let payloads: &'static [&'static [u8]] = Box::leak(list.into_boxed_slice());
    let groups = payloads.len() / group;
    let tasks = workers * 4;
    let per_task = requests / tasks;

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(workers).build().unwrap();
    let wall = Instant::now();
    let (mut times, in_bytes, out_bytes) = rt.block_on(async move {
        let mut handles = Vec::new();
        for t in 0..tasks {
            handles.push(tokio::spawn(async move {
                let mut times = Vec::with_capacity(per_task * group);
                let (mut in_bytes, mut out_bytes) = (0usize, 0usize);
                for r in 0..per_task {
                    std::hint::black_box(other_work(other));
                    let first = (t * 7919 + r) % groups * group;
                    for k in 0..group {
                        let payload = payloads[first + k].to_vec();
                        in_bytes += payload.len();
                        let t0 = Instant::now();
                        let n = match mode {
                            "local" => fastgzip::gzip_with(&payload, |gz| gz.len()),
                            "fresh" => fastgzip::Gzip::new().compress(&payload).len(),
                            "blocking" => tokio::task::spawn_blocking(move || fastgzip::gzip_with(&payload, |gz| gz.len()))
                                .await
                                .unwrap(),
                            _ => panic!("mode must be local, fresh or blocking"),
                        };
                        times.push(t0.elapsed().as_nanos() as u64);
                        out_bytes += n;
                    }
                    tokio::task::yield_now().await;
                }
                (times, in_bytes, out_bytes)
            }));
        }
        let mut times = Vec::new();
        let (mut in_bytes, mut out_bytes) = (0usize, 0usize);
        for h in handles {
            let (t, i, o) = h.await.unwrap();
            times.extend(t);
            in_bytes += i;
            out_bytes += o;
        }
        (times, in_bytes, out_bytes)
    });
    let wall = wall.elapsed().as_secs_f64();
    times.sort_unstable();
    let n = times.len();
    let mean = times.iter().sum::<u64>() as f64 / n as f64 / 1e3;
    println!(
        "fastgzip {} | {} payloads of {} B avg, {} per request | workers {} | other work {} KiB | median {:.2} us  mean {:.2}  p90 {:.2} | out {:.1}% of in | {:.0} payloads/s",
        mode,
        n,
        in_bytes / n,
        group,
        workers,
        other >> 10,
        times[n / 2] as f64 / 1e3,
        mean,
        times[n * 9 / 10] as f64 / 1e3,
        out_bytes as f64 * 100.0 / in_bytes as f64,
        n as f64 / wall
    );
}
