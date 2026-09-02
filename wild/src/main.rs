#[cfg(feature = "mimalloc")]
#[global_allocator]
static MIMALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(unix)]
mod incremental;

fn main() {
    if let Err(error) = run() {
        libwild::error::report_error_and_exit(&error)
    }
}

/// The current Wild version as written by build.rs.
const VERSION: &str = include_str!(concat!(env!("OUT_DIR"), "/version.txt"));

fn run() -> libwild::error::Result {
    #[cfg(feature = "dhat")]
    let _profiler = dhat::Profiler::new_heap();

    libwild::init_timing()?;

    #[cfg(unix)]
    if incremental::handle_internal_daemon_command()? {
        return Ok(());
    }

    #[cfg(unix)]
    if std::env::var("WILD_INCREMENTAL").is_ok_and(|value| value == "1") {
        let args = std::env::args().collect::<Vec<_>>();
        if incremental::should_use_daemon(&args) {
            return incremental::run_via_daemon(args);
        }
    }

    let mut args = libwild::Args::new(std::env::args)?;
    args.set_version(VERSION);
    args.parse(std::env::args)?;

    if libwild::should_fork(&args) {
        // Safety: We haven't spawned any threads yet.
        unsafe { libwild::run_in_subprocess(args) };
    } else {
        // Run the linker in this process without forking.

        // Note, we need to setup tracing before worker, otherwise the threads won't contribute to
        // counters such as --time=cycles,instructions etc.
        libwild::setup_tracing(&args)?;

        libwild::run(args)
    }
}
