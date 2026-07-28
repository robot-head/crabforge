//! The on-log representation of a git object.
//!
//! Git objects are stored as individual records in a per-repository compacted
//! topic, keyed by object id. Because objects are content-addressed and
//! immutable, that gives deduplication for free: re-pushing an object rewrites
//! the same key, and compaction collapses it.
//!
//! ## Framing
//!
//! ```text
//! magic        4    "FGO1"
//! kind         1    1=commit 2=tree 3=blob 4=tag
//! flags        1    bit0 CHUNKED (this is a manifest), bit1 IS_CHUNK
//! total_len    8    length of the full object content, little-endian
//! chunk_count  4    manifest only; 0 otherwise
//! data         ..   canonical object content (empty in a manifest)
//! ```
//!
//! Content is stored uncompressed, in git's canonical form, so the object id
//! can be recomputed and checked on the way in and on the way out. A corrupt
//! record is then detectable rather than silently serving bad data to a clone.
//!
//! ## Chunking
//!
//! Crabka's wire frame is capped at 100 MiB by a constant with no configuration
//! behind it, and crabka's own tests top out around 8 MiB, so a large blob is
//! split into 4 MiB chunks (see [`forge_types::limits::object_chunk`]). The base
//! key holds a manifest naming the chunk count; chunk `i` lives at
//! `<key>/c/<i>`. Small objects — the overwhelming majority — are written whole
//! with no manifest and no extra round trips.

use forge_types::{ByteSize, ChunkSize, Oid, limits};

const MAGIC: &[u8; 4] = b"FGO1";
const HEADER_LEN: usize = 4 + 1 + 1 + 8 + 4;

const FLAG_CHUNKED: u8 = 0b0000_0001;
const FLAG_IS_CHUNK: u8 = 0b0000_0010;

/// A git object's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl Kind {
    fn to_byte(self) -> u8 {
        match self {
            Self::Commit => 1,
            Self::Tree => 2,
            Self::Blob => 3,
            Self::Tag => 4,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::Commit),
            2 => Some(Self::Tree),
            3 => Some(Self::Blob),
            4 => Some(Self::Tag),
            _ => None,
        }
    }

    /// The word git uses in an object header.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Tree => "tree",
            Self::Blob => "blob",
            Self::Tag => "tag",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "commit" => Some(Self::Commit),
            "tree" => Some(Self::Tree),
            "blob" => Some(Self::Blob),
            "tag" => Some(Self::Tag),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("record is too short to be a frame ({0} bytes)")]
    TooShort(usize),
    #[error("not an FGO1 frame")]
    BadMagic,
    #[error("unknown object kind {0}")]
    BadKind(u8),
    #[error("frame claims {claimed} bytes of content but carries {actual}")]
    LengthMismatch { claimed: u64, actual: usize },
    #[error("object id mismatch: frame says {expected}, content hashes to {actual}")]
    CorruptContent { expected: Oid, actual: Oid },
    #[error("chunk {index} is missing")]
    MissingChunk { index: u32 },
}

/// A decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A complete object.
    Whole { kind: Kind, content: Vec<u8> },
    /// The head of a chunked object: names the parts without carrying them.
    Manifest {
        kind: Kind,
        total_len: u64,
        chunk_count: u32,
    },
    /// One part of a chunked object.
    Chunk { kind: Kind, data: Vec<u8> },
}

/// Record key for an object.
pub fn object_key(oid: Oid) -> String {
    format!("o/{oid}")
}

/// Record key for chunk `index` of an object.
///
/// Zero-padded so keys sort in chunk order, which makes a fold that buffers by
/// key prefix cheap to reason about.
pub fn chunk_key(oid: Oid, index: u32) -> String {
    format!("o/{oid}/c/{index:06}")
}

/// Split a record key back into its object id and optional chunk index.
pub fn parse_key(key: &str) -> Option<(Oid, Option<u32>)> {
    let rest = key.strip_prefix("o/")?;
    match rest.split_once("/c/") {
        Some((oid, index)) => Some((oid.parse().ok()?, Some(index.parse().ok()?))),
        None => Some((rest.parse().ok()?, None)),
    }
}

