//! gardenbed-protocol — the wire protocol crate.
//!
//! Owns the `.proto` schema ([`proto`]) and everything that moves between it
//! and [`gardenbed_model`]. Per v0.1 decision D4 the `.proto` plus
//! `docs/canonical-encoding.md` together form the published protocol; the
//! wire shapes mirror the canonical model exactly (ids as 32 raw bytes, ops
//! and modes with the same numeric values).
//!
//! Layers:
//! * [`proto`] — generated prost types plus the tonic `gardenbed_client` /
//!   `gardenbed_server` modules, re-exported here so the server and bed
//!   crates need only depend on `gardenbed-protocol`.
//! * [`convert`] — `TryFrom` bridges between wire messages and model types,
//!   mapping malformed wire data to [`ConversionError`].
//! * [`client`] — a thin typed wrapper [`GardenbedClient`] over the generated
//!   tonic client (TCP and unix-socket endpoints).

pub mod client;
pub mod convert;

pub use client::{CasOutcome, ConnectError, GardenbedClient};
pub use convert::ConversionError;

/// The generated protobuf namespace: messages, enums, and the raw tonic
/// client/server modules (`gardenbed_client`, `gardenbed_server`). Generated
/// by prost/tonic from `proto/gardenbed.proto` — never edit by hand.
pub mod proto {
    #![allow(clippy::all)]
    #![allow(rustdoc::all)]
    include!(concat!(env!("OUT_DIR"), "/gardenbed.protocol.v1.rs"));
}

/// Alias for [`proto`] — the generated wire namespace (messages + enums +
/// raw client/server modules).
pub use proto as pb;
/// The generated tonic gRPC client module (`GardenbedClient<T>`).
pub use proto::gardenbed_client;
/// The generated tonic gRPC server module (`Gardenbed` trait + `GardenbedServer<T>`).
pub use proto::gardenbed_server;
