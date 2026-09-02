# Persistent-link prototype results

Measured 2026-09-02 on an Intel Core Ultra X7 358H (16 cores), using Rust 1.97.1 and Wild commit
`a71e40a0`. Link-only outputs were written to tmpfs. The Cargo target directories were on Btrfs.

The prototype keeps Wild alive for five minutes and caches mmaps for inputs whose modification time
and size are unchanged. It does **not** yet retain parsed ELF state or patch individual output
sections: resolution, layout, relocation, and output writing are still performed in full.

## Link-only results

The linker invocations and all inputs were captured with `WILD_SAVE_BASE`. Measurements were
interleaved after warmup. Wild was passed `--no-fork` so the timing includes all linker cleanup.

| Project | Debug binary | Linker | Mean | Standard deviation | Runs |
|---------|--------------|--------|-----:|-------------------:|-----:|
| mr-boxington | 208 MiB | LLD | 345.185 ms | 14.887 ms | 15 |
| mr-boxington | 208 MiB | Wild | 159.013 ms | 12.365 ms | 15 |
| mr-boxington | 208 MiB | persistent Wild | 138.530 ms | 17.086 ms | 15 |
| mise | 1.1 GiB | LLD | 1,661.913 ms | 16.069 ms | 10 |
| mise | 1.1 GiB | Wild | 451.124 ms | 14.450 ms | 10 |
| mise | 1.1 GiB | persistent Wild | 390.947 ms | 13.212 ms | 10 |

The prototype improves the isolated Wild link by 12.9% (20.5 ms) for mr-boxington and 13.3%
(60.2 ms) for mise. The persistent and normal Wild outputs had identical SHA-256 hashes for both
projects, and both binaries ran successfully.

## End-to-end Cargo results

Each sample touched `src/main.rs`, then ran `cargo build --quiet` with the same already-warm target
directory. This causes Cargo and rustc to run but gives rustc unchanged source contents, so it is an
optimistic inner-loop case for the compiler. Normal Wild used its default fork behavior.

| Project | Linker | Mean | Standard deviation | Runs |
|---------|--------|-----:|-------------------:|-----:|
| mr-boxington | Wild | 579.345 ms | 76.633 ms | 11 |
| mr-boxington | persistent Wild | 544.799 ms | 61.782 ms | 11 |
| mise | Wild | 10,716.610 ms | 349.622 ms | 7 |
| mise | persistent Wild | 10,617.760 ms | 82.643 ms | 7 |

For mr-boxington the observed end-to-end improvement is 6.0%. For mise it is 0.9%, smaller than the
run-to-run variation of the normal-Wild samples. Even eliminating mise's measured 451 ms Wild link
entirely would reduce this particular 10.7 second rebuild by at most about 4.2%. Further linker work
therefore has a meaningful absolute target (roughly 440 ms if a section-level update approaches 10
ms), but rustc dominates mise's current edit/rebuild loop.

## Next implementation boundary

The next useful milestone is to retain parsed archive/object metadata, the symbol database, layout,
and a relocation reverse index, then rewrite only sections affected by changed codegen units. The
persistent-process protocol and input identity checks in this prototype provide a place to keep that
state. The implementation also needs concurrent request handling or separate per-output sessions,
per-request environment propagation, bounded cache eviction, and fallback-to-full-link logging
before it is suitable for general use.
