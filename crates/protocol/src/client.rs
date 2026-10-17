//! A thin, typed wrapper over the generated tonic client
//! ([`pb::gardenbed_client::GardenbedClient`]).
//!
//! The wrapper keeps wire types off the unary paths: ids go in and out as
//! [`gardenbed_model::Id`], changesets and trees as model structs, and
//! malformed server replies surface as [`tonic::Status::data_loss`]
//! (locally authored model data that cannot be converted surfaces as
//! [`tonic::Status::invalid_argument`]). Streaming RPCs hand back the raw
//! `tonic::Streaming` of wire messages (blob chunks, log summaries, …),
//! which callers consume item by item.
//!
//! [`GardenbedClient::connect`] understands `http(s)://host:port` (TCP) and
//! `unix:///path/to/socket` endpoints; tonic 0.14's transport resolves both
//! schemes natively.

use std::collections::BTreeMap;

use gardenbed_model as model;
use tokio_stream::{Stream, StreamExt};
use tonic::Status;
use tonic::transport::{Channel, Endpoint, Uri};

use crate::convert::{ConversionError, id_from_bytes, id_to_blob_key, id_to_bytes};
use crate::pb;

/// Errors from [`GardenbedClient::connect`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectError {
    /// The endpoint's scheme is neither `http`/`https` (TCP) nor `unix`.
    #[error(
        "unsupported endpoint {0:?}: expected http(s)://host:port or \
         unix:///path/to/socket"
    )]
    UnsupportedScheme(String),
    /// The endpoint was rejected (bad URI) or the connection failed. Unix
    /// socket connect failures (missing path, permission denied, …) surface
    /// here too.
    #[error(transparent)]
    Transport(#[from] tonic::transport::Error),
}

/// Outcome of a compare-and-swap ref update
/// ([`GardenbedClient::cas_ref`]). Conflicts are results, not errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CasOutcome {
    /// The swap won: the ref now points at the requested value.
    Won,
    /// The swap lost: the ref still points at `current`.
    Conflict {
        /// The ref's current value, which was not the `expected` one.
        current: model::Id,
    },
}

/// A thin typed client for the Gardenbed gRPC API (v0.1.md §5).
///
/// Cheap to clone and share: every method takes `&self` and clones the
/// underlying channel handle.
#[derive(Debug, Clone)]
pub struct GardenbedClient {
    inner: pb::gardenbed_client::GardenbedClient<Channel>,
}

impl GardenbedClient {
    /// Connects to a gardenbed server.
    ///
    /// * `http://host:port` / `https://host:port` — TCP.
    /// * `unix:///path/to/socket` — unix domain socket.
    ///
    /// Anything else fails with [`ConnectError::UnsupportedScheme`].
    pub async fn connect(endpoint: &str) -> Result<Self, ConnectError> {
        // Reject unsupported schemes up front with a clear error instead of
        // tonic's opaque invalid-URI error.
        let uri: Uri = endpoint
            .parse()
            .map_err(|_| ConnectError::UnsupportedScheme(endpoint.to_owned()))?;
        match uri.scheme_str() {
            Some("http" | "https" | "unix") => {}
            _ => return Err(ConnectError::UnsupportedScheme(endpoint.to_owned())),
        }
        // tonic 0.14 handles both transports natively: `Endpoint::from_shared`
        // parses `unix:` prefixes into a UDS connector, everything else goes
        // through the HTTP connector.
        let channel = Endpoint::from_shared(endpoint.to_owned())?
            .connect()
            .await?;
        Ok(Self::from_channel(channel))
    }

    /// Wraps an already-established channel (e.g. from
    /// `Endpoint::connect_lazy`, a custom connector, or a shared pool).
    pub fn from_channel(channel: Channel) -> Self {
        Self {
            inner: pb::gardenbed_client::GardenbedClient::new(channel),
        }
    }

    /// The underlying generated tonic client, for callers that need
    /// compression settings, interceptors, or server-streaming plumbing this
    /// wrapper does not expose.
    pub fn raw(&self) -> &pb::gardenbed_client::GardenbedClient<Channel> {
        &self.inner
    }

    /// Consumes the wrapper, returning the underlying generated tonic client.
    pub fn into_raw(self) -> pb::gardenbed_client::GardenbedClient<Channel> {
        self.inner
    }

    // ----------------------------------------------------- object access

