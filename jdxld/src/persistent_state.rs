//! Advisory, crash-safe state recorded after a successful worker link.
//!
//! This format records content digests supplied by the owning build session when available, but
//! deliberately cannot authorize incremental output reuse: no parsed metadata or output ranges are
//! restored yet, so the linker still performs a full link.

use libjdxld::CachedInputIdentity;
use libjdxld::error::Context as _;
use libjdxld::error::Result;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::UNIX_EPOCH;

const FORMAT_VERSION: u32 = 2;
const MANIFEST_FILE: &str = "manifest.postcard";
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format_version: u32,
    linker_version: String,
    generation: u64,
    structure_digest: [u8; 32],
    inputs: Vec<InputIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct InputIdentity {
    path: Vec<u8>,
    modified_nanos: u128,
    len: u64,
    content_digest: Option<[u8; 32]>,
}

pub(crate) struct ResolvedInput {
    pub(crate) identity: CachedInputIdentity,
    pub(crate) content_digest: Option<[u8; 32]>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct StateObservation {
    pub(crate) previous_generation: Option<u64>,
    pub(crate) unchanged: usize,
    pub(crate) changed: usize,
    pub(crate) added: usize,
    pub(crate) removed: usize,
}

pub(crate) fn record(
    root: &Path,
    cwd: &Path,
    arguments: &[String],
    linker_version: &str,
    inputs: Vec<ResolvedInput>,
) -> Result<StateObservation> {
    let input_paths = inputs
        .iter()
        .map(|input| input.identity.path.as_path())
        .collect::<BTreeSet<_>>();
    let structure_digest = structure_digest(cwd, arguments, &input_paths);
    let identity_digest = output_path(cwd, arguments).map_or(structure_digest, |output| {
        digest_parts([output.as_os_str()])
    });
    let state_dir = root.join(hex::encode(identity_digest));
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("failed to create jdxld state `{}`", state_dir.display()))?;
    let manifest_path = state_dir.join(MANIFEST_FILE);
    let previous = read_manifest(&manifest_path)
        .filter(|manifest| manifest.format_version == FORMAT_VERSION)
        .filter(|manifest| manifest.linker_version == linker_version)
        .filter(|manifest| manifest.structure_digest == structure_digest);
    let identities = inputs
        .into_iter()
        .map(InputIdentity::from)
        .collect::<Vec<_>>();
    let observation = compare(previous.as_ref(), &identities);
    if previous
        .as_ref()
        .is_some_and(|manifest| manifest.inputs == identities)
    {
        return Ok(observation);
    }
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        linker_version: linker_version.to_owned(),
        generation: previous
            .as_ref()
            .map_or(1, |manifest| manifest.generation.saturating_add(1)),
        structure_digest,
        inputs: identities,
    };
    write_manifest(&manifest_path, &manifest)?;
    Ok(observation)
}

fn structure_digest(cwd: &Path, arguments: &[String], input_paths: &BTreeSet<&Path>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    let mut is_output_value = false;
    let mut is_search_path_value = false;
    for argument in arguments.iter().skip(1) {
        if is_output_value {
            hash_part(&mut hasher, OsStr::new("<output>"));
            is_output_value = false;
        } else if is_search_path_value {
            if is_rustc_temporary_path(Path::new(argument)) {
                hash_part(&mut hasher, OsStr::new("<rustc-temporary-search-path>"));
            } else {
                hash_part(&mut hasher, OsStr::new(argument));
            }
            is_search_path_value = false;
        } else if argument == "-o" || argument == "--output" {
            hash_part(&mut hasher, OsStr::new(argument));
            is_output_value = true;
        } else if argument == "-L" {
            hash_part(&mut hasher, OsStr::new(argument));
            is_search_path_value = true;
        } else if argument.starts_with("--output=") || argument.starts_with("-o") {
            hash_part(&mut hasher, OsStr::new("<output>"));
        } else if argument
            .strip_prefix("-L")
            .is_some_and(|path| is_rustc_temporary_path(Path::new(path)))
        {
            hash_part(&mut hasher, OsStr::new("<rustc-temporary-search-path>"));
        } else if input_paths.contains(absolute_path(cwd, Path::new(argument)).as_path()) {
            // Input membership is the changing part described by the manifest, not link structure.
        } else {
            hash_part(&mut hasher, OsStr::new(argument));
        }
    }
    *hasher.finalize().as_bytes()
}

fn is_rustc_temporary_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .as_bytes()
            .strip_prefix(b"rustc")
            .is_some_and(|suffix| {
                !suffix.is_empty() && suffix.iter().all(u8::is_ascii_alphanumeric)
            })
    })
}

fn output_path(cwd: &Path, arguments: &[String]) -> Option<PathBuf> {
    let mut arguments = arguments.iter().skip(1);
    while let Some(argument) = arguments.next() {
        let output = if argument == "-o" || argument == "--output" {
            arguments.next().map(String::as_str)
        } else {
            argument.strip_prefix("--output=").or_else(|| {
                argument
                    .strip_prefix("-o")
                    .filter(|output| !output.is_empty())
            })
        };
        if let Some(output) = output {
            return Some(absolute_path(cwd, Path::new(output)));
        }
    }
    None
}

fn absolute_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn digest_parts<'a>(parts: impl IntoIterator<Item = &'a OsStr>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hash_part(&mut hasher, part);
    }
    *hasher.finalize().as_bytes()
}

