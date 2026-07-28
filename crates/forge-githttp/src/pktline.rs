//! Git's pkt-line framing.
//!
//! Only enough of it to wrap the service advertisement: each packet is a
//! four-hex-digit length prefix covering the prefix itself, and `0000` is a
//! flush packet. Everything else on the wire is produced and consumed by
//! `git upload-pack` directly.

/// Wrap a payload in a length-prefixed packet.
pub fn encode(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() + 4;
    let mut out = format!("{len:04x}").into_bytes();
    out.extend_from_slice(payload);
    out
}

/// The flush packet, which ends a section.
pub fn flush() -> &'static [u8] {
    b"0000"
}

/// The header git expects before a service's reference advertisement.
pub fn service_header(service: &str) -> Vec<u8> {
    let mut out = encode(format!("# service={service}\n").as_bytes());
    out.extend_from_slice(flush());
    out
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn a_packet_length_covers_its_own_prefix() {
        // "0009" = 4 prefix bytes + 5 payload bytes.
        check!(encode(b"hello") == b"0009hello");
    }

    #[test]
    fn an_empty_payload_is_a_four_byte_packet() {
        check!(encode(b"") == b"0004");
    }

    #[test]
    fn the_service_header_matches_what_git_sends() {
        // Exactly the bytes a real server emits for a fetch advertisement.
        let header = service_header("git-upload-pack");
        check!(header == b"001e# service=git-upload-pack\n0000");
    }

    #[test]
    fn lengths_are_lowercase_hex() {
        let long = vec![b'x'; 0xab - 4];
        let encoded = encode(&long);
        check!(&encoded[..4] == b"00ab");
    }
}