    /// Id of the root tree of a changeset; `None` for an empty root.
    pub async fn root_tree(
        &self,
        repo: &str,
        cs_id: model::Id,
    ) -> Result<Option<model::Id>, Status> {
        let req = pb::GetChangesetReq {
            repo: repo.to_owned(),
            cs_id: id_to_bytes(cs_id),
        };
        let mut inner = self.inner.clone();
        let resp = inner.root_tree(req).await?.into_inner();
        resp.tree_id
            .map(|b| wire_id(&b, "RootTreeResp.tree_id"))
            .transpose()
    }

    /// Fetch a tree's entries by id (want-list recursion uses this).
    pub async fn get_tree(&self, repo: &str, id: model::Id) -> Result<model::Tree, Status> {
        let req = pb::GetTreeReq {
            repo: repo.to_owned(),
            id: id_to_bytes(id),
        };
        let mut inner = self.inner.clone();
        let resp = inner.get_tree(req).await?.into_inner();
        wire_to_model(model::Tree::try_from(resp), "TreeResp")
    }

    /// Stream one blob's raw (uncompressed) content, chunk by chunk.
    pub async fn get_blob(
        &self,
        repo: &str,
        id: model::Id,
    ) -> Result<tonic::Streaming<pb::BlobChunk>, Status> {
        let req = pb::GetBlobReq {
            repo: repo.to_owned(),
            id: id_to_bytes(id),
        };
        let mut inner = self.inner.clone();
        Ok(inner.get_blob(req).await?.into_inner())
    }

    /// Fetch one changeset.
    pub async fn get_changeset(
        &self,
        repo: &str,
        cs_id: model::Id,
    ) -> Result<model::Changeset, Status> {
        let req = pb::GetChangesetReq {
            repo: repo.to_owned(),
            cs_id: id_to_bytes(cs_id),
        };
        let mut inner = self.inner.clone();
        let resp = inner.get_changeset(req).await?.into_inner();
        wire_to_model(model::Changeset::try_from(resp), "ChangesetMsg")
    }

    // ------------------------------------------------------- blob upload

    /// Upload one blob body, streamed; returns its content address.
    ///
    /// The repo rides on a leading header frame (client-streaming has no
    /// unary request message), then every body chunk is forwarded in order.
    /// Used for blobs > 1 MiB; smaller ones ride on
    /// [`GardenbedClient::create_changeset`].
    pub async fn put_blob<S, B>(&self, repo: &str, body: S) -> Result<model::Id, Status>
    where
        S: Stream<Item = B> + Send + 'static,
        B: AsRef<[u8]>,
    {
        let header = pb::PutBlobChunk {
            frame: Some(pb::put_blob_chunk::Frame::Header(pb::PutBlobHeader {
                repo: repo.to_owned(),
            })),
        };
        let chunks = body.map(|chunk| pb::PutBlobChunk {
            frame: Some(pb::put_blob_chunk::Frame::Data(chunk.as_ref().to_vec())),
        });
        let frames = tokio_stream::once(header).chain(chunks);
        let mut inner = self.inner.clone();
        let resp = inner.put_blob(frames).await?.into_inner();
        wire_id(&resp.id, "PutBlobResp.id")
    }

    // ------------------------------------------------------ change graph

    /// Commits a changeset (with ≤ 1 MiB blobs inline); returns its id.
    ///
    /// `inline_blobs` pairs each referenced blob id with the raw content;
    /// every pairing is checked locally (`blob_id(content) == id`) before
    /// anything is sent, since a mismatch would poison the repo's object
    /// store.
    pub async fn create_changeset(
        &self,
        repo: &str,
        cs: &model::Changeset,
        inline_blobs: impl IntoIterator<Item = (model::Id, Vec<u8>)>,
    ) -> Result<model::Id, Status> {
        let mut inline = std::collections::HashMap::new();
        for (id, content) in inline_blobs {
            if model::blob_id(&content) != id {
                return Err(Status::invalid_argument(format!(
                    "inline blob content does not hash to its id {}",
                    id
                )));
            }
            inline.insert(id_to_blob_key(id), content);
        }
        let msg = pb::ChangesetMsg::try_from(cs)
            .map_err(|e| Status::invalid_argument(format!("changeset is not valid: {e}")))?;
        let req = pb::CreateChangesetReq {
            repo: repo.to_owned(),
            cs: Some(msg),
            inline_blobs: inline,
        };
        let mut inner = self.inner.clone();
        let resp = inner.create_changeset(req).await?.into_inner();
        wire_id(&resp.id, "CreateChangesetResp.id")
    }

