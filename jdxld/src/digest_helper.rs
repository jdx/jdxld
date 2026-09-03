//! Content digests supplied by the mr-boxington session that owns the worker.

use crate::persistent_state::ResolvedInput;
use libjdxld::CachedInputIdentity;
use libjdxld::error::Context as _;
use libjdxld::error::Result;
use std::ffi::OsString;
use std::io::Read;
use std::io::Write;
use std::os::unix::ffi::OsStrExt as _;
use std::process::Command;
use std::process::Stdio;

const HELPER_ENV: &str = "MBX_JDXLD_DIGEST_HELPER";
const HELPER_ARG: &str = "__jdxld_digests_v1";
const MAGIC: &[u8; 8] = b"JDXLDG01";

pub(crate) fn resolve(inputs: Vec<CachedInputIdentity>) -> Result<Vec<ResolvedInput>> {
    let Some(helper) = std::env::var_os(HELPER_ENV).filter(|helper| !helper.is_empty()) else {
        return Ok(without_digests(inputs));
    };
    let digests = request_digests(helper, &inputs)?;
    Ok(inputs
        .into_iter()
        .zip(digests)
        .map(|(identity, content_digest)| ResolvedInput {
            identity,
            content_digest: Some(content_digest),
        })
        .collect())
}

pub(crate) fn without_digests(inputs: Vec<CachedInputIdentity>) -> Vec<ResolvedInput> {
    inputs
        .into_iter()
        .map(|identity| ResolvedInput {
            identity,
            content_digest: None,
        })
        .collect()
}

fn request_digests(helper: OsString, inputs: &[CachedInputIdentity]) -> Result<Vec<[u8; 32]>> {
    let mut child = Command::new(helper)
        .arg(HELPER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start the mr-boxington digest helper")?;
    {
        let mut stdin = child.stdin.take().context("digest helper has no stdin")?;
        write_request(&mut stdin, inputs)?;
    }
    let output = child
        .wait_with_output()
        .context("failed to wait for the mr-boxington digest helper")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(libjdxld::error!(
            "mr-boxington digest helper failed with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    read_response(&mut output.stdout.as_slice(), inputs.len())
}

fn write_request(output: &mut impl Write, inputs: &[CachedInputIdentity]) -> Result {
    output.write_all(MAGIC)?;
    write_u32(
        output,
        inputs.len().try_into().context("too many linker inputs")?,
    )?;
    for input in inputs {
        let path = input.path.as_os_str().as_bytes();
        write_u32(
            output,
            path.len().try_into().context("input path is too long")?,
        )?;
        output.write_all(path)?;
    }
    Ok(())
}

fn read_response(input: &mut impl Read, expected: usize) -> Result<Vec<[u8; 32]>> {
    let mut magic = [0; 8];
    input.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(libjdxld::error!("invalid mr-boxington digest response"));
    }
    let count = read_u32(input)? as usize;
    if count != expected {
        return Err(libjdxld::error!(
            "mr-boxington digest helper returned {count} digests for {expected} inputs"
        ));
    }
    let mut digests = Vec::with_capacity(count);
    for _ in 0..count {
        let mut digest = [0; 32];
        input.read_exact(&mut digest)?;
        digests.push(digest);
    }
    let mut trailing = [0];
    if input.read(&mut trailing)? != 0 {
        return Err(libjdxld::error!(
            "mr-boxington digest helper returned trailing data"
        ));
    }
    Ok(digests)
}

fn write_u32(output: &mut impl Write, value: u32) -> Result {
    output.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn read_u32(input: &mut impl Read) -> Result<u32> {
    let mut bytes = [0; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_requires_the_expected_number_of_digests() {
        let mut response = MAGIC.to_vec();
        response.extend_from_slice(&1_u32.to_le_bytes());
        response.extend_from_slice(&[7; 32]);
        assert_eq!(
            read_response(&mut response.as_slice(), 1).unwrap(),
            vec![[7; 32]]
        );
        assert!(read_response(&mut response.as_slice(), 2).is_err());
    }
}
