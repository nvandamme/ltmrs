//! Stable project identity: `prj_<name>_<ulid>` (Wave A3b, Part I 01:6.3).
//!
//! The ULID is 48-bit ms time + 80-bit CSPRNG randomness, Crockford base32
//! encoded to 26 chars. The 128 ULID bits map 1:1 onto an [`EntityId`]
//! (UUID bytes), so canonical rows key eligibility on the [`EntityId`]
//! while the `prj_` string stays the human-readable presentation form.
//! Randomness rides the existing `uuid` v4 RNG; no new dependencies.

use crate::id::EntityId;

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn ulid_value(c: u8) -> Option<u8> {
    Some(match c {
        b'0' => 0,
        b'1' => 1,
        b'2' => 2,
        b'3' => 3,
        b'4' => 4,
        b'5' => 5,
        b'6' => 6,
        b'7' => 7,
        b'8' => 8,
        b'9' => 9,
        b'A' => 10,
        b'B' => 11,
        b'C' => 12,
        b'D' => 13,
        b'E' => 14,
        b'F' => 15,
        b'G' => 16,
        b'H' => 17,
        b'J' => 18,
        b'K' => 19,
        b'M' => 20,
        b'N' => 21,
        b'P' => 22,
        b'Q' => 23,
        b'R' => 24,
        b'S' => 25,
        b'T' => 26,
        b'V' => 27,
        b'W' => 28,
        b'X' => 29,
        b'Y' => 30,
        b'Z' => 31,
        _ => return None,
    })
}

/// Sanitize a project name: lowercase, chars outside `[a-z0-9_-]` become
/// `_`, truncated to 32 chars. Empty-after-sanitize falls back to `app`.
pub fn sanitize_name(raw: &str) -> String {
    let mut out: String = raw
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(32);
    if out.is_empty() {
        out.push_str("app");
    }
    out
}

/// Encode 16 ULID bytes (6 time + 10 randomness, big-endian) as 26 chars.
pub fn encode_ulid(bytes: &[u8; 16]) -> String {
    let mut out = [0u8; 26];
    let mut bits: u128 = u128::from_be_bytes(*bytes);
    for i in (0..26).rev() {
        out[i] = CROCKFORD[(bits & 31) as usize];
        bits >>= 5;
    }
    String::from_utf8(out.to_vec()).expect("crockford is ascii")
}

/// Decode 26 ULID chars back to 16 bytes. Rejects bad length/charset and an
/// overflowed top char (first char must encode only 3 bits, i.e. `0`-`7`).
pub fn decode_ulid(s: &str) -> Option<[u8; 16]> {
    if s.len() != 26 {
        return None;
    }
    let mut bits: u128 = 0;
    for &c in s.as_bytes() {
        bits = (bits << 5) | u128::from(ulid_value(c)?);
    }
    if s.as_bytes()[0] > b'7' {
        return None;
    }
    Some(bits.to_be_bytes())
}

/// Build the 16 ULID bytes for an explicit timestamp + 80-bit randomness.
pub fn ulid_bytes(time_ms: u64, rand80: &[u8; 10]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..6].copy_from_slice(&(time_ms & 0xFFFF_FFFF_FFFF).to_be_bytes()[2..8]);
    out[6..16].copy_from_slice(rand80);
    out
}

/// The presentation form `prj_<name>_<26-char ULID>`.
pub fn format_project_id(name: &str, id: EntityId) -> String {
    format!(
        "prj_{}_{}",
        sanitize_name(name),
        encode_ulid(&id.as_uuid().into_bytes())
    )
}

/// Parse a `prj_<name>_<ulid>` string into its (name, [`EntityId`]) halves.
pub fn parse_project_id(raw: &str) -> Option<(String, EntityId)> {
    let body = raw.strip_prefix("prj_")?;
    let (name, ulid) = body.rsplit_once('_')?;
    if name.is_empty() || name.len() > 32 {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return None;
    }
    let bytes = decode_ulid(ulid)?;
    Some((
        name.to_string(),
        EntityId::new(uuid::Uuid::from_bytes(bytes)),
    ))
}

