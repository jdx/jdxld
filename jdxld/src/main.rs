#[cfg(feature = "mimalloc")]
#[global_allocator]
static MIMALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(unix)]
mod digest_helper;
#[cfg(unix)]
mod mbx_worker;
#[cfg(unix)]
mod persistent_state;

fn main() {
    if let Err(error) = run() {
        libjdxld::error::report_error_and_exit(&error)
    }
}

/// The current jdxld version as written by build.rs.
const VERSION: &str = include_str!(concat!(env!("OUT_DIR"), "/version.txt"));

fn run() -> libjdxld::error::Result {
    #[cfg(feature = "dhat")]
    let _profiler = dhat::Profiler::new_heap();

    libjdxld::init_timing()?;

    #[cfg(unix)]
    if mbx_worker::handle_internal_worker_command()? {
        return Ok(());
    }

    #[cfg(unix)]
    if let Some(socket) = mbx_worker::socket_from_environment() {
        let args = std::env::args().collect::<Vec<_>>();
        if mbx_worker::should_use_worker(&args) {
            return mbx_worker::run_via_worker(&socket, args);
        }
    }

    let mut args = libjdxld::Args::new(std::env::args)?;
    args.set_version(VERSION);
    args.parse(std::env::args)?;

    if libjdxld::should_fork(&args) {
        // Safety: We haven't spawned any threads yet.
        unsafe { libjdxld::run_in_subprocess(args) };
    } else {
        // Run the linker in this process without forking.

        // Note, we need to setup tracing before worker, otherwise the threads won't contribute to
        // counters such as --time=cycles,instructions etc.
        libjdxld::setup_tracing(&args)?;

        libjdxld::run(args)
    }
}
