//! Connection plumbing of [`GardenbedClient::connect`].
//!
//! No server is spun up here (the server crate owns that); these tests only
//! prove the transport resolves schemes correctly and fails cleanly against
//! dead endpoints.

use gardenbed_protocol::{ConnectError, GardenbedClient};

#[tokio::test]
async fn tcp_connect_to_a_dead_endpoint_fails() {
    // Bind and immediately drop: the port is free again, so the connection
    // is refused.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let result = GardenbedClient::connect(&format!("http://{addr}")).await;
    assert!(result.is_err(), "connecting to a closed TCP port must fail");
}

#[tokio::test]
async fn unix_connect_to_a_missing_socket_fails() {
    let result = GardenbedClient::connect("unix:///tmp/opencode/gardenbed-test-missing.sock").await;
    assert!(result.is_err(), "connecting to a missing socket must fail");
}

#[tokio::test]
async fn unsupported_schemes_are_rejected_with_a_clear_error() {
    for endpoint in ["ftp://example.invalid", "gopher://example.invalid"] {
        let err = GardenbedClient::connect(endpoint)
            .await
            .expect_err("unsupported scheme must fail");
        assert!(
            matches!(err, ConnectError::UnsupportedScheme(_)),
            "{endpoint}: {err:?}"
        );
        assert!(
            err.to_string().contains("unsupported endpoint"),
            "{endpoint}: message should say what went wrong: {err}"
        );
    }
}

#[tokio::test]
async fn garbage_endpoints_are_rejected() {
    let err = GardenbedClient::connect("not a uri at all")
        .await
        .expect_err("garbage must fail");
    assert!(matches!(err, ConnectError::UnsupportedScheme(_)), "{err:?}");
}
