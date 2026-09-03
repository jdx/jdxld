# mr-boxington worker prototype

Measured 2026-09-02 on an Intel Core Ultra X7 358H (16 cores), using Rust 1.97.1 and upstream Wild
base commit `a71e40a0`. The captured mr-boxington debug link writes a 208 MiB output to tmpfs.
Measurements were interleaved by Hyperfine after three warmups.

The worker is started and owned by the build session. It has no idle timeout and exits when its
mr-boxington parent exits. It caches mappings for inputs whose modification time and size are
unchanged, but still performs symbol resolution, layout, relocation, and output writing in full.

| Mode | Mean | Standard deviation | Range | Runs |
|------|-----:|-------------------:|------:|-----:|
| standalone jdxld | 94.2 ms | 3.5 ms | 88.9–101.1 ms | 15 |
| mr-boxington-owned worker | 82.7 ms | 3.4 ms | 78.7–90.7 ms | 15 |

The worker is 1.14× faster, reducing this isolated link by 12.2% or 11.5 ms. Standalone and worker
outputs were byte-for-byte identical with SHA-256
`704351b993db0cb29abd584974cb134039f50016b8cf2619d86fa62a4a5cfc4a`, and the linked `mbx`
reported version 1.4.1 when executed.

This is the gain from process and mmap reuse, not section-level incremental linking. Since a final
binary usually links once per Cargo command, this result is not an estimate of cross-command edit
latency. Persistent on-disk link state remains necessary for that.

## Earlier sizing results

The same mapping-cache mechanism was first measured behind a five-minute experimental daemon. That
daemon is not part of the current design, but those measurements help size larger projects:

| Project | Full jdxld-equivalent link | Mapping-cache link | Reduction |
|---------|---------------------------:|-------------------:|----------:|
| mr-boxington | 159.0 ms | 138.5 ms | 12.9% / 20.5 ms |
| mise | 451.1 ms | 390.9 ms | 13.3% / 60.2 ms |

In an optimistic `touch src/main.rs; cargo build` loop, mr-boxington improved from 579.3 ms to
544.8 ms (6.0%). Mise improved from 10,716.6 ms to 10,617.8 ms (0.9%), within run-to-run variation.
Even eliminating mise's measured 451 ms link entirely would improve that particular rebuild by only
about 4.2%, because rustc dominates it.

The next meaningful benchmark should accompany persistent parsed metadata and output patching. The
target is a one-codegen-unit change that can avoid full resolution, layout, relocation, and output
rewriting, with an automatic full-link fallback for unsupported changes.
