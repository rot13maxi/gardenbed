//! Concurrent first-open schema initialization for the Postgres
//! metadata store (the 23505 race the CI agent found: on a fresh
//! database, parallel `PostgresMetadataStore::open()` calls race
//! `CREATE TABLE IF NOT EXISTS` and one loses with a duplicate-key
//! error on `pg_type_typname_nsp_index`).
//!
//! Gating: these tests need a live Postgres and are skipped (with a
//! printed note) unless `GARDENBED_TEST_PG_URL` is set — same contract
//! as `tests/parity.rs`. The compose fixture
//! (`tools/scale/compose.yaml`) provides it on 55432.
//!
//! Isolation: each test runs against a SCHEMA it creates in the live
//! database and drops at the end — never `public`, never the shared
//! tables the parity suite uses. Store URLs point at that schema via
//! `?options=-csearch_path%3D<schema>` (tokio-postgres URL query
//! parameter), so unqualified DDL in `SCHEMA`/`MIGRATIONS` lands inside
//! it. Every schema name carries a per-run tag, so repeated runs and
//! concurrent runs against one database coexist. Cleanup drops ONLY the
//! schemas this file creates (name prefix `gbs_` + per-run tag).
//!

// The scale backends; meaningless in a --no-default-features build.
#![cfg(feature = "scale")]

use std::future::Future;
use std::process::Command;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::LazyLock;
use std::thread;

use gardenbed_store::PostgresMetadataStore;

/// Live-service Postgres URL, or `None` (skip) when unset.
fn pg_url() -> Option<String> {
    std::env::var("GARDENBED_TEST_PG_URL").ok()
}

/// Per-run tag: unique schema names so repeated runs against one
/// database coexist (same convention as `tests/parity.rs`'s ref tag).
fn run_tag() -> String {
    format!("{:x}", std::process::id() as u64 ^ nanos_salt())
}

/// Sub-nanos since the epoch, so two runs in one second still differ.
fn nanos_salt() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// A raw connection to the live database (test-side scaffolding only —
/// it never touches the store's tables; it only creates/drops ISOLATED
/// test schemas and reads the catalog to prove emptiness).
fn admin_conn(url: &str) -> tokio_postgres::Client {
    let url = url.to_owned();
    let url2 = url.clone();
    let (client, connection) =
        block(async move { tokio_postgres::connect(&url2, tokio_postgres::NoTls).await })
            .unwrap_or_else(|e| panic!("admin connection to {url}: {e}"));
    // Drive the connection stream on the SAME persistent runtime the
    // client's calls run on (a per-call throwaway runtime drops the
    // reactor — and with it the socket — the moment the call returns).
    DRIVER.spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("admin connection driver ended: {e}");
        }
    });
    client
}

/// The shared driver thread for all test-side async: a private
/// single-threaded runtime whose handle outlives every call — the same
/// pattern as the store's own `driver.rs`, so a client's socket is
/// never orphaned on a dropped reactor.
static DRIVER: LazyLock<tokio::runtime::Handle> = LazyLock::new(|| {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::Builder::new()
        .name("pg-schema-test-driver".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test driver runtime builds");
            let _ = tx.send(runtime.handle().clone());
            // Park forever; tasks arrive through the handle.
            runtime.block_on(std::future::pending::<()>());
        })
        .expect("test driver thread spawns");
    rx.recv().expect("test driver publishes its handle")
});

/// Blocks on one client call on the DRIVER runtime. The future may
/// borrow locals: it is awaited inline inside a spawned driver task —
/// the driver's executor keeps polling until the send completes, so
/// the borrows live as long as the `recv` below (the same lifetime
/// bridge `block_on` would give, without a second reactor owning the
/// client's socket).
fn block<T>(fut: impl Future<Output = T> + Send + 'static) -> T
where
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    DRIVER.spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.recv().expect("test driver delivers result")
}

/// Percent-encodes exactly the characters a search_path option needs:
/// the value sits inside a URL query parameter, so `=` must not split
/// it, and the schema name must survive verbatim.
fn encode_options_value(schema: &str) -> String {
    schema
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32 as u8),
        })
        .collect()
}