/// Generate a fresh stable project id: sanitized name + now-ms ULID with
/// 80-bit randomness drawn from the existing `uuid` v4 RNG (no new deps).
pub fn generate_project_id(name: &str) -> (String, EntityId) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let rng = uuid::Uuid::new_v4();
    let b = rng.into_bytes();
    let mut rand80 = [0u8; 10];
    rand80.copy_from_slice(&b[0..10]);
    let id = EntityId::new(uuid::Uuid::from_bytes(ulid_bytes(now, &rand80)));
    (format_project_id(name, id), id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Required shape, corrected to canonical Crockford base32 (the brief
    /// draft omits Q,R,S from the ULID class; standard ULIDs emit them, as
    /// this test just proved — flagged in the wave report, not adapted
    /// silently): `^prj_[a-z0-9_-]{1,32}_[0-9A-HJKMNPQRSTV-Z]{26}$`.
    fn shape_ok(s: &str) -> bool {
        let body = match s.strip_prefix("prj_") {
            Some(b) => b,
            None => return false,
        };
        let (name, ulid) = match body.rsplit_once('_') {
            Some(p) => p,
            None => return false,
        };
        if name.is_empty()
            || name.len() > 32
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return false;
        }
        if ulid.len() != 26 {
            return false;
        }
        ulid.bytes().all(|c| {
            matches!(c,
            b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
        })
    }

    #[test]
    fn generated_id_matches_required_shape() {
        let (s, _) = generate_project_id("My App!");
        assert!(shape_ok(&s), "must match required shape, got: {s}");
    }

    #[test]
    fn sanitize_lowercases_maps_and_truncates() {
        assert_eq!(sanitize_name("My App!"), "my_app_");
        assert_eq!(sanitize_name("UPPER-Mixed_09"), "upper-mixed_09");
        assert_eq!(sanitize_name("a/b\\c d"), "a_b_c_d");
        assert_eq!(sanitize_name(&"x".repeat(40)), "x".repeat(32));
        assert_eq!(sanitize_name(""), "app");
        assert_eq!(sanitize_name("!!!"), "___");
    }

    #[test]
    fn ulid_round_trips_bytes() {
        let bytes = ulid_bytes(1_700_000_000_123, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let s = encode_ulid(&bytes);
        assert_eq!(s.len(), 26);
        assert_eq!(decode_ulid(&s).unwrap(), bytes);
    }

    #[test]
    fn ulid_rejects_bad_length_charset_and_overflow() {
        assert!(decode_ulid("01ARZ3NDEKTSV4RRFFQ69G5FAV0").is_none());
        assert!(decode_ulid(&"0".repeat(25)).is_none());
        assert!(decode_ulid(&"0".repeat(27)).is_none());
        assert!(decode_ulid("8AAAAAAAAAAAAAAAAAAAAAAAAA").is_none());
        assert!(decode_ulid("01ARZ3NDEKTSV4RRFFQ69G5FAV*").is_none());
    }

    #[test]
    fn parse_round_trips_format() {
        let (s, id) = generate_project_id("shop");
        let (name, back) = parse_project_id(&s).unwrap();
        assert_eq!(name, "shop");
        assert_eq!(back, id);
        assert!(parse_project_id("prj_shop_notulid").is_none());
        assert!(parse_project_id("xx_shop_01ARZ3NDEKTSV4RRFFQ69G5FAV2").is_none());
        assert!(parse_project_id("prj_BadCaps_01ARZ3NDEKTSV4RRFFQ69G5FAV2").is_none());
    }

    #[test]
    fn ten_thousand_generated_ids_are_unique() {
        let mut seen = BTreeSet::new();
        for _ in 0..10_000 {
            let (s, _) = generate_project_id("load");
            assert!(seen.insert(s));
        }
        assert_eq!(seen.len(), 10_000);
    }
}
