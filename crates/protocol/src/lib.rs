//! Wire schema shared by the server and every client.
//!
//! All tiers (scarecrow → nursery → arboretum) answer identical RPCs, so
//! this crate doubles as the published protocol spec. Encoding TBD: the
//! canonical serialization must be byte-stable across implementations.