/// A store URL identical to the live one except that unqualified names
/// resolve inside `schema` (created empty by the caller).
fn url_for_schema(base: &str, schema: &str) -> String {
    let sep = if base.contains('?') { '&' } else { '?' };
    format!(
        "{base}{sep}options=-csearch_path%3D{}",
        encode_options_value(schema)
    )
}

/// A fresh, provably-empty schema in the live database; the guard
/// returns its name and drops it when done. Emphasis: `public` is never
/// touched — the parity suite and any shared fixtures live there.
fn fresh_schema(base: &str, tag: &str) -> SchemaGuard {
    let schema = format!("gbs_{tag}");
    let client = admin_conn(base);
    let probe = schema.clone();
    let exists: bool = block(async move {
        client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname = $1)",
                &[&probe],
            )
            .await
    })
    .expect("namespace probe")
    .get(0);
    assert!(
        !exists,
        "schema {schema} already exists — per-run tag collision"
    );
    let client = admin_conn(base);
    let ddl = format!(
        // AUTHORIZATION keeps CREATE inside the schema grant-free even
        // when the test role differs from the fixture owner.
        "CREATE SCHEMA \"{schema}\" AUTHORIZATION current_user"
    );
    block(async move { client.batch_execute(&ddl).await }).expect("create schema");
    let client = admin_conn(base);
    // Provable emptiness: no relations of any kind before the race.
    let count_sql = format!(
        "SELECT count(*) FROM pg_class c JOIN pg_namespace n
           ON c.relnamespace = n.oid
          WHERE n.nspname = '{schema}'"
    );
    let count: i64 = block(async move {
        client
            .query_one(&count_sql, &[])
            .await
            .expect("relation count")
            .get(0)
    });
    assert_eq!(count, 0, "fresh schema {schema} must start empty");
    SchemaGuard {
        url: base.to_owned(),
        schema,
    }
}

/// Drops the schema it owns (and nothing else) on scope exit.
struct SchemaGuard {
    url: String,
    schema: String,
}

impl Drop for SchemaGuard {
    fn drop(&mut self) {
        let client = admin_conn(&self.url);
        // CASCADE: the store created tables inside; the drop removes
        // exactly this test's objects. A panic mid-test still cleans up.
        let schema = self.schema.clone();
        let res = block(async move {
            client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
                .await
        });
        if let Err(e) = res {
            panic!(
                "cleanup failed for schema {} — drop it manually if \
                 this repeats: {e}",
                self.schema
            );
        }
    }
}

/// Verifies the store's tables exist with the expected shapes inside
/// `schema` — the DDL outcome every racing opener must converge to.
fn assert_schema_complete(base: &str, schema: &str) {
    let client = admin_conn(base);
    let probe = schema.to_owned();
    let rows = block(async move {
        client
            .query(
                "SELECT c.relname FROM pg_class c JOIN pg_namespace n
                   ON c.relnamespace = n.oid
                  WHERE n.nspname = $1 AND c.relkind = 'r'
                  ORDER BY c.relname",
                &[&probe],
            )
            .await
            .expect("table listing")
    });
    let tables: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
    let expected = [
        "changesets",
        "derived",
        "git_map",
        "git_raw_commit",
        "git_raw_tree",
        "refs",
        "repo_incarnation",
        "validated_changesets_v1",
    ];
    assert_eq!(tables, expected, "schema {schema} table set");
    // `MIGRATIONS` applied: `changesets.arrived` exists as a column.
    let client = admin_conn(base);
    let probe = schema.to_owned();
    let has_arrived: bool = block(async move {
        client
            .query_one(
                "SELECT EXISTS(
                    SELECT 1 FROM information_schema.columns
                     WHERE table_schema = $1
                       AND table_name = 'changesets'
                       AND column_name = 'arrived')",
                &[&probe],
            )
            .await
            .expect("column probe")
            .get(0)
    });
    assert!(has_arrived, "migration arrived column in {schema}");
}

