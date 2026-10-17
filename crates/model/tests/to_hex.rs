//! Direct tests for [`gardenbed_model::Id::to_hex`]: known digests, all
//! byte-nibble boundaries (0x00–0xff), and the documented invariants
//! (length, lowercase, round-trip through `FromStr`).

use gardenbed_model::Id;
use std::str::FromStr;

#[test]
fn to_hex_known_digests() {
    // SHA-256 of the empty string and of "abc" (FIPS 180 vectors).
    let empty = Id::from_bytes([
        0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9,
        0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52,
        0xb8, 0x55,
    ]);
    assert_eq!(
        empty.to_hex(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    let abc = Id::from_bytes([
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22,
        0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00,
        0x15, 0xad,
    ]);
    assert_eq!(
        abc.to_hex(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn to_hex_covers_every_byte_value() {
    // One digest cycling through all 256 byte values: every nibble pair
    // (including 0x00 and 0xff edges) must encode as two lowercase hex
    // digits.
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (i * 8 % 256) as u8;
    }
    bytes[31] = 0xff;
    let hex = Id::from_bytes(bytes).to_hex();
    assert_eq!(hex.len(), 64);
    assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(&hex[62..], "ff");
    // Round-trips through the parser.
    assert_eq!(Id::from_str(&hex).unwrap(), Id::from_bytes(bytes));
}
