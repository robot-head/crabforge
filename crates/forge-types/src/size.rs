//! Byte quantities, with the unit carried in the type.
//!
//! Sizes matter disproportionately on this stack. Crabka's wire frame is capped
//! at 100 MiB with no config key to raise it, git blobs are chunked to stay well
//! under that, and topic `segment.bytes` decides whether log compaction ever
//! fires. A MiB/MB or bytes/KiB slip in any of those is a production incident
//! that unit tests on bare integers tend not to catch — the number looks
//! plausible either way.
//!
//! [`ByteSize`] wraps `uom`'s `Information` quantity so the unit is part of the
//! type and conversions are exact integer arithmetic. Construction always names
//! a unit; there is no `ByteSize(4)` to misread.

use refinement_types::{Refinement, int::u64::NonZero};
use uom::si::{
    information::{byte, gibibyte, kibibyte, mebibyte},
    u64::Information,
};

/// A chunk size, which cannot be zero.
///
/// The predicate is part of the type, so [`chunk_count`] cannot be handed a
/// zero to divide by — not by convention, but because no such value can be
/// constructed. Compare the alternative: a `chunk: u64` parameter with a
/// comment asking callers not to pass zero.
pub type ChunkSize = Refinement<u64, NonZero>;

/// Build a [`ChunkSize`], panicking on zero.
///
/// For constants known at authoring time. Anything derived from input should
/// use `ChunkSize::refine` and handle the error.
pub fn chunk_size(bytes: u64) -> ChunkSize {
    ChunkSize::refine(bytes).expect("chunk size must be positive")
}

/// A quantity of bytes.
///
/// Ordering, addition and subtraction are unit-safe. Rendering for humans goes
/// through [`ByteSize::human`]; rendering for the broker goes through
/// [`ByteSize::as_config_value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(u64);

impl ByteSize {
    pub fn bytes(n: u64) -> Self {
        Self(Information::new::<byte>(n).get::<byte>())
    }

    pub fn kib(n: u64) -> Self {
        Self(Information::new::<kibibyte>(n).get::<byte>())
    }

    pub fn mib(n: u64) -> Self {
        Self(Information::new::<mebibyte>(n).get::<byte>())
    }

    pub fn gib(n: u64) -> Self {
        Self(Information::new::<gibibyte>(n).get::<byte>())
    }

    /// The underlying `uom` quantity, for arithmetic against other units.
    pub fn quantity(self) -> Information {
        Information::new::<byte>(self.0)
    }

    pub fn as_bytes(self) -> u64 {
        self.0
    }

    /// Topic configs and Kafka wire fields are signed; saturate rather than
    /// wrap so an absurd size becomes a rejected config instead of a negative
    /// one that the broker reads as "unlimited".
    pub fn as_i64(self) -> i64 {
        i64::try_from(self.0).unwrap_or(i64::MAX)
    }

    /// The spelling the broker expects in a topic config map.
    pub fn as_config_value(self) -> String {
        self.as_i64().to_string()
    }

    /// How many chunks of `chunk` this size splits into, rounding up.
    ///
    /// A zero-length object still occupies one chunk: git objects are
    /// content-addressed, and an empty blob is a real object with a real id.
    pub fn chunks_of(self, chunk: ByteSize) -> u64 {
        chunk_count(self.0, chunk_size(chunk.0))
    }

    pub fn human(self) -> String {
        const STEPS: &[(u64, &str)] = &[(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
        for (unit, label) in STEPS {
            if self.0 >= *unit {
                let whole = self.0 / unit;
                let frac = (self.0 % unit) * 10 / unit;
                return if frac == 0 {
                    format!("{whole} {label}")
                } else {
                    format!("{whole}.{frac} {label}")
                };
            }
        }
        format!("{} B", self.0)
    }
}

/// Number of `chunk`-sized pieces needed to hold `total` bytes.
///
/// Always at least one: a zero-length object is still an object, with an id of
/// its own. Taking a [`ChunkSize`] rather than a bare `u64` moves the "must not
/// be zero" precondition out of the documentation and into the signature —
/// there is no zero-valued `ChunkSize` to pass.
pub fn chunk_count(total: u64, chunk: ChunkSize) -> u64 {
    if total == 0 {
        return 1;
    }
    total.div_ceil(*chunk)
}

impl std::ops::Add for ByteSize {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self((self.quantity() + rhs.quantity()).get::<byte>())
    }
}

impl std::fmt::Display for ByteSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.human())
    }
}

