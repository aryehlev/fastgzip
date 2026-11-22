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

That is level 0, which is what this crate is for. `Gzip::with_level(1..=9)`
has libdeflate's levels as well, for output as small as libdeflate's; see
Levels.

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

## Levels

`Gzip::new()` is level 0: everything above. `Gzip::with_level(n)` also has
levels 1 to 9, which are libdeflate's: each searches a payload the way
libdeflate does at that level (`src/levels.rs` says what was taken from it),
and the payload gets Huffman codes of its own. The output is the size of
libdeflate's at the same level, and depends on nothing but the payload.

Microseconds per payload on one thread, and the size of the output against
libdeflate's at level 1, for the synthetic 3.7 KB requests on the arm64
runner (Neoverse N2):

| level | libdeflate | fastgzip | fastgzip's share of libdeflate's time | output: libdeflate | fastgzip |
|---|---|---|---|---|---|
| 0 | | **5.35** | 30% of level 1 | | +2.1% |
| 1 | 17.80 | **15.12** | 85% | 0 | +0.05% |
| 2 | 28.07 | **23.58** | 84% | +0.2% | +0.1% |
| 3 | 28.46 | **23.91** | 84% | +0.1% | 0.0% |
| 4 | 28.58 | **23.99** | 84% | 0.0% | -0.1% |
| 5 | 32.35 | **26.37** | 82% | -1.7% | -1.7% |
| 6 | 32.65 | **26.88** | 82% | -1.7% | -1.7% |
| 7 | 32.75 | **27.06** | 83% | -1.7% | -1.7% |
| 8 | 34.51 | **28.72** | 83% | -1.9% | -1.9% |
| 9 | 34.43 | **28.63** | 83% | -1.9% | -1.9% |

Other sizes on the same machine, levels 1 to 9: 75 to 82% of libdeflate's
time at 500 bytes, 72 to 85% at 1.5 KB, 80 to 85% at 5 KB, 85 to 90% at
11 KB, 83 to 96% at 22 KB. Up to 32 KiB the output is within 0.1% of
libdeflate's at every level and size that was measured, and the same number
of bytes from 8 KiB on, where the tables are as large as libdeflate's.

Above 32 KiB, levels 2 to 9 do one thing that libdeflate does not, and it
is where most of their gain there comes from. libdeflate looks for matches
of three bytes in every payload that holds 80 or more different bytes, and
keeps a table for it into which every position goes. Here a block is
searched for them only if its first 4 KiB hold 160 or more different bytes,
which machine code does and text and JSON do not. In machine code those
matches make the output 1.5 to 3% smaller. In text they change it by less
than 0.3% and take a tenth of the time; in JSON they make it about 1%
larger and take a fifth (`src/big.rs`). Shares of libdeflate's time at the
same level on the same machine, and the output against libdeflate's:

| payloads | size | level 1 | levels 2 to 9 | output at levels 2 to 9 |
|---|---|---|---|---|
| twelve, 24 and 100 of the synthetic requests in an array | 45, 90, 375 KB | 80 to 81% | 69 to 80% | 0.8 to 1.2% smaller |
| source text | 200 KB | 101% | 77 to 86% | 0.1 to 0.2% larger |
| machine code | 200 KB | 82% | 89 to 96% | less than 0.1% larger |
| text, then machine code, then requests | 210 KB | 85% | 81 to 89% | 0.2% smaller to the same |

(Each range is over the levels and over two runs. One of the 48 figures
behind "69 to 80%" was 85%. At level 1 the output is libdeflate's size to
the byte.)

On an x86-64 runner (Intel Xeon 6975P-C): 72 to 81% of libdeflate's time up
to 1.5 KB; from 1.8 to 5 KB, 80 to 89% at level 1 and 81 to 97% at the
levels above; at 11 and 22 KB, 86 to 91% at level 1 and 96 to 106% above.
Above 32 KiB: 78 to 83% at level 1 (108% on the text); at levels 2 to 9,
75 to 82% on the requests, 88 to 96% on the text, 95 to 100% on the machine
code and 89 to 93% on the mixed payloads.

A compressor of these levels keeps 256 KiB of tables, of which a small
payload touches only its share (56 KiB for payloads of up to 4 KiB); a
payload above 32 KiB uses all of it, at levels 4 to 9 another 256 KiB, and
8 bytes for every match of a block.

So these levels are libdeflate's in less time. On arm64 that is 15 to 28%
less up to 5 KB, 10 to 15% less at 11 KB and 4 to 17% less at 22 KB; above
32 KiB, 20 to 31% less on JSON, 14 to 23% on text and 4 to 11% on machine
code. They do not gain what level 0 gains, and cannot:

- Their time goes into the search and into making each payload's codes, and
  libdeflate does both about as fast as they can be done. What is left to
  save is what a payload does not need. A small one does not need tables of
  full size: libdeflate clears 128 KiB of them per call at level 1 and
  192 KiB at the levels above, and then works in tables of that size, where
  here they are sized to the payload (`fast_order` in `src/levels.rs`). A
  large one of text or JSON does not need the table for matches of three
  bytes (above).
