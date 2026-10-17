//! Reflection over the published protocol's descriptor set.
//!
//! The build script persists the compiled `FileDescriptorSet` (produced by
//! protox — no protoc) next to the generated code. These tests assert the
//! *published* shape of the protocol from that descriptor set, independent
//! of the generated Rust code: every RPC of v0.1.md §5 exists, every request
//! carries `repo` as field #1, the enums mirror the canonical op/mode bytes,
//! and no response message ever carries blob content.
//!
//! prost-types 0.14 mirrors the descriptor.proto optionality faithfully, so
//! every scalar/string field is an `Option`; the accessors below unwrap the
//! invariants this file guarantees (names, numbers, types are always set).

use prost::Message as _;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{
    DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorSet,
    ServiceDescriptorProto,
};
use std::sync::OnceLock;

const FDS_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gardenbed.fds.bin"));

const PACKAGE: &str = "gardenbed.protocol.v1";
const SERVICE: &str = "Gardenbed";

/// Every RPC of §5, in declared order, with its streaming flags
/// `(name, client_streaming, server_streaming)`.
const RPCS: &[(&str, bool, bool)] = &[
    ("RootTree", false, false),
    ("GetTree", false, false),
    ("GetBlob", false, true),
    ("GetChangeset", false, false),
    ("PutBlob", true, false),
    ("CreateChangeset", false, false),
    ("MergeBase", false, false),
    ("IsAncestor", false, false),
    ("Diff", false, true),
    ("Log", false, true),
    ("Blame", false, true),
    ("ListRefs", false, true),
    ("GetRef", false, false),
    ("CasRef", false, false),
    ("Rebase", false, false),
    ("Publish", false, false),
    ("Merge", false, false),
    ("GetStats", false, false),
];

fn fds() -> &'static FileDescriptorSet {
    static FDS: OnceLock<FileDescriptorSet> = OnceLock::new();
    FDS.get_or_init(|| FileDescriptorSet::decode(FDS_BYTES).expect("descriptor set decodes"))
}

fn file() -> &'static prost_types::FileDescriptorProto {
    fds()
        .file
        .iter()
        .find(|f| f.package.as_deref() == Some(PACKAGE))
        .expect("gardenbed.protocol.v1 is in the descriptor set")
}

fn service() -> &'static ServiceDescriptorProto {
    file()
        .service
        .iter()
        .find(|s| s.name.as_deref() == Some(SERVICE))
        .expect("the Gardenbed service is in the descriptor set")
}

/// Resolves a (possibly fully-qualified) message name within the file.
fn message(name: &str) -> &'static DescriptorProto {
    let short = name.trim_start_matches('.').rsplit('.').next().unwrap();
    file()
        .message_type
        .iter()
        .find(|m| m.name.as_deref() == Some(short))
        .unwrap_or_else(|| panic!("message {short:?} is in the descriptor set"))
}

fn enum_type(name: &str) -> &'static EnumDescriptorProto {
    file()
        .enum_type
        .iter()
        .find(|e| e.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("enum {name:?} is in the descriptor set"))
}

fn field<'a>(msg: &'a DescriptorProto, name: &str) -> &'a FieldDescriptorProto {
    msg.field
        .iter()
        .find(|f| f.name.as_deref() == Some(name))
        .unwrap_or_else(|| {
            panic!(
                "field {}.{} is missing from the descriptor set",
                msg.name.as_deref().unwrap_or("?"),
                name
            )
        })
}

/// Unwraps a field's name (always present in this file).
fn fname(f: &FieldDescriptorProto) -> &str {
    f.name.as_deref().unwrap_or_default()
}