/// How many racing openers to use: enough to reproduce the original
/// 6/7-failure shape many times over, small enough to stay a unit test.
const OPENERS: usize = 16;

#[test]
fn concurrent_first_open_on_fresh_schema() {
    let Some(base) = pg_url() else {
        eprintln!(
            "skipping postgres schema tests: GARDENBED_TEST_PG_URL is \
             not set (see this file's header)"
        );
        return;
    };
    let guard = fresh_schema(&base, &run_tag());
    let url = url_for_schema(&base, &guard.schema);

    // INDEPENDENT handles, each its own pool and driver thread, exactly
    // like separate replicas (or separate test processes) racing the
    // same fresh database. Every opener must succeed; with the
    // advisory lock reverted this fails with the production 23505.
    let barrier = Arc::new(Barrier::new(OPENERS));
    let handles: Vec<_> = (0..OPENERS)
        .map(|_| {
            let url = url.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                PostgresMetadataStore::open(&url)
            })
        })
        .collect();
    for (i, h) in handles.into_iter().enumerate() {
        let store = h
            .join()
            .expect("opener thread")
            .unwrap_or_else(|e| panic!("opener {i} failed to open the fresh schema: {e:?}"));
        // The store must be immediately usable: a trivial read through
        // the same pool proves the handle is not just connected but
        // functional against the schema the race just created.
        use gardenbed_store::MetadataStore;
        assert_eq!(
            store
                .get_changeset(gardenbed_model::Id::from_bytes([0u8; 32]))
                .expect("read through raced handle"),
            None
        );
    }

    // Exactly one schema cut: idempotent DDL under the lock leaves the
    // six tables (and the migration column) — never a duplicate.
    assert_schema_complete(&base, &guard.schema);
}

/// Repeated fresh-schema cycles: drop, recreate, race again. A lock
/// that leaked across opens (session-level, or never released) would
/// serialize or deadlock later rounds; transaction scope must not.
#[test]
fn repeated_fresh_schema_cycles_stay_independent() {
    let Some(base) = pg_url() else {
        eprintln!("skipping postgres schema tests: GARDENBED_TEST_PG_URL is not set");
        return;
    };
    for round in 0..3 {
        let guard = fresh_schema(&base, &format!("{}r{round}", run_tag()));
        let url = url_for_schema(&base, &guard.schema);
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let url = url.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    PostgresMetadataStore::open(&url)
                })
            })
            .collect();
        for (i, h) in handles.into_iter().enumerate() {
            h.join()
                .expect("opener thread")
                .unwrap_or_else(|e| panic!("round {round} opener {i}: {e:?}"));
        }
        assert_schema_complete(&base, &guard.schema);
        // Guard drops the schema before the next round recreates it.
    }
}

/// The two-process variant: schema DDL serialization must hold across
/// PROCESS boundaries (the production shape — separate
/// `gardenbed-server` replicas starting against one fresh database).
/// Child processes run this same binary with `--exact` filtering (the
/// `gblo2.rs` child-entry pattern), each opening its own store handle.
#[test]
fn concurrent_first_open_across_processes() {
    let Some(base) = pg_url() else {
        eprintln!("skipping postgres schema tests: GARDENBED_TEST_PG_URL is not set");
        return;
    };
    let guard = fresh_schema(&base, &run_tag());
    let url = url_for_schema(&base, &guard.schema);

    const CHILDREN: usize = 6;
    let mut children = Vec::new();
    for i in 0..CHILDREN {
        let exe = std::env::current_exe().expect("test binary");
        children.push(
            Command::new(exe)
                .arg("schema_two_process_child_open")
                .arg("--exact")
                .arg("--nocapture")
                .env("GARDENBED_CHILD_PG_URL", &url)
                .env("GARDENBED_CHILD_INDEX", format!("{i}"))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| panic!("child {i} spawn: {e}")),
        );
    }
    for (i, child) in children.into_iter().enumerate() {
        let out = child
            .wait_with_output()
            .unwrap_or_else(|e| panic!("child {i} wait: {e}"));
        assert!(
            out.status.success(),
            "child {i} failed ({}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("CHILD_OPEN_OK"),
            "child {i} did not report success; output:\n{stdout}"
        );
    }

    assert_schema_complete(&base, &guard.schema);
}

