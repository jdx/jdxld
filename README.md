# jdxld

![jdxld logo - drawing of rusty chain links with vines](/images/jdxld.png)

jdxld is an experimental fork of [Wild](https://github.com/wild-linker/wild), created by David
Lattimore and its contributors. This fork is focused on incremental linking for Rust builds
orchestrated by [mr-boxington](https://github.com/jdx/mr-boxington).

The plan is to eventually make it incremental, however that isn't yet implemented. It is however
already pretty fast even without incremental linking.

## Development status

jdxld has not published standalone packages or releases. Build it from a checkout with:

```sh
cargo build --release --bin jdxld
```

The standalone binary remains useful for development and compatibility testing. The supported
incremental mode is being designed around mr-boxington rather than as a general-purpose daemon.

## Using as your default linker

Being a drop-in replacement, jdxld can be used similarly to other linkers by being invoked by GCC or
Clang. Meaning you have several options:

* Clang's exclusive option `--ld-path=jdxld`
* GCC 16.1+ and Clang's option `-fuse-ld=jdxld` (note that Clang requires `ld.jdxld` binary/symlink)
* Generally supported `-B <path>`, where `<path>` is the directory containing `ld` that points to
  `jdxld`

Below are examples of integrating jdxld with various build systems.

### Rust (Cargo)

You can use one of the options mentioned above in `~/.cargo/config.toml`:

```toml
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-Clink-arg=--ld-path=jdxld"]
```

Or:

```toml
[target.x86_64-unknown-linux-gnu]
# linker = "clang" # Uncomment this line if your GCC is older than version 16.
rustflags = ["-Clink-arg=-fuse-ld=jdxld"]
```

### C/C++ (autotools, meson, old CMake etc.)

Usually setting `LDFLAGS` is enough, but there are projects that implement their own solutions:

```sh
export LDFLAGS="${LDFLAGS} -fuse-ld=jdxld"
```

Or (especially useful for older GCC versions), create a symlink `ld` pointing to `jdxld` and pass the
directory to GCC:

```sh
ln -s /usr/bin/jdxld /tmp/ld

export CFLAGS="${CFLAGS} -B/tmp"
export CXXFLAGS="${CXXFLAGS} -B/tmp"
export LDFLAGS="${LDFLAGS} -B/tmp"
```

Then configure the project (you might need to remove the configuration cache first) and run your
usual build steps.

Due to the complexity of these build systems, you might want to verify that jdxld was used to link a
binary with [readelf](#how-can-i-verify-that-jdxld-was-used-to-link-a-binary).

### Illumos specific Cargo configuration:

```toml
[target.x86_64-unknown-illumos]
# Absolute path to clang - on OmniOS this is likely something like /opt/ooce/bin/clang.
linker = "/usr/bin/clang"

rustflags = [
    # Will silently delegate to GNU ld or Sun ld unless the absolute path to jdxld is provided.
    "-Clink-arg=-fuse-ld=/absolute/path/to/jdxld"
]
```

## Q&A

### Why another linker?

Mold is already very fast, however it doesn't do incremental linking and the author has stated that
they don't intend to. jdxld doesn't do incremental linking yet, but that is the end-goal. By writing
jdxld in Rust, it's hoped that the complexity of incremental linking will be achievable.

### What's working?

The following platforms / architectures are currently supported:

* x86-64 on Linux
* ARM64 on Linux
* RISC-V (riscv64gc) on Linux
* LoongArch64 on Linux (initial support)
* PPC64LE on Linux (initial support)

The following is working with the caveat that there may be bugs:

* Output to statically linked, non-relocatable binaries
* Output to statically linked, position-independent binaries (static-PIE)
* Output to dynamically linked binaries
* Output to shared objects (.so files)
* Rust proc-macros, when linked with jdxld work
* Most of the top downloaded crates on crates.io have been tested with jdxld and pass their tests
* Debug info
* GNU jobserver support
* Partial linker script support. See the [linker script support matrix](LINKER_SCRIPT_SUPPORT.md) for details.
* Linker plugin LTO - [known issues](https://github.com/jdx/jdxld/issues?q=is%3Aissue%20state%3Aopen%20label%3ALTO)

### What isn't yet supported?

Here are some of the larger things that aren't yet done, roughly sorted by current priority:

* Incremental linking
* More complex linker scripts
* Mach-O support
* Windows support

### How can I verify that jdxld was used to link a binary?

Install `readelf` (available from binutils package), then run:

```sh
readelf --string-dump .comment my-executable
```

Look for a line like:

```
Linker: jdxld version 0.1.0
```

You can probably also get away with `strings` (also available from binutils package):

```sh
strings my-executable | grep 'Linker:'
```

### Where did the name come from?

jdxld is named for this fork's owner and the conventional `ld` linker suffix. The upstream Wild
name and history remain credited in this repository's history and [NOTICE](NOTICE).

## Benchmarks

The goal of jdxld is to eventually be very fast via incremental linking. However, we also want to be
as fast as we can be for non-incremental linking and for the initial link when incremental linking
is enabled.

All benchmarks are run with output to a tmpfs. See [BENCHMARKING.md](BENCHMARKING.md) for details on
running benchmarks.

We run benchmarks on a few different systems:

* [Ryzen 9 9955HX (16 core, 32 thread)](benchmarks/ryzen-9955hx.md)
* [2020 era Intel-based laptop with 4 cores and 8 threads](benchmarks/lemp9.md)
* [Raspberry Pi 5](benchmarks/raspberrypi.md)

Here's a few highlights.

### Ryzen 9955HX (16 core, 32 thread)

First, we link the Chrome web browser (or technically, Chromium).

![Benchmark of linking chrome-crel](benchmarks/images/ryzen-9955hx/chrome-crel-time.svg)

Memory consumption when linking Chromium:

![Benchmark of linking chrome-crel](benchmarks/images/ryzen-9955hx/chrome-crel-memory.svg)

librustc-driver is the shared object where most of the code in the Rust compiler lives. This
benchmark shows the time to link it.

![Benchmark of linking librustc-driver](benchmarks/images/ryzen-9955hx/librustc-driver-time.svg)

For something much smaller, this historical upstream benchmark shows the time to link Wild itself.
It predates the jdxld fork.

![Historical benchmark of linking Wild](benchmarks/images/ryzen-9955hx/wild-time.svg)

### Raspberry Pi 5

Here's linking rust-analyzer on a Raspberry Pi 5.

![Time to link rust-analyzer-no-debug](benchmarks/images/raspberrypi/rust-analyzer-no-debug-time.svg)

## Linking Rust code

The following is a `cargo test` command-line that can be used to build and test a crate using jdxld.
This has been run successfully on a few popular crates (e.g. ripgrep, serde, tokio, rand, bitflags).
It assumes that the "jdxld" binary is on your path. It also depends on the Clang compiler being
installed, since GCC doesn't allow using an arbitrary linker.

```sh
RUSTFLAGS="-Clinker=clang -Clink-args=--ld-path=jdxld" cargo test
```

Alternatively, with `ld.jdxld` symlink pointing at `jdxld`:
```sh
RUSTFLAGS="-Clinker=clang -Clink-args=-fuse-ld=jdxld" cargo test
```

## Contributing

For more information on contributing to `jdxld` see [CONTRIBUTING.md](CONTRIBUTING.md).

For a high-level overview of jdxld's design, see [DESIGN.md](DESIGN.md).

## Further reading

Many of the posts on [David's blog](https://davidlattimore.github.io/) are about various aspects of
the jdxld linker.

# Code of Conduct

The jdxld project adheres to the [Rust code of conduct](CODE_OF_CONDUCT.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
jdxld by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