- Level 0's way, codes shared between payloads, was tried at these levels.
  It saves the making of codes, a quarter of the time at level 1, and costs
  some 25 bytes per payload, 1% of the output at 4 KB and 2% at 2 KB: shared
  codes have to describe every symbol that any of the payloads uses, and fit
  none of them exactly. That is more than levels 2 to 4 gain over level 1 on
  payloads like these, and half or more of what levels 5 to 9 gain. Level 0
  gets its single pass for those bytes; here they buy too little.

Tried at these levels and left out, measured on both runners:

- **The steps over positions that have no candidate as a function of their
  own** at levels 2 to 9, as level 1 has them: about 3% slower on arm64 for
  payloads of a few kilobytes. A search that calls a function from its
  steps cannot keep its state in registers across them. The loop is inside
  the search now.
- **Hashing a position, or loading its table entry, one step early**, so
  that the answer is there when a step asks whether it is a literal: no
  gain on arm64, and the second 3 to 5% slower on one x86-64 machine.
- **Chains walked four steps at a time** (which payloads above 32 KiB get
  at levels 4 to 9) for smaller payloads: 2 to 6% quicker on x86-64 from
  16 KiB on, 2 to 3% slower on arm64 at levels 5 and 6.
- **A match's length limits kept from step to step** and cut down only
  near the end of the payload: 1 to 2% slower.

And for payloads above 32 KiB:

- **Matches of three bytes in every block, as libdeflate has them**: its
  output to within 0.01%, at 88 to 100% of its time on arm64 where the
  search without them takes 69 to 86% on text and JSON. So they are kept
  for the blocks that gain by them, and that is what machine code gets.
- **Rows in place of chains**: a table whose every entry is the four most
  recent positions with a hash, all four looked at without a branch between
  them. At level 2 on arm64 it took 65 to 66% of libdeflate's time on the
  requests where chains take 69 to 73%, and gave output another 0.8%
  smaller. On text it was no quicker, and to give text the size of levels 3
  and 4 it has to look at the next position before taking a match, which
  made it slower than chains there (92% of libdeflate's time at level 3
  against 81%, and 119 to 122% on x86-64). Not worth a second search for
  one level.
- **Rows of 16 to 64 positions with a tag each**, the tags compared sixteen
  at a time, for the levels that look at many candidates: entering a
  position costs more than it does in a chain, and a first form was 7 to
  27% slower than libdeflate on one x86-64 machine. Not taken further.

Which level, then: 0 if time matters; 5 if output 4% smaller is worth five
times the time. The gain of libdeflate's levels on payloads like these comes
with level 5, where a match is given up when the next position has a better
one; levels 2 to 4 gain almost nothing over 1, and 6 to 9 little over 5.

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
- **Levels 1 to 9**: the same round trips at every level; each level's two
  forms of its search (`src/fast.rs` and `src/big.rs` have the one that
  runs, `src/levels.rs` the plain one) compared byte for byte on payloads
  of every shape and of up to 650 KB, among them payloads made to end a
  block in each of the ways a block can end; both ways of writing a block
  compared likewise; 20,000 payloads per level through the stress run on
  each runner, a part of them longer than 32 KiB, those of three levels read
  back by zlib and 500 of each by gzip; the tests that take every path once
  under Miri. From 8 to 32 KiB, where the tables are as large as
  libdeflate's, the output has libdeflate's size to the byte, which is how
  it is known that the searches are the same. Above 32 KiB it is within
  0.01% of that size when every block is searched for matches of three
  bytes, as libdeflate searches it; the rest of the difference is the
  payload's first position and its last seven bytes (`src/big.rs`).
- **Rust 1.83** builds the library.

The workflow in `.github/workflows/ci.yml` repeats all of this at every push,
with a stress run of 50,000 payloads at level 0 and of 5,000 at each level
from 1 to 9, on x86-64 and on arm64.

Not tested: big-endian targets (the code is written for them, with explicit
byte orders, but nothing ran it there).

## To reproduce

```
cargo test                                         # debug build, all assertions
cargo test --profile checked                       # optimized, all assertions
cargo run --profile checked --example stress -- 1 100000 dump.bin
python3 tools/check_dump.py dump.bin --gzip 2000   # zlib and gzip read it back
LEVEL=9 cargo run --profile checked --example stress -- 19 20000   # any level
cargo run --release --example payloads -- requests.jsonl
LEVEL=5 cargo run --release --example payloads -- requests.jsonl   # any level
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
- Above 32 KiB, levels 2 to 9 save least on machine code and other data
  that uses every byte value: 4 to 11% of libdeflate's time on arm64, and
  nothing on x86-64. On x86-64 they also save nothing on payloads of 11 to
  22 KB, and take up to 6% more than libdeflate at 22 KB (see Levels).
- Level 0 and levels 1 to 9 are two different things (see Levels); there is
  nothing in between.
- Needs Rust 1.83 or later.