fn hash_part(hasher: &mut blake3::Hasher, part: &OsStr) {
    let bytes = part.as_bytes();
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn read_manifest(path: &Path) -> Option<Manifest> {
    let bytes = std::fs::read(path).ok()?;
    postcard::from_bytes(&bytes).ok()
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result {
    let bytes = postcard::to_stdvec(manifest)?;
    let temporary = temporary_path(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("failed to create jdxld state `{}`", temporary.display()))?;
    use std::io::Write as _;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("failed to publish jdxld state `{}`", path.display()))?;
    std::fs::File::open(path.parent().expect("manifest path has a parent"))?.sync_all()?;
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        ".{MANIFEST_FILE}.{}.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn compare(previous: Option<&Manifest>, current: &[InputIdentity]) -> StateObservation {
    let Some(previous) = previous else {
        return StateObservation {
            added: current.len(),
            ..StateObservation::default()
        };
    };
    let old = previous
        .inputs
        .iter()
        .map(|input| (input.path.as_slice(), input))
        .collect::<BTreeMap<_, _>>();
    let mut observation = StateObservation {
        previous_generation: Some(previous.generation),
        ..StateObservation::default()
    };
    let mut matched_old_paths = BTreeSet::new();
    let mut unmatched_current = Vec::new();
    for identity in current {
        if let Some(old) = old.get(identity.path.as_slice()) {
            matched_old_paths.insert(old.path.clone());
            if same_contents(old, identity) {
                observation.unchanged += 1;
            } else {
                observation.changed += 1;
            }
        } else {
            unmatched_current.push(identity);
        }
    }
    for identity in unmatched_current {
        if let Some((_, old)) = old.iter().find(|(_, old)| {
            !matched_old_paths.contains(&old.path)
                && old.content_digest.is_some()
                && old.content_digest == identity.content_digest
        }) {
            matched_old_paths.insert(old.path.clone());
            observation.unchanged += 1;
        } else {
            observation.added += 1;
        }
    }
    observation.removed = old.len().saturating_sub(matched_old_paths.len());
    observation
}

fn same_contents(old: &InputIdentity, current: &InputIdentity) -> bool {
    match (old.content_digest, current.content_digest) {
        (Some(old), Some(current)) => old == current,
        _ => old.modified_nanos == current.modified_nanos && old.len == current.len,
    }
}

impl From<ResolvedInput> for InputIdentity {
    fn from(input: ResolvedInput) -> Self {
        let modified_nanos = input
            .identity
            .modification_time
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            path: input.identity.path.into_os_string().into_encoded_bytes(),
            modified_nanos,
            len: input.identity.len,
            content_digest: input.content_digest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn input(path: &str, revision: u64) -> ResolvedInput {
        ResolvedInput {
            identity: CachedInputIdentity {
                path: PathBuf::from(path),
                modification_time: UNIX_EPOCH + Duration::from_secs(revision),
                len: revision,
            },
            content_digest: Some(*blake3::hash(&revision.to_le_bytes()).as_bytes()),
        }
    }

    #[test]
    fn state_survives_process_boundaries_and_reports_changes() {
        let root = tempfile::tempdir().unwrap();
        let first_arguments = vec![
            "jdxld".into(),
            "-L".into(),
            "/tmp/rustcOne/raw-dylibs".into(),
            "/tmp/rustc-one/symbols.o".into(),
            "/stable/one.o".into(),
            "/stable/two.o".into(),
            "-o".into(),
            "/workspace/output".into(),
        ];
        let first = record(
            root.path(),
            Path::new("/workspace"),
            &first_arguments,
            "test-version",
            vec![
                input("/tmp/rustc-one/symbols.o", 1),
                input("/stable/one.o", 1),
                input("/stable/two.o", 1),
            ],
        )
        .unwrap();
        assert_eq!(first.added, 3);

        let second_arguments = vec![
            "jdxld".into(),
            "-L".into(),
            "/tmp/rustcTwo/raw-dylibs".into(),
            "/tmp/rustc-two/symbols.o".into(),
            "/stable/one.o".into(),
            "/stable/two.o".into(),
            "/stable/three.o".into(),
            "-o".into(),
            "/workspace/output".into(),
        ];
        let second = record(
            root.path(),
            Path::new("/workspace"),
            &second_arguments,
            "test-version",
            vec![
                input("/tmp/rustc-two/symbols.o", 1),
                input("/stable/one.o", 1),
                input("/stable/two.o", 2),
                input("/stable/three.o", 1),
            ],
        )
        .unwrap();
        assert_eq!(
            second,
            StateObservation {
                previous_generation: Some(1),
                unchanged: 2,
                changed: 1,
                added: 1,
                removed: 0,
            }
        );

        let unchanged = record(
            root.path(),
            Path::new("/workspace"),
            &second_arguments,
            "test-version",
            vec![
                input("/tmp/rustc-two/symbols.o", 1),
                input("/stable/one.o", 1),
                input("/stable/two.o", 2),
                input("/stable/three.o", 1),
            ],
        )
        .unwrap();
        assert_eq!(unchanged.previous_generation, Some(2));

        let manifest_path = root
            .path()
            .join(hex::encode(digest_parts([OsStr::new("/workspace/output")])))
            .join(MANIFEST_FILE);
        assert_eq!(read_manifest(&manifest_path).unwrap().generation, 2);
    }
}
