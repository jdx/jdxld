# Incremental linking under mr-boxington

Incremental linking in jdxld is supported only when orchestrated by
[mr-boxington](https://github.com/jdx/mr-boxington). The standalone linker remains available for
full links, tests, and reproductions, but it does not create a background daemon or silently choose
an incremental-state directory.

## Why require mr-boxington?

A correct incremental linker needs more than a warm process. It needs stable content identities for
inputs, exclusive ownership of mutable output state, crash-safe transactions, bounded retention,
and a way to invalidate state when the compiler, arguments, or linker changes. mr-boxington already
owns the surrounding build session, file-digest ledger, cache, and incremental-state lifecycle, so
duplicating those facilities in jdxld would create two competing sources of truth.

Requiring mr-boxington also permits an intentionally narrow first target: Rust debug links on
x86-64 Linux with arguments produced by a known rustc/toolchain family. Unsupported link shapes
fall back to a full jdxld link.

## Process model

For each build session, mr-boxington creates a private Unix socket and starts a `jdxld --mbx-worker`
child. Linker shims receive the socket through `MBX_JDXLD_SOCKET` and send the exact linker argument
vector to that worker. mr-boxington terminates and reaps the worker when the build session ends.
There is no idle timeout and no process left behind after the command exits.

The worker serializes requests initially. This avoids concurrent mutation of the same output and is
sufficient to measure process reuse. Parallel links can later use one worker per output identity or
independent state handles.

The session-scoped worker can retain mappings and parsed metadata during one command. It cannot, by
itself, speed up the next `mbx cargo build`, because that command creates a new session. Cross-command
incrementality therefore comes from persistent state, not process lifetime.

## Persistent state

mr-boxington assigns each output a state directory under its incremental root. The identity includes:

- the normalized linker arguments and output path;
- the target and Rust toolchain identity;
- the jdxld executable identity and state-format version; and
- the content digest of every linker input.

The first link writes a complete output and a new state generation. A later link compares input
digests, loads the previous generation, and gives jdxld the changed input set. State updates are
written transactionally and become visible only after the output is complete. Interrupted or
incompatible generations are discarded and produce a full-link fallback.

mr-boxington owns locking, retention, and garbage collection. jdxld owns the contents and versioning
of a generation: parsed ELF/archive metadata, symbol resolution results, layout reservations, and
the reverse index from input sections to affected relocations and output ranges.

The first on-disk format is intentionally advisory. After every successful full link, jdxld writes
a versioned input manifest with an argument identity and publishes it with an atomic rename. A new
worker in a later Cargo command loads that manifest and identifies unchanged, changed, added, and
removed inputs. Its initial file identities use path, modification time, and length, so they cannot
authorize reuse and never cause linker work to be skipped. Replacing those observations with
mr-boxington content digests is the correctness boundary for persisting parsed metadata.

## Initial milestones

1. Reuse unchanged input mappings inside an mr-boxington-owned worker while still performing a full
   resolution, layout, relocation, and output write.
2. Persist the input manifest and link metadata, using mr-boxington content digests instead of mtime
   and file size.
3. Incrementally update one changed Rust codegen object when arguments and all other inputs are
   unchanged.
4. Add layout slack and reverse relocation indexes for size-changing codegen units.
5. Expand supported link shapes only when benchmarks justify the additional invalidation surface.

Every milestone retains a correctness-first full-link fallback. Benchmark reports must distinguish
link-only time from the complete edit-to-build loop and compare output hashes plus executable
behavior.