/// Hard limits imposed by the crabka platform.
pub mod limits {
    use super::ByteSize;

    /// Crabka's maximum wire frame, matching Apache Kafka's default
    /// `socket.request.max.bytes`.
    ///
    /// This is a `pub const` in both the broker and the client with no config
    /// key, env var or TOML setting behind it — raising it means patching
    /// crabka and rebuilding. Everything the forge writes must fit underneath,
    /// including request framing overhead.
    pub fn max_frame() -> ByteSize {
        ByteSize::mib(100)
    }

    /// The largest object chunk the forge will produce, as a refined size.
    pub fn object_chunk_size() -> super::ChunkSize {
        super::chunk_size(object_chunk().as_bytes())
    }

    /// The largest object chunk the forge will produce.
    ///
    /// Far below [`max_frame`] on purpose. Crabka's own test suite tops out
    /// around 8 MiB and its benchmarks at 100 KiB, so multi-megabyte records
    /// are past the envelope upstream has exercised. 4 MiB keeps us inside it
    /// while still amortising per-record overhead across large blobs.
    pub fn object_chunk() -> ByteSize {
        ByteSize::mib(4)
    }

    /// Largest request body the HTTP layer accepts outside the git endpoints.
    pub fn max_api_body() -> ByteSize {
        ByteSize::mib(8)
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn unit_constructors_agree_on_byte_counts() {
        check!(ByteSize::kib(1).as_bytes() == 1024);
        check!(ByteSize::mib(1).as_bytes() == 1024 * 1024);
        check!(ByteSize::gib(1).as_bytes() == 1024 * 1024 * 1024);
        check!(ByteSize::mib(4) == ByteSize::kib(4096));
    }

    #[test]
    fn sizes_order_and_add_in_bytes() {
        check!(ByteSize::kib(1) < ByteSize::mib(1));
        check!(ByteSize::mib(1) + ByteSize::kib(1) == ByteSize::bytes(1024 * 1024 + 1024));
    }

    #[test]
    fn chunking_rounds_up_and_never_yields_zero_chunks() {
        let chunk = limits::object_chunk();
        check!(
            ByteSize::bytes(0).chunks_of(chunk) == 1,
            "empty blobs are real objects"
        );
        check!(ByteSize::bytes(1).chunks_of(chunk) == 1);
        check!(chunk.chunks_of(chunk) == 1);
        check!((chunk + ByteSize::bytes(1)).chunks_of(chunk) == 2);
        check!(ByteSize::mib(10).chunks_of(chunk) == 3);
    }

    #[test]
    fn a_zero_chunk_size_cannot_be_constructed() {
        // The precondition `chunk_count` used to state in a comment.
        check!(ChunkSize::refine(0).is_err());
        check!(ChunkSize::refine(1).is_ok());
    }

    #[test]
    fn every_chunk_fits_inside_the_wire_frame() {
        // The invariant the whole chunking scheme exists to preserve.
        check!(limits::object_chunk() < limits::max_frame());
    }

    #[test]
    fn config_values_are_rendered_as_signed_integers() {
        check!(ByteSize::mib(64).as_config_value() == "67108864");
    }

    #[test]
    fn human_rendering_picks_a_readable_unit() {
        check!(ByteSize::bytes(512).human() == "512 B");
        check!(ByteSize::kib(4).human() == "4 KiB");
        check!(ByteSize::mib(100).human() == "100 MiB");
        check!(ByteSize::bytes(1024 * 1024 * 3 / 2).human() == "1.5 MiB");
    }
}
