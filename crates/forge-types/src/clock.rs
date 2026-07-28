//! Timestamps.
//!
//! Everything in the forge that records a time uses [`now`], which truncates to
//! microseconds.
//!
//! The reason is storage: PostgreSQL's `timestamptz` holds microseconds, so a
//! nanosecond-precision value does not survive a round trip through gres. Left
//! alone, that produces a class of bug where a value read back is *almost*
//! equal to the one written — comparisons fail, idempotency checks think a
//! record changed, and tests fail in ways that look like flakes. Adopting the
//! storage layer's precision everywhere makes "what I wrote is what I read"
//! true by construction.

use time::OffsetDateTime;

/// The current time, at the precision the database can store.
pub fn now() -> OffsetDateTime {
    truncate_to_micros(OffsetDateTime::now_utc())
}

/// Drop sub-microsecond precision.
pub fn truncate_to_micros(t: OffsetDateTime) -> OffsetDateTime {
    let micros = t.nanosecond() / 1_000;
    t.replace_nanosecond(micros * 1_000)
        .expect("a truncated nanosecond count is always in range")
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use time::macros::datetime;

    use super::*;

    #[test]
    fn truncation_drops_only_sub_microsecond_digits() {
        let t = datetime!(2026-07-28 12:34:56.123456789 UTC);
        let truncated = truncate_to_micros(t);
        check!(truncated.nanosecond() == 123_456_000);
        check!(truncated.unix_timestamp() == t.unix_timestamp());
    }

    #[test]
    fn truncation_is_idempotent() {
        let once = now();
        check!(truncate_to_micros(once) == once);
    }

    #[test]
    fn now_survives_a_database_round_trip_unchanged() {
        // What `timestamptz` will hand back is exactly what we minted.
        let t = now();
        check!(t.nanosecond() % 1_000 == 0);
    }
}
