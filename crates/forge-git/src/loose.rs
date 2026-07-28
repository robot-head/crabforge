//! Reading and writing git's loose object format.
//!
//! The forge's local cache is a real bare git repository, because that is what
//! lets `git upload-pack` serve it — the protocol implementation stays git's,
//! which is the only implementation nobody has to trust.
//!
//! A loose object is zlib-compressed `"<kind> <len>\0<content>"` at
//! `objects/<first two hex>/<remaining 38>`. That is the whole format, so the
//! forge writes it directly rather than shelling out per object.

use std::{
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use forge_types::Oid;

use crate::frame::Kind;

#[derive(Debug, thiserror::Error)]
pub enum LooseError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("object header is malformed")]
    BadHeader,
    #[error("unknown object kind '{0}'")]
    UnknownKind(String),
}

/// Where a loose object lives inside an object directory.
pub fn object_path(objects_dir: &Path, oid: Oid) -> PathBuf {
    let hex = oid.to_hex();
    objects_dir.join(&hex[0..2]).join(&hex[2..])
}

/// Write an object into `objects_dir`.
///
/// Idempotent: an object already present is left alone rather than rewritten.
/// Objects are immutable and content-addressed, so a second write could only
/// produce identical bytes — and skipping it makes a cache rebuild cheap when
/// most of the objects are already there.
pub fn write(objects_dir: &Path, oid: Oid, kind: Kind, content: &[u8]) -> Result<bool, LooseError> {
    let path = object_path(objects_dir, oid);
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(kind.as_str().as_bytes())?;
    encoder.write_all(b" ")?;
    encoder.write_all(content.len().to_string().as_bytes())?;
    encoder.write_all(&[0u8])?;
    encoder.write_all(content)?;
    let compressed = encoder.finish()?;

    // Write to a temporary name and rename, so a reader never sees a partial
    // object. Git does the same, and a torn object would fail a clone in a way
    // that is very hard to diagnose.
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temp, &compressed)?;
    std::fs::rename(&temp, &path)?;
    Ok(true)
}

/// Read an object back.
pub fn read(objects_dir: &Path, oid: Oid) -> Result<Option<(Kind, Vec<u8>)>, LooseError> {
    let path = object_path(objects_dir, oid);
    if !path.exists() {
        return Ok(None);
    }
    let compressed = std::fs::read(&path)?;
    let mut decoder = ZlibDecoder::new(&compressed[..]);
    let mut raw = Vec::new();
    decoder.read_to_end(&mut raw)?;

    let nul = raw
        .iter()
        .position(|b| *b == 0)
        .ok_or(LooseError::BadHeader)?;
    let header = std::str::from_utf8(&raw[..nul]).map_err(|_| LooseError::BadHeader)?;
    let (kind, _len) = header.split_once(' ').ok_or(LooseError::BadHeader)?;
    let kind = Kind::parse(kind).ok_or_else(|| LooseError::UnknownKind(kind.to_string()))?;

    Ok(Some((kind, raw[nul + 1..].to_vec())))
}

/// Whether an object is already present.
pub fn contains(objects_dir: &Path, oid: Oid) -> bool {
    object_path(objects_dir, oid).exists()
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;
    use crate::frame::compute_oid;

    fn temp_objects() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn an_object_round_trips_through_the_loose_format() {
        let dir = temp_objects();
        let content = b"hello from the log\n";
        let oid = compute_oid(Kind::Blob, content);

        check!(write(dir.path(), oid, Kind::Blob, content).unwrap());
        let (kind, read_back) = read(dir.path(), oid).unwrap().unwrap();
        check!(kind == Kind::Blob);
        check!(read_back == content);
    }

    #[test]
    fn writing_an_existing_object_is_a_no_op() {
        // Makes an incremental cache rebuild cheap.
        let dir = temp_objects();
        let oid = compute_oid(Kind::Blob, b"once");

        check!(write(dir.path(), oid, Kind::Blob, b"once").unwrap());
        check!(!write(dir.path(), oid, Kind::Blob, b"once").unwrap());
    }

    #[test]
    fn objects_are_sharded_by_the_first_byte_of_their_id() {
        // Git's layout, which `git upload-pack` requires.
        let dir = temp_objects();
        let oid = compute_oid(Kind::Blob, b"sharded");
        write(dir.path(), oid, Kind::Blob, b"sharded").unwrap();

        let hex = oid.to_hex();
        let expected = dir.path().join(&hex[0..2]).join(&hex[2..]);
        check!(expected.exists(), "expected {expected:?}");
    }

    #[test]
    fn a_missing_object_reads_as_none() {
        let dir = temp_objects();
        let oid = compute_oid(Kind::Blob, b"never written");
        check!(read(dir.path(), oid).unwrap().is_none());
        check!(!contains(dir.path(), oid));
    }

    #[test]
    fn every_kind_round_trips() {
        let dir = temp_objects();
        for kind in [Kind::Commit, Kind::Tree, Kind::Blob, Kind::Tag] {
            let content = format!("body of a {}", kind.as_str());
            let oid = compute_oid(kind, content.as_bytes());
            write(dir.path(), oid, kind, content.as_bytes()).unwrap();
            let (read_kind, body) = read(dir.path(), oid).unwrap().unwrap();
            check!(read_kind == kind);
            check!(body == content.as_bytes());
        }
    }

    #[test]
    fn an_empty_object_round_trips() {
        let dir = temp_objects();
        let oid = compute_oid(Kind::Blob, b"");
        write(dir.path(), oid, Kind::Blob, b"").unwrap();
        let (_, content) = read(dir.path(), oid).unwrap().unwrap();
        check!(content.is_empty());
    }

    #[test]
    fn no_temporary_files_are_left_behind() {
        // A stray temp file in an object shard would confuse git's own
        // enumeration of loose objects.
        let dir = temp_objects();
        let oid = compute_oid(Kind::Blob, b"tidy");
        write(dir.path(), oid, Kind::Blob, b"tidy").unwrap();

        let shard = dir.path().join(&oid.to_hex()[0..2]);
        let entries: Vec<_> = std::fs::read_dir(shard)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        check!(entries.len() == 1, "found {entries:?}");
        check!(!entries[0].contains("tmp"));
    }
}
