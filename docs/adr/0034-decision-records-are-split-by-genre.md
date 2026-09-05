# Decision records are split by genre

The decision records were carrying three genres under one lifecycle: product intent, architectural constraint and implementation detail, all amended by the same procedure and all read as equally binding. The three now live apart. Product intent lives in [product.md](../product.md), architectural constraint stays in `docs/adr/`, and implementation detail lives in `docs/spec/`. A record that cannot say which of the three it is does not belong here.

An audit of 423 normative claims across the 33 records forced the split. Implementation detail was 42% of all claims and was amended away at 16%, against 6% for architectural constraint, so the genre with the shortest half-life was the one occupying the most space in the documents with the longest one. The claims that survived were the ones with a test behind them: scan regions, exhaustive matches, compile-time glyph disjointness. The ones without a test drifted, and three of them were contradicted by the code while the record still asserted them.

So a live ADR names its enforcement. Each states the boundary, then names the test, lint or compile-time assertion that holds it, by file and function. Where nothing holds it, the record says so plainly rather than leaving a reader to assume the restriction is real. A record with no enforcement and no honest admission of that is the shape this decision exists to stop.

**Enforcement:** `crates/repon/tests/adr_enforcement.rs` reads every record in `docs/adr/`, treats one as retired only when its own opening lines say so, and fails when a live record carries no `**Enforcement:**` line or names a function, `just` recipe or path that does not exist. Liveness is read from each file rather than from a list, so there is no second list to go stale. What it cannot check is whether a named test holds the claim above it, which stays a review job.

Earlier revisions of this record, including its amendment history, are in the git history of this file.

See [#423](https://github.com/paulchiu/repon/issues/423).
