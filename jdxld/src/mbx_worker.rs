//! Experimental linker worker owned by an mr-boxington build session.
//!
//! This retains mappings of unchanged inputs, but still performs symbol resolution, layout,
//! relocation, and output writing for every link.

use libjdxld::CachingFileSystem;
use libjdxld::error::Context as _;
use libjdxld::error::Result;
use std::ffi::OsString;
use std::io::Read as _;
use std::io::Write as _;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

const WORKER_ARG: &str = "--mbx-worker";
const SOCKET_ENV: &str = "MBX_JDXLD_SOCKET";
const PROTOCOL_VERSION: u32 = 1;

pub(crate) fn should_use_worker(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--help" | "-help" | "--version" | "-version" | "-v" | "-V"
        )
    })
}

pub(crate) fn handle_internal_worker_command() -> Result<bool> {
    let mut args = std::env::args_os();
    let _program = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new(WORKER_ARG)) {
        return Ok(false);
    }
    let socket = args
        .next()
        .map(PathBuf::from)
        .context("missing mr-boxington worker socket path")?;
    let parent_pid = args
        .next()
        .and_then(|arg| arg.to_str().and_then(|arg| arg.parse::<u32>().ok()))
        .context("missing or invalid mr-boxington parent PID")?;
    let state_root = args.next().map(PathBuf::from);
    if args.next().is_some() {
        return Err(libjdxld::error!("unexpected mr-boxington worker argument"));
    }
    serve(&socket, parent_pid, state_root.as_deref())?;
    Ok(true)
}

pub(crate) fn socket_from_environment() -> Option<PathBuf> {
    std::env::var_os(SOCKET_ENV)
        .filter(|socket| !socket.is_empty())
        .map(PathBuf::from)
}

pub(crate) fn run_via_worker(socket: &Path, args: Vec<String>) -> Result {
    let mut stream = UnixStream::connect(socket).with_context(|| {
        format!(
            "failed to connect to mr-boxington jdxld worker at `{}`",
            socket.display()
        )
    })?;

    write_u32(&mut stream, PROTOCOL_VERSION)?;
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    write_bytes(&mut stream, cwd.as_os_str().as_encoded_bytes())?;
    write_u32(
        &mut stream,
        args.len().try_into().context("too many linker arguments")?,
    )?;
    for arg in args {
        write_bytes(&mut stream, arg.as_bytes())?;
    }
    stream.flush()?;

    let success = read_u32(&mut stream)? == 0;
    let message = String::from_utf8(read_bytes(&mut stream)?)
        .context("mr-boxington jdxld worker returned invalid UTF-8")?;
    if success {
        if !message.is_empty() {
            eprint!("{message}");
        }
        Ok(())
    } else {
        Err(libjdxld::error!("{message}"))
    }
}

fn serve(socket: &Path, parent_pid: u32, state_root: Option<&Path>) -> Result {
    if socket.exists() {
        std::fs::remove_file(socket)
            .with_context(|| format!("failed to remove stale socket `{}`", socket.display()))?;
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("failed to bind `{}`", socket.display()))?;
    let _cleanup = SocketCleanup(socket.to_path_buf());
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let file_system = CachingFileSystem::new();

    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                handle_request(&mut stream, &file_system, state_root);
                file_system.prune();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if unsafe { libc::getppid() } as u32 != parent_pid {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn handle_request(
    stream: &mut UnixStream,
    file_system: &CachingFileSystem,
    state_root: Option<&Path>,
) {
    let warnings = Arc::new(Mutex::new(String::new()));
    let warning_output = Arc::clone(&warnings);
    let state_warning_output = Arc::clone(&warnings);
    let result = read_request(stream).and_then(|(cwd, arguments)| {
        std::env::set_current_dir(&cwd)
            .with_context(|| format!("failed to change directory to `{}`", cwd.display()))?;
        file_system.start_recording();
        let get_arguments = || arguments.iter().map(String::as_str);
        let mut args = libjdxld::Args::new(get_arguments)?;
        args.set_version(super::VERSION);
        args.on_warning(Box::new(move |warning| {
            use std::fmt::Write as _;
            let _ = writeln!(warning_output.lock().unwrap(), "{warning}");
        }));
        args.parse(get_arguments)?;
        libjdxld::run_with_file_system(args, file_system.clone())?;
        if let Some(state_root) = state_root
            && let Err(error) = crate::persistent_state::record(
                state_root,
                &cwd,
                &arguments,
                super::VERSION,
                file_system.recorded_inputs(),
            )
        {
            use std::fmt::Write as _;
            let _ = writeln!(
                state_warning_output.lock().unwrap(),
                "jdxld: warning: persistent link state was not recorded: {error:?}"
            );
        }
        Ok(())
    });

    let warnings = warnings.lock().unwrap();
    let (status, message) = match result {
        Ok(()) => (0, warnings.clone()),
        Err(error) => (1, format!("{warnings}{error:?}")),
    };
    let _ = write_u32(stream, status);
    let _ = write_bytes(stream, message.as_bytes());
    let _ = stream.flush();
}

fn read_request(stream: &mut UnixStream) -> Result<(PathBuf, Vec<String>)> {
    let protocol = read_u32(stream)?;
    if protocol != PROTOCOL_VERSION {
        return Err(libjdxld::error!(
            "unsupported mr-boxington jdxld protocol {protocol}; expected {PROTOCOL_VERSION}"
        ));
    }
    let cwd = PathBuf::from(OsString::from_vec(read_bytes(stream)?));
    let argument_count = read_u32(stream)?;
    let mut arguments = Vec::with_capacity(argument_count as usize);
    for _ in 0..argument_count {
        arguments.push(
            String::from_utf8(read_bytes(stream)?)
                .context("linker argument from mr-boxington is not valid UTF-8")?,
        );
    }
    Ok((cwd, arguments))
}

struct SocketCleanup(PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write_u32(stream: &mut UnixStream, value: u32) -> Result {
    stream.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn read_u32(stream: &mut UnixStream) -> Result<u32> {
    let mut bytes = [0; 4];
    stream.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn write_bytes(stream: &mut UnixStream, bytes: &[u8]) -> Result {
    write_u32(
        stream,
        bytes.len().try_into().context("message is too large")?,
    )?;
    stream.write_all(bytes)?;
    Ok(())
}

fn read_bytes(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let len = read_u32(stream)? as usize;
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn informational_invocations_bypass_worker() {
        assert!(!should_use_worker(&["jdxld".into(), "--version".into()]));
        assert!(!should_use_worker(&["jdxld".into(), "-V".into()]));
        assert!(should_use_worker(&[
            "jdxld".into(),
            "main.o".into(),
            "-o".into(),
            "main".into(),
        ]));
    }
}