/// Child entry: opens one store against the schema the parent created
/// and prints the success marker. Driven only by
/// `concurrent_first_open_across_processes` (same shape as
/// `fastcdc_table_child_entry`).
#[test]
fn schema_two_process_child_open() {
    let url = match std::env::var("GARDENBED_CHILD_PG_URL") {
        Ok(u) => u,
        Err(_) => {
            // Run directly (not via the parent): nothing to race; say so
            // instead of failing (the gblo2 child panics, but its parent
            // always sets the env; here a bare run is reachable via
            // --ignored-style sweeps, so we no-op explicitly).
            eprintln!("child entry: GARDENBED_CHILD_PG_URL not set; nothing to do");
            return;
        }
    };
    let store = PostgresMetadataStore::open(&url)
        .unwrap_or_else(|e| panic!("child open against fresh schema: {e:?}"));
    use gardenbed_store::MetadataStore;
    assert_eq!(
        store
            .get_changeset(gardenbed_model::Id::from_bytes([0u8; 32]))
            .expect("read through child handle"),
        None
    );
    println!(
        "CHILD_OPEN_OK {}",
        std::env::var("GARDENBED_CHILD_INDEX").unwrap_or_default()
    );
}

/// The repository incarnation: minted atomically on first use, stable
/// across independent handles (pools, threads, processes), and fresh
/// for a new schema/database. These are the identity properties the
/// daemon's cache scoping relies on (see
/// `MetadataStore::repo_incarnation`).
#[test]
fn repo_incarnation_is_atomic_and_stable_across_handles() {
    let Some(base) = pg_url() else {
        eprintln!("skipping postgres identity tests: GARDENBED_TEST_PG_URL is not set");
        return;
    };
    let guard = fresh_schema(&base, &format!("{}ident", run_tag()));
    let url = url_for_schema(&base, &guard.schema);

    // INDEPENDENT handles race the mint: every opener answers the SAME
    // 16 bytes (one insert wins; the losers read the winner's row).
    let handles: Vec<_> = (0..OPENERS)
        .map(|_| {
            let url = url.clone();
            thread::spawn(move || {
                let store =
                    PostgresMetadataStore::open(&url).map_err(|e| format!("open: {e:?}"))?;
                use gardenbed_store::MetadataStore;
                store
                    .repo_incarnation()
                    .map_err(|e| format!("identity: {e:?}"))
            })
        })
        .collect();
    let mut first = None;
    for (i, h) in handles.into_iter().enumerate() {
        let inc = h
            .join()
            .expect("identity thread")
            .unwrap_or_else(|e| panic!("handle {i} failed to mint/read the incarnation: {e}"));
        first.get_or_insert(inc);
        assert_eq!(
            first,
            Some(inc),
            "handle {i} observed a different incarnation — the mint is not atomic"
        );
    }

    // A LATER opener over the same schema (fresh pool, no in-process
    // memory to inherit from) reads the SAME stored bytes: identity
    // survives reopen, exactly like a server restart.
    let later = PostgresMetadataStore::open(&url).expect("later reopen");
    use gardenbed_store::MetadataStore;
    assert_eq!(
        later.repo_incarnation().expect("later identity read"),
        first.expect("at least one racer"),
        "identity must persist across independent handles"
    );

    // A NEW schema (a re-initialized repository) gets a fresh identity:
    // the pair (schema, identity) distinguishes replacement from move.
    let guard2 = fresh_schema(&base, &format!("{}ident2", run_tag()));
    let url2 = url_for_schema(&base, &guard2.schema);
    let other = PostgresMetadataStore::open(&url2).expect("second schema opens");
    let inc2 = other.repo_incarnation().expect("second schema identity");
    assert_ne!(
        first,
        Some(inc2),
        "a new database must not inherit another's incarnation"
    );
}