    /// Merge base of two changesets; `None` when they share none.
    pub async fn merge_base(
        &self,
        repo: &str,
        a: model::Id,
        b: model::Id,
    ) -> Result<Option<model::Id>, Status> {
        let req = pb::MergeBaseReq {
            repo: repo.to_owned(),
            a: id_to_bytes(a),
            b: id_to_bytes(b),
        };
        let mut inner = self.inner.clone();
        let resp = inner.merge_base(req).await?.into_inner();
        resp.id.map(|b| wire_id(&b, "MergeBaseResp.id")).transpose()
    }

    /// Is `a` an ancestor of `b`?
    pub async fn is_ancestor(
        &self,
        repo: &str,
        a: model::Id,
        b: model::Id,
    ) -> Result<bool, Status> {
        let req = pb::IsAncestorReq {
            repo: repo.to_owned(),
            a: id_to_bytes(a),
            b: id_to_bytes(b),
        };
        let mut inner = self.inner.clone();
        Ok(inner.is_ancestor(req).await?.into_inner().is_ancestor)
    }

    /// Stream the change list between the trees of `a` and `b` (a -> b).
    /// Ids only — never blob content.
    pub async fn diff(
        &self,
        repo: &str,
        a: model::Id,
        b: model::Id,
    ) -> Result<tonic::Streaming<pb::ChangeMsg>, Status> {
        let req = pb::DiffReq {
            repo: repo.to_owned(),
            a: id_to_bytes(a),
            b: id_to_bytes(b),
        };
        let mut inner = self.inner.clone();
        Ok(inner.diff(req).await?.into_inner())
    }

    /// Log from `cs_id`, newest first, at most `limit` entries (0 =
    /// unlimited). Date-ordered over all parent lineages, like `git log`'s
    /// default walk.
    pub async fn log(
        &self,
        repo: &str,
        cs_id: model::Id,
        limit: u32,
    ) -> Result<tonic::Streaming<pb::ChangesetSummary>, Status> {
        let req = pb::LogReq {
            repo: repo.to_owned(),
            cs_id: id_to_bytes(cs_id),
            limit,
        };
        let mut inner = self.inner.clone();
        Ok(inner.log(req).await?.into_inner())
    }

    /// First-parent lineage blame of one path at `cs_id`, with rename
    /// following (v0.1.md D7).
    pub async fn blame(
        &self,
        repo: &str,
        cs_id: model::Id,
        path: &str,
    ) -> Result<tonic::Streaming<pb::BlameRange>, Status> {
        let req = pb::BlameReq {
            repo: repo.to_owned(),
            cs_id: id_to_bytes(cs_id),
            path: path.to_owned(),
        };
        let mut inner = self.inner.clone();
        Ok(inner.blame(req).await?.into_inner())
    }

    // ------------------------------------------------------------- refs

    /// Stream all refs of the repo.
    pub async fn list_refs(&self, repo: &str) -> Result<tonic::Streaming<pb::RefEntry>, Status> {
        let req = pb::ListRefsReq {
            repo: repo.to_owned(),
        };
        let mut inner = self.inner.clone();
        Ok(inner.list_refs(req).await?.into_inner())
    }

    /// One ref's value; `None` when the ref does not exist.
    pub async fn get_ref(&self, repo: &str, name: &str) -> Result<Option<model::Id>, Status> {
        let req = pb::GetRefReq {
            repo: repo.to_owned(),
            name: name.to_owned(),
        };
        let mut inner = self.inner.clone();
        let resp = inner.get_ref(req).await?.into_inner();
        resp.cs_id
            .map(|b| wire_id(&b, "GetRefResp.cs_id"))
            .transpose()
    }

    /// Compare-and-swap one ref. `expected` absent means the ref must not
    /// exist yet (create). Conflicts come back as [`CasOutcome::Conflict`],
    /// never as errors.
    pub async fn cas_ref(
        &self,
        repo: &str,
        name: &str,
        expected: Option<model::Id>,
        new: model::Id,
    ) -> Result<CasOutcome, Status> {
        let req = pb::CasRefReq {
            repo: repo.to_owned(),
            name: name.to_owned(),
            expected: expected.map(id_to_bytes),
            new: id_to_bytes(new),
        };
        let mut inner = self.inner.clone();
        let resp = inner.cas_ref(req).await?.into_inner();
        match resp.result {
            Some(pb::cas_resp::Result::Won(true)) => Ok(CasOutcome::Won),
            Some(pb::cas_resp::Result::Won(false)) => {
                // A conforming server answers conflicts with
                // `conflict_current`, never with `won = false`.
                Err(malformed_response("CasResp.won = false"))
            }
            Some(pb::cas_resp::Result::ConflictCurrent(b)) => {
                let current = wire_id(&b, "CasResp.conflict_current")?;
                Ok(CasOutcome::Conflict { current })
            }
            None => Err(malformed_response("CasResp carries no result")),
        }
    }

