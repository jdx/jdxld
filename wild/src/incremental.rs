//! Experimental persistent-process mode.
//!
//! This is deliberately a small first step toward incremental linking. It retains mappings of
//! unchanged inputs, but still performs symbol resolution, layout, relocation and output writing
//! for every link.

use libwild::CachingFileSystem;
use libwild::error::Context as _;
use libwild::error::Result;
use std::hash::Hash as _;
use std::hash::Hasher as _;
use std::io::Read as _;
use std::io::Write as _;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

const DAEMON_ARG: &str = "--wild-incremental-daemon";
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) fn should_use_daemon(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--help" | "-help" | "--version" | "-version" | "-v" | "-V"
        )
    })
}

pub(crate) fn handle_internal_daemon_command() -> Result<bool> {
    let mut args = std::env::args();
    let _program = args.next();
    if args.next().as_deref() != Some(DAEMON_ARG) {
        return Ok(false);
    }
    let socket = args
        .next()
        .map(PathBuf::from)
        .context("missing incremental daemon socket path")?;
    serve(&socket)?;
    Ok(true)
}

pub(crate) fn run_via_daemon(args: Vec<String>) -> Result {
    let socket = socket_path()?;
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(_) => {
            start_daemon(&socket)?;
            connect_with_retry(&socket)?
        }
    };

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
        .context("incremental daemon returned invalid UTF-8")?;
    if success {
        if !message.is_empty() {
            eprint!("{message}");
        }
        Ok(())
    } else {
        Err(libwild::error!("{message}"))
    }
}

fn socket_path() -> Result<PathBuf> {
    let base = if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir)
    } else {
        let path = std::env::temp_dir().join(format!("wild-{}", unsafe { libc::geteuid() }));
        if !path.exists() {
            std::fs::create_dir(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        path
    };
    let executable = std::env::current_exe()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    executable.hash(&mut hasher);
    Ok(base.join(format!("wild-incremental-{:x}.sock", hasher.finish())))
}

fn start_daemon(socket: &Path) -> Result {
    std::process::Command::new(std::env::current_exe()?)
        .arg(DAEMON_ARG)
        .arg(socket)
        .env_remove("WILD_INCREMENTAL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start incremental linker daemon")?;
    Ok(())
}

fn connect_with_retry(socket: &Path) -> Result<UnixStream> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
                let _ = error;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to connect to incremental linker daemon at `{}`",
                        socket.display()
                    )
                });
            }
        }
    }
}

fn serve(socket: &Path) -> Result {
    if socket.exists() {
        match UnixStream::connect(socket) {
            Ok(_) => return Ok(()),
            Err(_) => std::fs::remove_file(socket)?,
        }
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("failed to bind `{}`", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let file_system = CachingFileSystem::new();
    let mut last_request = Instant::now();

    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                last_request = Instant::now();
                handle_request(&mut stream, &file_system);
                file_system.prune();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if last_request.elapsed() >= IDLE_TIMEOUT {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
    std::fs::remove_file(socket)?;
    Ok(())
}

fn handle_request(stream: &mut UnixStream, file_system: &CachingFileSystem) {
    let warnings = Arc::new(Mutex::new(String::new()));
    let warning_output = Arc::clone(&warnings);
    let result = read_request(stream).and_then(|(cwd, arguments)| {
        std::env::set_current_dir(&cwd)
            .with_context(|| format!("failed to change directory to `{}`", cwd.display()))?;
        let get_arguments = || arguments.iter().map(String::as_str);
        let mut args = libwild::Args::new(get_arguments)?;
        args.set_version(super::VERSION);
        args.on_warning(Box::new(move |warning| {
            use std::fmt::Write as _;
            let _ = writeln!(warning_output.lock().unwrap(), "wild: warning: {warning}");
        }));
        args.parse(get_arguments)?;
        libwild::run_with_file_system(args, file_system.clone())
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
    let cwd = PathBuf::from(std::ffi::OsString::from_vec(read_bytes(stream)?));
    let argument_count = read_u32(stream)?;
    let mut arguments = Vec::with_capacity(argument_count as usize);
    for _ in 0..argument_count {
        arguments.push(
            String::from_utf8(read_bytes(stream)?).context("linker argument is not valid UTF-8")?,
        );
    }
    Ok((cwd, arguments))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn informational_invocations_bypass_daemon() {
        assert!(!should_use_daemon(&["wild".into(), "--version".into()]));
        assert!(!should_use_daemon(&["wild".into(), "-V".into()]));
        assert!(should_use_daemon(&[
            "wild".into(),
            "main.o".into(),
            "-o".into(),
            "main".into(),
        ]));
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
