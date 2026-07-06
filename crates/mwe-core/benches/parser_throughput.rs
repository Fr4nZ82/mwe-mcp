// SPDX-License-Identifier: AGPL-3.0-or-later
//! Throughput benchmark for the marker parser.
//!
//! Builds a ~10 MB synthetic corpus with a realistic-ish mix of prose,
//! marker-delimited regions, and self-closing embeds. Reports MB/s.
//!
//! Roadmap target (roadmap): single-thread throughput
//! ≥ 10 MB/s. This bench is **not gated in CI** (criterion is slow and
//! noise-sensitive on shared runners); run it locally with:
//!
//! ```bash
//! cargo bench -p mwe-core --bench parser_throughput
//! ```

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use mwe_core::parser::parse;

/// Reference `UUIDv7` baked into the corpus.
const SAMPLE_UUID_V7: &str = "018f1234-5678-7abc-9def-0123456789ab";

/// Build a corpus of approximately `target_bytes` made of repeated
/// realistic chunks: prose, a region with ACL + sender + `fact_id`,
/// more prose, an embed, more prose. The chunk is ~1 KB so the marker
/// density is "natural" rather than pathological.
fn build_corpus(target_bytes: usize) -> String {
    let chunk = format!(
        "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do \
eiusmod tempor incididunt ut labore et dolore magna aliqua. \
{{{{owner=user:alice sender=user:galadriel allow=group:famiglia f={SAMPLE_UUID_V7}}}}}\
Galadriel ha notato che Sméagol oggi era stanco e nervoso dopo scuola — \
probabilmente per il compito di matematica di domani.{{{{/}}}} \
Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut \
aliquip ex ea commodo consequat. {{{{embed=c-2026-05-10-foto-001.jpg}}}} \
Duis aute irure dolor in reprehenderit in voluptate velit esse cillum \
dolore eu fugiat nulla pariatur. Excepteur sint occaecat cupidatat non \
proident, sunt in culpa qui officia deserunt mollit anim id est laborum. \
{{{{owner=global f={SAMPLE_UUID_V7}}}}}Decisione presa nel meeting: rilascio \
v2 entro luglio.{{{{/}}}} \
"
    );
    let mut buf = String::with_capacity(target_bytes + chunk.len());
    while buf.len() < target_bytes {
        buf.push_str(&chunk);
    }
    buf
}

fn bench_parse(c: &mut Criterion) {
    let corpus = build_corpus(10 * 1024 * 1024); // ~10 MiB
    let mut group = c.benchmark_group("parser_throughput");
    // Throughput in bytes lets criterion report MB/s directly.
    group.throughput(Throughput::Bytes(corpus.len() as u64));
    // 10 MiB × N iterations is heavy — keep sample size conservative.
    group.sample_size(20);
    group.bench_function("parse_10MiB_mixed_corpus", |b| {
        b.iter(|| {
            let out = parse(black_box(&corpus));
            black_box(out)
        });
    });
    group.finish();
}

criterion_group!(benches, bench_parse);
criterion_main!(benches);