/// Encode a whole object.
pub fn encode_whole(kind: Kind, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + content.len());
    write_header(&mut out, kind, 0, content.len() as u64, 0);
    out.extend_from_slice(content);
    out
}

/// Encode the manifest for a chunked object.
pub fn encode_manifest(kind: Kind, total_len: u64, chunk_count: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    write_header(&mut out, kind, FLAG_CHUNKED, total_len, chunk_count);
    out
}

/// Encode one chunk.
pub fn encode_chunk(kind: Kind, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + data.len());
    write_header(&mut out, kind, FLAG_IS_CHUNK, data.len() as u64, 0);
    out.extend_from_slice(data);
    out
}

fn write_header(out: &mut Vec<u8>, kind: Kind, flags: u8, len: u64, chunk_count: u32) {
    out.extend_from_slice(MAGIC);
    out.push(kind.to_byte());
    out.push(flags);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&chunk_count.to_le_bytes());
}

/// Decode a frame.
pub fn decode(bytes: &[u8]) -> Result<Frame, FrameError> {
    if bytes.len() < HEADER_LEN {
        return Err(FrameError::TooShort(bytes.len()));
    }
    if &bytes[0..4] != MAGIC {
        return Err(FrameError::BadMagic);
    }
    let kind = Kind::from_byte(bytes[4]).ok_or(FrameError::BadKind(bytes[4]))?;
    let flags = bytes[5];
    let claimed = u64::from_le_bytes(bytes[6..14].try_into().expect("8 bytes"));
    let chunk_count = u32::from_le_bytes(bytes[14..18].try_into().expect("4 bytes"));
    let body = &bytes[HEADER_LEN..];

    if flags & FLAG_CHUNKED != 0 {
        return Ok(Frame::Manifest {
            kind,
            total_len: claimed,
            chunk_count,
        });
    }

    if claimed != body.len() as u64 {
        return Err(FrameError::LengthMismatch {
            claimed,
            actual: body.len(),
        });
    }

    if flags & FLAG_IS_CHUNK != 0 {
        Ok(Frame::Chunk {
            kind,
            data: body.to_vec(),
        })
    } else {
        Ok(Frame::Whole {
            kind,
            content: body.to_vec(),
        })
    }
}

/// How a single object should be written to the log.
pub struct Encoded {
    /// `(key, value)` pairs, in the order they should be produced.
    pub records: Vec<(String, Vec<u8>)>,
}

/// Encode an object, chunking it if it exceeds the chunk size.
pub fn encode_object(oid: Oid, kind: Kind, content: &[u8]) -> Encoded {
    let chunk_size = limits::object_chunk();
    let size = ByteSize::bytes(content.len() as u64);

    if size <= chunk_size {
        return Encoded {
            records: vec![(object_key(oid), encode_whole(kind, content))],
        };
    }

    let chunk_bytes = chunk_size.as_bytes() as usize;
    let count = chunk_count_for(content.len(), limits::object_chunk_size());
    let mut records = Vec::with_capacity(count as usize + 1);
    records.push((
        object_key(oid),
        encode_manifest(kind, content.len() as u64, count),
    ));
    for (index, part) in content.chunks(chunk_bytes).enumerate() {
        records.push((chunk_key(oid, index as u32), encode_chunk(kind, part)));
    }
    Encoded { records }
}

/// Number of chunks an object of `len` bytes occupies.
///
/// Only called when `len` exceeds the chunk size, so the result is at least
/// two. The chunk size arrives as a [`forge_types::ChunkSize`], which cannot be
/// zero — so there is no division-by-zero case to guard against here.
fn chunk_count_for(len: usize, chunk: ChunkSize) -> u32 {
    len.div_ceil(*chunk as usize) as u32
}