/// Asserts a singular field's number and type (proto3 singular = Optional).
fn assert_scalar(f: &FieldDescriptorProto, number: i32, ty: Type) {
    assert_eq!(f.number, Some(number), "field {}: numbering", fname(f));
    assert_eq!(f.r#type, Some(ty as i32), "field {}: type", fname(f));
    assert_eq!(
        f.label,
        Some(Label::Optional as i32),
        "field {}: label",
        fname(f)
    );
}

fn assert_repeated(f: &FieldDescriptorProto, number: i32, ty: Type) {
    assert_eq!(f.number, Some(number), "field {}: numbering", fname(f));
    assert_eq!(f.r#type, Some(ty as i32), "field {}: type", fname(f));
    assert_eq!(
        f.label,
        Some(Label::Repeated as i32),
        "field {}: label",
        fname(f)
    );
}

// -------------------------------------------------------------- service

#[test]
fn package_and_service_names() {
    assert_eq!(file().package.as_deref(), Some(PACKAGE));
    // protox records the file name relative to the include path, like protoc.
    assert_eq!(file().name.as_deref(), Some("gardenbed.proto"));
    assert_eq!(service().name.as_deref(), Some(SERVICE));
    assert_eq!(
        service().method.len(),
        RPCS.len(),
        "exactly the §5 RPCs, no more"
    );
}

#[test]
fn every_rpc_of_the_spec_is_present_with_streaming_flags() {
    let methods = &service().method;
    assert_eq!(methods.len(), RPCS.len(), "exactly the §5 RPCs, no more");
    for (m, (name, client_streaming, server_streaming)) in methods.iter().zip(RPCS.iter()) {
        assert_eq!(m.name.as_deref(), Some(*name), "RPC order matches §5");
        assert_eq!(
            m.client_streaming.unwrap_or(false),
            *client_streaming,
            "{name}"
        );
        assert_eq!(
            m.server_streaming.unwrap_or(false),
            *server_streaming,
            "{name}"
        );
        // No bidirectional RPCs.
        assert!(
            !(m.client_streaming.unwrap_or(false) && m.server_streaming.unwrap_or(false)),
            "{name} must not be bidirectional"
        );
        // Input and output types resolve within the package.
        assert!(
            m.input_type
                .as_deref()
                .unwrap_or_default()
                .starts_with(&format!(".{PACKAGE}.")),
            "{name} input"
        );
        assert!(
            m.output_type
                .as_deref()
                .unwrap_or_default()
                .starts_with(&format!(".{PACKAGE}.")),
            "{name} output"
        );
    }
}

#[test]
fn rpc_input_output_types_match_the_spec() {
    // (rpc, input, output)
    let types: &[(&str, &str, &str)] = &[
        ("RootTree", "GetChangesetReq", "RootTreeResp"),
        ("GetTree", "GetTreeReq", "TreeResp"),
        ("GetBlob", "GetBlobReq", "BlobChunk"),
        ("GetChangeset", "GetChangesetReq", "ChangesetMsg"),
        ("PutBlob", "PutBlobChunk", "PutBlobResp"),
        (
            "CreateChangeset",
            "CreateChangesetReq",
            "CreateChangesetResp",
        ),
        ("MergeBase", "MergeBaseReq", "MergeBaseResp"),
        ("IsAncestor", "IsAncestorReq", "IsAncestorResp"),
        ("Diff", "DiffReq", "ChangeMsg"),
        ("Log", "LogReq", "ChangesetSummary"),
        ("Blame", "BlameReq", "BlameRange"),
        ("ListRefs", "ListRefsReq", "RefEntry"),
        ("GetRef", "GetRefReq", "GetRefResp"),
        ("CasRef", "CasRefReq", "CasResp"),
        ("Rebase", "RebaseReq", "RebaseResp"),
        ("Publish", "PublishReq", "PublishResp"),
        ("Merge", "MergeReq", "MergeResp"),
        ("GetStats", "GetStatsReq", "StatsResp"),
    ];
    for m in service().method.iter() {
        let name = m.name.as_deref().unwrap_or_default();
        let (input, output) = types
            .iter()
            .find(|(rpc, _, _)| rpc == &m.name.as_deref().unwrap_or(""))
            .map(|(_, i, o)| (*i, *o))
            .unwrap_or_else(|| panic!("RPC {} is not in the §5 table", name));
        assert_eq!(
            m.input_type.as_deref(),
            Some(format!(".{PACKAGE}.{input}").as_str()),
            "{name} input"
        );
        assert_eq!(
            m.output_type.as_deref(),
            Some(format!(".{PACKAGE}.{output}").as_str()),
            "{name} output"
        );
    }
}

// ----------------------------------------------------------- repo field

#[test]
fn repo_is_field_one_on_every_request() {
    for (name, client_streaming, _) in RPCS {
        if *client_streaming {
            // Client-streaming PutBlob has no unary request message; the repo
            // rides on the leading header frame instead.
            let repo = field(message("PutBlobHeader"), "repo");
            assert_scalar(repo, 1, Type::String);
        } else {
            let m = service()
                .method
                .iter()
                .find(|m| m.name.as_deref() == Some(*name))
                .expect("RPC exists");
            let repo = field(message(m.input_type.as_deref().unwrap_or("")), "repo");
            assert_scalar(repo, 1, Type::String);
        }
    }
}

// ---------------------------------------------------------------- enums

#[test]
fn op_enum_mirrors_canonical_op_bytes() {
    let op = enum_type("Op");
    let values: Vec<(&str, i32)> = op
        .value
        .iter()
        .map(|v| {
            (
                v.name.as_deref().unwrap_or("?"),
                v.number.expect("value number"),
            )
        })
        .collect();
    assert_eq!(
        values,
        vec![
            ("OP_ADD", 0),
            ("OP_MODIFY", 1),
            ("OP_DELETE", 2),
            ("OP_RENAME", 3)
        ]
    );
}

#[test]
fn mode_enum_mirrors_canonical_mode_bytes() {
    let mode = enum_type("Mode");
    let values: Vec<(&str, i32)> = mode
        .value
        .iter()
        .map(|v| {
            (
                v.name.as_deref().unwrap_or("?"),
                v.number.expect("value number"),
            )
        })
        .collect();
    assert_eq!(
        values,
        vec![
            ("MODE_REGULAR", 0),
            ("MODE_EXEC", 1),
            ("MODE_SYMLINK", 2),
            ("MODE_GITLINK", 3),
        ]
    );
}

// -------------------------------------------------------- message shape

#[test]
fn change_msg_carries_ids_only_never_content() {
    let msg = message("ChangeMsg");
    // The declared fields, exactly.
    let names: Vec<&str> = msg.field.iter().map(fname).collect();
    assert_eq!(names, vec!["op", "path", "blob_id", "src_path", "mode"]);
    // No field is even *named* like content.
    assert!(names.iter().all(|n| !n.contains("content")));
    // The only bytes-typed field is the 32-byte blob id.
    assert!(
        msg.field
            .iter()
            .all(|f| f.r#type != Some(Type::Bytes as i32) || fname(f) == "blob_id")
    );
    // op and mode are enums; blob_id is raw content-address material.
    assert_eq!(field(msg, "op").r#type, Some(Type::Enum as i32));
    assert_eq!(field(msg, "mode").r#type, Some(Type::Enum as i32));
    let blob_id = field(msg, "blob_id");
    assert_eq!(blob_id.number, Some(3));
    assert_eq!(blob_id.r#type, Some(Type::Bytes as i32));
    assert_eq!(
        blob_id.proto3_optional,
        Some(true),
        "blob_id is proto3 optional"
    );
}

#[test]
fn changeset_msg_mirrors_the_model_field_for_field() {
    let msg = message("ChangesetMsg");
    let names: Vec<&str> = msg.field.iter().map(fname).collect();
    assert_eq!(
        names,
        vec![
            "parents",
            "changes",
            "message",
            "author",
            "committer",
            "signature"
        ]
    );
    assert_repeated(field(msg, "parents"), 1, Type::Bytes);
    let changes = field(msg, "changes");
    assert_eq!(changes.number, Some(2));
    assert_eq!(changes.r#type, Some(Type::Message as i32));
    assert_eq!(changes.label, Some(Label::Repeated as i32));
    assert_eq!(
        changes.type_name.as_deref(),
        Some(format!(".{PACKAGE}.ChangeMsg").as_str())
    );
    assert_eq!(
        field(msg, "author").type_name.as_deref(),
        Some(format!(".{PACKAGE}.IdentityMsg").as_str())
    );
    assert_eq!(
        field(msg, "committer").type_name.as_deref(),
        Some(format!(".{PACKAGE}.IdentityMsg").as_str())
    );
    let sig = field(msg, "signature");
    assert_eq!(sig.number, Some(6));
    assert_eq!(sig.r#type, Some(Type::Bytes as i32));
    assert_eq!(sig.proto3_optional, Some(true));
    // Blob content never rides on a changeset either: the only bytes fields
    // are parent ids and the signature.
    assert!(
        msg.field
            .iter()
            .all(|f| f.r#type != Some(Type::Bytes as i32)
                || fname(f) == "parents"
                || fname(f) == "signature")
    );
}

#[test]
fn identity_msg_preserves_git_native_timestamps() {
    let msg = message("IdentityMsg");
    assert_scalar(field(msg, "time_secs"), 3, Type::Uint64);
    // sint32: signed, zigzag-encoded, so negative tz offsets round-trip.
    assert_scalar(field(msg, "tz_min"), 4, Type::Sint32);
}

#[test]
fn tree_entry_uses_a_real_oneof_for_blob_or_tree() {
    let msg = message("TreeEntry");
    let oneof_index = msg
        .oneof_decl
        .iter()
        .position(|d| d.name.as_deref() == Some("id"))
        .expect("TreeEntry declares the `id` oneof");
    let blob_id = field(msg, "blob_id");
    assert_eq!(blob_id.number, Some(3));
    assert_eq!(blob_id.r#type, Some(Type::Bytes as i32));
    assert_eq!(blob_id.oneof_index, Some(oneof_index as i32));
    let tree_id = field(msg, "tree_id");
    assert_eq!(tree_id.number, Some(4));
    assert_eq!(tree_id.r#type, Some(Type::Bytes as i32));
    assert_eq!(tree_id.oneof_index, Some(oneof_index as i32));
}

#[test]
fn put_blob_frames_carry_header_then_data() {
    let chunk = message("PutBlobChunk");
    let oneof_index = chunk
        .oneof_decl
        .iter()
        .position(|d| d.name.as_deref() == Some("frame"))
        .expect("PutBlobChunk declares the `frame` oneof");
    let header = field(chunk, "header");
    assert_eq!(header.oneof_index, Some(oneof_index as i32));
    assert_eq!(header.r#type, Some(Type::Message as i32));
    assert_eq!(
        header.type_name.as_deref(),
        Some(format!(".{PACKAGE}.PutBlobHeader").as_str())
    );
    let data = field(chunk, "data");
    assert_eq!(data.oneof_index, Some(oneof_index as i32));
    assert_eq!(data.r#type, Some(Type::Bytes as i32));
    // And the header frame names the repo.
    let repo = field(message("PutBlobHeader"), "repo");
    assert_scalar(repo, 1, Type::String);
}

#[test]
fn cas_resp_uses_a_real_oneof_for_won_or_conflict() {
    let msg = message("CasResp");
    let oneof_index = msg
        .oneof_decl
        .iter()
        .position(|d| d.name.as_deref() == Some("result"))
        .expect("CasResp declares the `result` oneof");
    let won = field(msg, "won");
    assert_eq!(won.number, Some(1));
    assert_eq!(won.r#type, Some(Type::Bool as i32));
    assert_eq!(won.oneof_index, Some(oneof_index as i32));
    let conflict = field(msg, "conflict_current");
    assert_eq!(conflict.number, Some(2));
    assert_eq!(conflict.r#type, Some(Type::Bytes as i32));
    assert_eq!(conflict.oneof_index, Some(oneof_index as i32));
}

#[test]
fn responses_carry_ids_never_blob_content() {
    // Diff streams ChangeMsg and Log streams ChangesetSummary: ids only,
    // never blob content (v0.1.md §5).
    for name in ["ChangeMsg", "ChangesetSummary"] {
        let msg = message(name);
        assert!(
            msg.field
                .iter()
                .all(|f| f.r#type != Some(Type::Bytes as i32)
                    || fname(f) == "cs_id"
                    || fname(f) == "parents"
                    || fname(f) == "signature"
                    || fname(f) == "blob_id"),
            "{name} must not carry blob content",
        );
        assert!(msg.field.iter().all(|f| !fname(f).contains("content")));
    }
}

#[test]
fn log_blame_and_changeset_requests_name_the_changeset_cs_id() {
    for name in ["LogReq", "BlameReq", "GetChangesetReq"] {
        let cs = field(message(name), "cs_id");
        assert_eq!(cs.number, Some(2), "{name}: cs_id is field #2 after repo");
        assert_eq!(cs.r#type, Some(Type::Bytes as i32));
    }
}
