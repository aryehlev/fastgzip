# fastgzip

[![CI](https://github.com/aryehlev/fastgzip/actions/workflows/ci.yml/badge.svg)](https://github.com/aryehlev/fastgzip/actions/workflows/ci.yml)

gzip for a stream of small, similar payloads (JSON requests of a few
kilobytes) on a hot path. Pure Rust: no C, no build script, one dependency
(`crc32fast`). The output is ordinary gzip.

```rust
// on the worker thread, inside the task
fastgzip::gzip_with(&payload, |gz| body.extend_from_slice(gz));
```

or with a compressor of your own (one per kind of payload, see below):

```rust
let mut gz = fastgzip::Gzip::new();
let bytes: &[u8] = gz.compress(&payload); // valid until the next call
```

## What it costs

Microseconds per call inside tokio (2 workers), on a GitHub arm64 runner
(Neoverse N2), next to libdeflate at level 1. "Other work" is memory the
worker thread walks between two calls, which is what decides how much of the
compressor is still in the caches when the next payload arrives.

| payloads | other work | libdeflate, level 1 | fastgzip | fastgzip's share of libdeflate's time |
|---|---|---|---|---|
| synthetic JSON requests, 3.7 KB | none | 17.34 | **5.17** | 29.8% |
| | 4 MiB | 18.11 | **5.46** | 30.1% |
| | 64 MiB | 20.11 | **6.47** | 32.2% |
| real JSON requests of one kind, 1.8 KB, 62% of each a base64 field | none | 9.54 | **2.47** | 25.9% |
| | 4 MiB | 10.47 | **2.73** | 26.1% |
| | 64 MiB | 12.28 | **3.42** | 27.9% |
| six of the synthetic requests in an array, 22 KB | none | 78.47 | **27.31** | 34.8% |
| | 4 MiB | 79.61 | **27.73** | 34.8% |
| | 64 MiB | 81.34 | **29.26** | 36.0% |

The output is 2.1%, 3.0% and 1.2% larger than libdeflate's for these three
kinds; see Output size for others.

On an x86-64 runner (AMD EPYC 9V74) the 3.7 KB requests take 17.81 / 18.99 /
30.00 with libdeflate and 5.51 / 5.86 / 9.42 with fastgzip.

Each figure is the lowest median of five runs.

Your payloads will differ. `cargo run --release --example payloads --
requests.jsonl` runs a file of them (one per line) through one compressor and
prints the output size, the time per call and how the blocks were written;
`examples/tokio_bench.rs` does the timing inside tokio with other work in
between.

## Using it well

**One compressor per kind of payload.** Consecutive payloads of one `Gzip`
share their Huffman codes. Requests of one protocol are one kind as far as
that goes, also when they come from different senders with their own
dialects (one of the sets under Output size is 70 such requests on one
compressor). Payloads as different as JSON and protobuf would not be: give
each its own `Gzip`. `gzip_with` uses one compressor per thread for
everything it is given.

**Look at `stats()` once.** `Gzip::stats()` (and `local_stats()` for the
thread-local one) says how the blocks were written. In steady state nearly
all of them should be `one_pass`. If `own_codes` keeps growing, the payloads
are not of one kind, and the calls that hit it cost twice as much or more.

**The output depends on what came before.** Each output is a complete gzip
stream that any decoder reads on its own, but the same payload compressed
twice gives different bytes, because the codes have moved in between. Do not
compare outputs byte for byte in tests; decompress them.

**Memory.** A `Gzip` is about 75 KiB and its output buffer, which grows to
about three times the largest payload it has seen (for payloads over 16 KiB:
their size and 32 KiB) and is reused. Nothing is allocated per call. A
`Gzip` is `Send`.

**Not on a blocking pool.** A call takes microseconds; `spawn_blocking` costs
more than that, and puts the payload on a thread whose compressor has other
codes.

**Taking the bytes.** They are in the compressor's buffer until the next
call. Copy them to where they have to go (`body.extend_from_slice(gz)`,
`Bytes::copy_from_slice(gz)`): copying a kilobyte or two is nothing next to
the call.

## How it is made fast, part by part

A general DEFLATE compressor reads a payload twice. First it searches for
repetitions and counts the symbols it would write; then it makes Huffman
codes from the counts and goes over the payload's items again to write them.
For a payload of a few kilobytes, making the codes is a large part of the
whole.

**The codes exist before the payload does.** They are made from the payloads
that came before, so a payload is written while it is searched: one pass, and
no codes to make. This is where most of the time goes away. The price is
that the codes are not exactly this payload's, which is about half of the
difference in output size. Nothing else is carried from one payload to the
next: no dictionary, no cache of earlier outputs.

**The checksum comes first** (`lib.rs`, `compress`). gzip wants a CRC-32 of
the input. `crc32fast` computes it with carry-less multiplication where the
processor has it, on arm64 and on x86-64; doing it before the search also
means that the search finds the payload in the cache.

**Room for the worst case is reserved once** (`compress`). Then nothing in
the encoder tests for space: a flush is a plain 8-byte store. The buffer
belongs to the compressor, so after the first few calls `reserve` is a
comparison.

**The search** (`enc.rs`, `scan`):

- One earlier position per hash value, and a table with about one entry per
  input byte: 8 KiB for a payload of up to 4096 bytes. Small enough to live
  in the first-level cache.
- The table is zeroed at the start of every payload. That is cheap at this
  size, and it matters when the thread has done other work since its last
  payload: zeroing walks the table front to back, which the hardware fetches
  far faster than the scattered first touches of the lookups. Keeping the
  table from call to call instead, to save the clearing, loses more than it
  saves as soon as the caches are cold.
- A position is an offset into the payload, so an empty entry reads as
  position 0. The bytes there are compared like any other candidate's. No
  entry is tested for being empty, and the only branch in a step is the one
  taken for a match.
- A step reads eight bytes at the current position, once. The low byte is the
  literal, the low four bytes are compared with the candidate's, and bytes 1
  to 5 are hashed for the next position, so that the next table lookup can
  start before the next step does.
- Five bytes are hashed, not three or four. A repetition of four bytes saves
  next to nothing in the output, and not looking for it means fewer false
  candidates.
- The hash is one multiplication and one shift: the shift that would drop
  the three bytes that are not hashed is folded into the multiplier.
- Eight bytes are read at the candidate too, though four decide. When they
  decide for a match, the XOR that was computed for the comparison already
  says how long a match of up to seven bytes is, which most are.
- Inside a match only three positions go into the table: the two after its
  start, whose hashes come out of the word already read, and the last one,
  whose word also gives the hash for the position after the match. One load
  per match instead of one per position.
- One bound check per four steps.
- The table size is a constant in each compiled copy of the loop (a const
  generic), so the hash shift is an immediate and the clearing has a fixed
  size.

**Writing** (`scan`, `codes.rs`):

- Everything to be written for a byte is one 32-bit table entry: codeword in
  the low half, its length in the high half. The same for a match length
  (codeword and extra bits already combined) and an offset slot. libdeflate
  keeps codewords and lengths in separate arrays and looks the length slot up
  in a third.
- No literal or length codeword is longer than 14 bits (the format allows
  15). Then four literals and the up to 7 bits left over from the last flush
  always fit in the 64-bit buffer, so the steps come four to a flush, with no
  test for room.
- A byte the codes have no codeword for is not tested for where it is
  written. Its table entry adds 2^15 to the bit count; shifts only use the
  count modulo 64, a flush keeps everything from bit 15 up, and the mark is
  looked at once, when the block is done.
- A match is assembled into one value of at most 47 bits and OR-ed in once.
  The offset slot and its extra bits are arithmetic on the bit length of the
  distance, not a table lookup.
- The block header is made when the codes are, and copied: 128 bytes with a
  fixed-size copy, which is a few vector moves.

**Keeping the codes current** (`lib.rs`):

- Codes are remade after the 1st, 2nd, 6th, 14th ... payload and then every
  1024 payloads. Early on new codes pay at once. Later they change little:
  making them every 64 payloads and counting one payload in eight cost 4 to
  8% more time on the N2 runner for no gain in size.
- Between two makings, about 32 payloads have their symbols counted while
  they are written (one extra increment per symbol; 15 to 25% on those
  payloads). The counts go into the next codes, and they say whether the
  current codes still fit: no code can need fewer bits than the entropy of
  the symbols, and codes that need 3% more than that are a bad fit.
- Old payloads fade: the counts are halved each time codes are made.
- Every printable ASCII byte, every match length and every offset has a
  codeword even if it has not occurred yet, so a byte that appears for the
  first time rarely sends a payload down the slow path.

**When the codes do not suit a payload** (`two_pass`). The first payload, a
payload with a byte that has no codeword, one that the codes fit badly, and
every payload under 1 KiB (where the block header is a large part of the
output, and the fixed codes of the format may do better) are searched first,
with the matches recorded and the symbols counted, and then written with
whichever is smallest: the shared codes, codes of their own, the fixed codes,
or stored. After three bad fits in a row the shared codes are remade from the
recent payloads.

**Payloads over 16 KiB** (`large`, `chunk`) are done 16 KiB at a time, each
piece searching the piece before it as well (between pieces the table's
positions are moved down, with vector instructions), and all pieces that can
be written in one pass form a single DEFLATE block with one header.

**Making codes** (`huff.rs`, `codes.rs`) is the one slow thing left: 3 to 5
microseconds on x86-64, about what a payload takes. It runs once per 1024
payloads. It is still without allocation: fixed arrays on the stack, one
sort of `u32` keys, the Huffman tree built in place in the sorted array, and
the tables of the fixed codes made by the compiler.

**Cold caches** (`fetch_encoder`). A worker that did megabytes of other work
since its last payload has lost the compressor from its caches. The table is
zeroed, not fetched; everything a compressor keeps is one allocation, with
what every call reads at the front; and on arm64 the encoder's machine code
is asked for before the checksum is computed, so that it arrives while there
is other work to do.

### What was tried and did not help

Measured on the Neoverse N2 runner unless noted, against the encoder as it
is:

- **Looking up only every other position** after 16 bytes without a match
  (measured on x86-64): 4% faster on the synthetic requests and 15% on the
  ones that are mostly base64, for 1.8% and 1.2% more output. Not taken: it
  buys time with size.
- **Entering the last two positions of a match** into the table instead of
  the last one: 1% slower; output 0.1% smaller on the synthetic requests,
  0.6% larger on very repetitive JSON.
- **A 4 KiB table** for payloads of 1 to 2 KiB instead of 8 KiB: the same
  time, with cold caches too, and 0.1% more output.
- **Hashing a position when it is looked up**, not one step ahead, which is
  one instruction less per step: 1% slower.
- **Eight steps per loop iteration**; **one flush for a match and the
  literals before it** when both fit in the buffer; **a branch of its own for
  a match that directly follows a match**: within half a percent of the same
  time, or 1% slower.
- **Asking for the code tables ahead** and **zeroing the output buffer
  ahead**, for cold caches: no difference beyond the noise of the
  measurement. Asking for the encoder's machine code ahead did help on arm64
  (3 to 5% after 64 MiB of other work) and is in; on x86-64 it did nothing
  and is left out.
- **Making the code lengths of neighbouring symbols equal**, so that the
  block header gets shorter: 0.15% less output for 3.7 KB payloads. Not worth
  a second kind of codes.

## Output size

Against libdeflate at level 1, each set through one compressor:

| payloads | size | libdeflate's output | fastgzip's is larger by |
|---|---|---|---|
| synthetic JSON requests | 3.7 KB | 58.1% of the input | 2.1% |
| real JSON requests of one kind, 62% of each a base64 field | 1.8 KB | 75.0% | 3.0% |
| 70 sample requests of one protocol from many senders, shuffled | 1.5 KB | 58.2% | 4.9% |
| 192 such requests, more varied still | 1.4 KB | 52.2% | 8.3% |
| three, six, twelve synthetic requests in an array | 11, 22, 45 KB | 44.7%, 39.9%, 36.9% | 1.4%, 1.2%, 1.7% |
| the last 900, 500, 300 bytes of the synthetic requests | | 73.3%, 71.7%, 82.8% | 3.4%, 5.9%, 7.5% |
| Twitter API JSON cut into pieces | 5 KB | 33.7% | 6.2% |
| a very repetitive JSON file (citm_catalog) cut into pieces | 8 KB | 6.6% | 14% |

Where the difference comes from: the codes are made for the payloads before
this one, not for this one (about half of it on the synthetic requests), and
the search keeps one candidate per hash where libdeflate keeps two (the other
half).

It grows when one compressor is given payloads of several kinds, when the
payloads are small (the header of the shared codes, about 70 bytes, goes out
with every payload, and a small payload's own codes would need a shorter
one), and when most of a payload is long repetitions, which a second
candidate finds more of.

Every output is at most 18 bytes and 24 per 16 KiB larger than its input.

## How it was checked

The encoder reads and writes through raw pointers. Every such access has a
debug assertion on its bounds, and the `checked` profile compiles them into
an optimized build.

- **Tests** (`cargo test`): the codes against the rules of the format
  (complete, prefix-free, length-limited, the fixed codes as RFC 1951 gives
  them), and round trips through `flate2` (miniz_oxide, which shares no code
  with this crate) of every shape of payload: empty, tiny, one byte
  repeated, noise, text with every byte value, requests of several kinds in
  turn, payloads of up to 1 MiB, and each edge between two blocks.
- **Stress** (`examples/stress.rs`): over a million payloads through
  long-lived compressors in the `checked` build, 200,000 of them on arm64
  (Neoverse N2), every output decoded and compared.
- **Other decoders**: 50,000 of those outputs read back by zlib 1.3 on the
  runners (zlib-ng 1.3.1 on the build machine), and 1,500 by GNU gzip 1.12.
- **Miri**: the unit tests and a test that takes every path once, on both
  architectures. Nothing reported.
- **Rust 1.83** builds the library.

The workflow in `.github/workflows/ci.yml` repeats all of this at every push,
with a stress run of 50,000 payloads, on x86-64 and on arm64.

Not tested: big-endian targets (the code is written for them, with explicit
byte orders, but nothing ran it there).

## To reproduce

```
cargo test                                         # debug build, all assertions
cargo test --profile checked                       # optimized, all assertions
cargo run --profile checked --example stress -- 1 100000 dump.bin
python3 tools/check_dump.py dump.bin --gzip 2000   # zlib and gzip read it back
cargo run --release --example payloads -- requests.jsonl
cargo run --release --example tokio_bench -- requests.jsonl lines 16000 2 67108864 local
```

`tokio_bench <file> <payload_bytes|0|lines> <requests> <workers>
<other_work_bytes> <local|fresh|blocking>`: with `lines` every line of the
file is a payload; `other_work_bytes` is the memory a worker walks between
two calls.

## Limits

- Not timed on Neoverse N1. The runner is N2. The code uses nothing that N1
  lacks, but N1 is a narrower core, so expect higher absolute times there.
- For payloads of mixed kinds on one compressor, and for very repetitive
  payloads (most of the input covered by long repetitions), the output is
  further from libdeflate's than the figures above; see Output size.
- Level 1 only. There is no setting that trades time for size.
- Needs Rust 1.83 or later.