/// Reassemble a chunked object from its manifest and parts.
///
/// `chunks` must be indexed by chunk number.
pub fn reassemble(
    total_len: u64,
    chunk_count: u32,
    chunks: &[Option<Vec<u8>>],
) -> Result<Vec<u8>, FrameError> {
    let mut content = Vec::with_capacity(total_len as usize);
    for index in 0..chunk_count {
        let part = chunks
            .get(index as usize)
            .and_then(Option::as_ref)
            .ok_or(FrameError::MissingChunk { index })?;
        content.extend_from_slice(part);
    }
    if content.len() as u64 != total_len {
        return Err(FrameError::LengthMismatch {
            claimed: total_len,
            actual: content.len(),
        });
    }
    Ok(content)
}

/// Compute a git object id: SHA-1 over `"<kind> <len>\0<content>"`.
///
/// Reimplemented rather than delegated so an object can be verified without a
/// repository open, which is what the log fold does on every record.
pub fn compute_oid(kind: Kind, content: &[u8]) -> Oid {
    use sha1::{Digest as _, Sha1};

    let mut hasher = Sha1::new();
    hasher.update(kind.as_str().as_bytes());
    hasher.update(b" ");
    hasher.update(content.len().to_string().as_bytes());
    hasher.update([0u8]);
    hasher.update(content);
    Oid::from_bytes(hasher.finalize().into())
}