    /// Replay a changeset's changes onto a new parent (linearized rebase);
    /// returns the id of the rebased changeset. Semantic conflicts abort
    /// with `FAILED_PRECONDITION` (conflicting paths in the status message).
    pub async fn rebase(
        &self,
        repo: &str,
        cs_id: model::Id,
        new_parent: model::Id,
    ) -> Result<model::Id, Status> {
        let req = pb::RebaseReq {
            repo: repo.to_owned(),
            cs_id: id_to_bytes(cs_id),
            new_parent: id_to_bytes(new_parent),
        };
        let mut inner = self.inner.clone();
        let resp = inner.rebase(req).await?.into_inner();
        wire_id(&resp.id, "RebaseResp.id")
    }

    /// Fast-forwards a ref to `cs_id` and returns the changeset the ref now
    /// points at (v0.1.md D5).
    ///
    /// `ref_name` selects the ref (e.g. `"feature"`); `None` means `"main"`,
    /// exactly the v0.1 request shape (the wire field is left unset).
    /// `force` permits a non-descendant move of a non-main ref — the
    /// deliberate rewind escape hatch.
    pub async fn publish(
        &self,
        repo: &str,
        cs_id: model::Id,
        ref_name: Option<&str>,
        force: bool,
    ) -> Result<model::Id, Status> {
        let req = pb::PublishReq {
            repo: repo.to_owned(),
            cs_id: id_to_bytes(cs_id),
            r#ref: ref_name.unwrap_or_default().to_owned(),
            force,
        };
        let mut inner = self.inner.clone();
        let resp = inner.publish(req).await?.into_inner();
        wire_id(&resp.id, "PublishResp.id")
    }

    /// Server-side two-parent merge of `target` into `cs_id`
    /// (v0.2.md D9-D11): one changeset `parents = [cs_id, target]` whose
    /// changes are the diff `tree(cs_id) -> merged tree`. Conflicts abort
    /// with `FAILED_PRECONDITION` listing every conflicting path. When
    /// `target` is already an ancestor of `cs_id`, answers `up_to_date`
    /// with `id = cs_id` and nothing is stored. Returns
    /// `(id, up_to_date)`.
    pub async fn merge(
        &self,
        repo: &str,
        cs_id: model::Id,
        target: model::Id,
        message: &str,
        author: &model::Identity,
        committer: &model::Identity,
    ) -> Result<(model::Id, bool), Status> {
        let req = pb::MergeReq {
            repo: repo.to_owned(),
            cs_id: id_to_bytes(cs_id),
            target: id_to_bytes(target),
            message: message.to_owned(),
            author: Some(pb::IdentityMsg::from(author)),
            committer: Some(pb::IdentityMsg::from(committer)),
        };
        let mut inner = self.inner.clone();
        let resp = inner.merge(req).await?.into_inner();
        Ok((wire_id(&resp.id, "MergeResp.id")?, resp.up_to_date))
    }

    /// Per-repo request counters (debug/test infrastructure).
    pub async fn get_stats(&self, repo: &str) -> Result<BTreeMap<String, u64>, Status> {
        let req = pb::GetStatsReq {
            repo: repo.to_owned(),
        };
        let mut inner = self.inner.clone();
        let resp = inner.get_stats(req).await?.into_inner();
        Ok(resp.counters.into_iter().collect())
    }
}

/// Converts a raw id off the wire, mapping a bad length to a data-loss
/// status (the server sent something that is not a content address).
fn wire_id(bytes: &[u8], what: &'static str) -> Result<model::Id, Status> {
    id_from_bytes(bytes).map_err(|_| malformed_response(what))
}

/// Maps a wire→model conversion failure to a data-loss status: a conforming
/// server never sends shapes the model cannot represent.
fn wire_to_model<T>(
    converted: Result<T, ConversionError>,
    what: &'static str,
) -> Result<T, Status> {
    converted.map_err(|e| Status::data_loss(format!("malformed {what} from server: {e}")))
}

fn malformed_response(what: &'static str) -> Status {
    Status::data_loss(format!("malformed response from server: {what}"))
}