/// Verify that `content` really is the object `oid` names.
pub fn verify(oid: Oid, kind: Kind, content: &[u8]) -> Result<(), FrameError> {
    let actual = compute_oid(kind, content);
    if actual == oid {
        Ok(())
    } else {
        Err(FrameError::CorruptContent {
            expected: oid,
            actual,
        })
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    fn oid_of(content: &[u8]) -> Oid {
        compute_oid(Kind::Blob, content)
    }

    #[test]
    fn a_whole_object_round_trips() {
        let content = b"hello, forge";
        let encoded = encode_whole(Kind::Blob, content);
        assert!(let Ok(Frame::Whole { kind, content: decoded }) = decode(&encoded));
        check!(kind == Kind::Blob);
        check!(decoded == content);
    }

    #[test]
    fn an_empty_object_round_trips() {
        // The empty blob is a real object with a real id; git uses it often.
        let encoded = encode_whole(Kind::Blob, b"");
        assert!(let Ok(Frame::Whole { content, .. }) = decode(&encoded));
        check!(content.is_empty());
    }

    #[test]
    fn object_ids_match_what_git_computes() {
        // The canonical id of the empty blob, verifiable with
        // `printf '' | git hash-object --stdin`.
        check!(compute_oid(Kind::Blob, b"").to_hex() == "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        // And of a blob containing "hello\n".
        check!(
            compute_oid(Kind::Blob, b"hello\n").to_hex()
                == "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn verification_catches_corrupted_content() {
        let content = b"the original";
        let oid = oid_of(content);
        check!(verify(oid, Kind::Blob, content).is_ok());
        assert!(let Err(FrameError::CorruptContent { .. }) = verify(oid, Kind::Blob, b"tampered"));
    }

    #[test]
    fn small_objects_are_written_as_a_single_record() {
        let content = vec![7u8; 1024];
        let encoded = encode_object(oid_of(&content), Kind::Blob, &content);
        check!(encoded.records.len() == 1, "no manifest for a small object");
        check!(!encoded.records[0].0.contains("/c/"));
    }

    #[test]
    fn an_object_exactly_at_the_chunk_size_is_still_one_record() {
        let content = vec![0u8; limits::object_chunk().as_bytes() as usize];
        let encoded = encode_object(oid_of(&content), Kind::Blob, &content);
        check!(encoded.records.len() == 1, "the boundary is inclusive");
    }

    #[test]
    fn a_large_object_becomes_a_manifest_plus_chunks() {
        let chunk = limits::object_chunk().as_bytes() as usize;
        let content = vec![3u8; chunk + 1];
        let oid = oid_of(&content);
        let encoded = encode_object(oid, Kind::Blob, &content);

        check!(encoded.records.len() == 3, "manifest plus two chunks");
        check!(encoded.records[0].0 == object_key(oid));
        check!(encoded.records[1].0 == chunk_key(oid, 0));
        check!(encoded.records[2].0 == chunk_key(oid, 1));

        assert!(let Ok(Frame::Manifest { total_len, chunk_count, .. }) = decode(&encoded.records[0].1));
        check!(total_len == content.len() as u64);
        check!(chunk_count == 2);
    }

    #[test]
    fn a_chunked_object_reassembles_to_its_original_bytes() {
        let chunk = limits::object_chunk().as_bytes() as usize;
        let content: Vec<u8> = (0..chunk * 2 + 517).map(|i| (i % 251) as u8).collect();
        let oid = oid_of(&content);
        let encoded = encode_object(oid, Kind::Blob, &content);

        assert!(let Ok(Frame::Manifest { total_len, chunk_count, .. }) = decode(&encoded.records[0].1));
        let mut parts = vec![None; chunk_count as usize];
        for (key, value) in &encoded.records[1..] {
            let (_, index) = parse_key(key).unwrap();
            assert!(let Ok(Frame::Chunk { data, .. }) = decode(value));
            parts[index.unwrap() as usize] = Some(data);
        }

        let rebuilt = reassemble(total_len, chunk_count, &parts).unwrap();
        check!(rebuilt == content);
        // And it still hashes to the id it was stored under.
        check!(verify(oid, Kind::Blob, &rebuilt).is_ok());
    }

    #[test]
    fn a_missing_chunk_is_an_error_not_a_truncated_object() {
        // Serving a silently truncated blob would corrupt a clone.
        let parts = vec![Some(vec![1u8; 10]), None];
        assert!(let Err(FrameError::MissingChunk { index: 1 }) = reassemble(20, 2, &parts));
    }

    #[test]
    fn keys_round_trip() {
        let oid = oid_of(b"whatever");
        check!(parse_key(&object_key(oid)) == Some((oid, None)));
        check!(parse_key(&chunk_key(oid, 42)) == Some((oid, Some(42))));
        check!(parse_key("nonsense").is_none());
    }

    #[test]
    fn chunk_keys_sort_in_chunk_order() {
        // Zero padding, so lexicographic order is numeric order.
        let oid = oid_of(b"ordered");
        let mut keys: Vec<String> = (0..12).map(|i| chunk_key(oid, i)).collect();
        let expected = keys.clone();
        keys.sort();
        check!(keys == expected);
    }

    #[test]
    fn every_object_kind_survives_encoding() {
        for kind in [Kind::Commit, Kind::Tree, Kind::Blob, Kind::Tag] {
            let encoded = encode_whole(kind, b"body");
            assert!(let Ok(Frame::Whole { kind: decoded, .. }) = decode(&encoded));
            check!(decoded == kind);
            check!(Kind::parse(kind.as_str()) == Some(kind));
        }
    }

    #[test]
    fn malformed_records_are_rejected_rather_than_misread() {
        assert!(let Err(FrameError::TooShort(_)) = decode(b"FGO"));
        assert!(let Err(FrameError::BadMagic) = decode(b"XXXX\x03\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"));

        let mut bad_kind = encode_whole(Kind::Blob, b"x");
        bad_kind[4] = 99;
        assert!(let Err(FrameError::BadKind(99)) = decode(&bad_kind));

        let mut truncated = encode_whole(Kind::Blob, b"hello");
        truncated.truncate(truncated.len() - 2);
        assert!(let Err(FrameError::LengthMismatch { .. }) = decode(&truncated));
    }

    #[test]
    fn chunks_never_exceed_the_brokers_frame_limit() {
        // The invariant the whole chunking scheme exists to preserve.
        let chunk = limits::object_chunk().as_bytes() as usize;
        let content = vec![0u8; chunk * 3];
        let encoded = encode_object(oid_of(&content), Kind::Blob, &content);
        for (key, value) in &encoded.records {
            check!(
                ByteSize::bytes(value.len() as u64) < limits::max_frame(),
                "record {key} would not fit in a broker frame"
            );
        }
    }
}
