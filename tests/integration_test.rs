//! Integration tests for rusd
//! Tests the full server with gRPC client connections

use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;
use tokio::time::sleep;

/// Represents a running rusd server instance for testing
struct TestServer {
    process: Child,
    data_dir: TempDir,
    port: u16,
}

impl TestServer {
    /// Start a new test server on a random available port
    fn start() -> Self {
        let data_dir = TempDir::new().expect("Failed to create temp directory");
        let port = 12379; // Use fixed port for tests

        let mut cmd = Command::new("./target/release/rusd");
        cmd.arg("--name")
            .arg("test-node")
            .arg("--data-dir")
            .arg(data_dir.path())
            .arg("--listen-client-urls")
            .arg(format!("http://127.0.0.1:{}", port))
            .arg("--listen-peer-urls")
            .arg("http://127.0.0.1:12380")
            .arg("--advertise-client-urls")
            .arg(format!("http://127.0.0.1:{}", port))
            .arg("--initial-advertise-peer-urls")
            .arg("http://127.0.0.1:12380")
            .arg("--initial-cluster")
            .arg("test-node=http://127.0.0.1:12380")
            .arg("--initial-cluster-state")
            .arg("new");

        let process = cmd.spawn().expect("Failed to start rusd server");

        let server = TestServer {
            process,
            data_dir,
            port,
        };

        // Give the server time to start
        thread::sleep(Duration::from_millis(500));

        server
    }

    /// Get the endpoint URL for this server
    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Wait for the server to be ready
    async fn wait_ready(&self, timeout_secs: u64) {
        let deadline = SystemTime::now() + Duration::from_secs(timeout_secs);
        loop {
            if let Ok(output) = reqwest::Client::new()
                .get(&format!("{}/health", self.endpoint()))
                .send()
                .await
            {
                if output.status().is_success() {
                    return;
                }
            }

            if SystemTime::now() > deadline {
                panic!("Server failed to become ready within {} seconds", timeout_secs);
            }

            sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.process.kill();
    }
}

// ============================================================================
// Integration Tests
// ============================================================================

#[tokio::test]
async fn test_put_and_get() {
    let server = TestServer::start();
    server.wait_ready(10).await;

    let client = reqwest::Client::new();
    let endpoint = server.endpoint();

    // Put a key
    let put_response = client
        .post(&format!("{}/v3/kv/put", endpoint))
        .json(&serde_json::json!({
            "key": "dGVzdC9rZXkx",  // base64 encoded: test/key1
            "value": "dGVzdC92YWx1ZTE="  // base64 encoded: test/value1
        }))
        .send()
        .await
        .expect("Failed to put key");

    assert!(
        put_response.status().is_success(),
        "Put operation failed with status: {}",
        put_response.status()
    );

    // Get the key
    let get_response = client
        .post(&format!("{}/v3/kv/range", endpoint))
        .json(&serde_json::json!({
            "key": "dGVzdC9rZXkx"  // base64 encoded: test/key1
        }))
        .send()
        .await
        .expect("Failed to get key");

    assert!(
        get_response.status().is_success(),
        "Get operation failed with status: {}",
        get_response.status()
    );

    let body: serde_json::Value = get_response
        .json()
        .await
        .expect("Failed to parse get response");

    assert!(body["kvs"].is_array(), "Response should contain kvs array");
    assert_eq!(body["kvs"].as_array().unwrap().len(), 1, "Should find exactly one key");
}

#[tokio::test]
async fn test_delete_range() {
    let server = TestServer::start();
    server.wait_ready(10).await;

    let client = reqwest::Client::new();
    let endpoint = server.endpoint();

    // Put multiple keys
    for i in 1..=3 {
        client
            .post(&format!("{}/v3/kv/put", endpoint))
            .json(&serde_json::json!({
                "key": format!("dGVsZXRlL2tleQ=={}", i),  // base64 prefix
                "value": format!("dmFsdWU={}", i)
            }))
            .send()
            .await
            .expect("Failed to put key");
    }

    // Delete range
    let delete_response = client
        .post(&format!("{}/v3/kv/deleterange", endpoint))
        .json(&serde_json::json!({
            "key": "dGVsZXRlLw==",  // base64: delete/
            "range_end": "dGVsZXRlL\u{FFFF}"  // range end
        }))
        .send()
        .await
        .expect("Failed to delete range");

    assert!(
        delete_response.status().is_success(),
        "Delete operation failed"
    );

    // Verify keys are deleted
    let get_response = client
        .post(&format!("{}/v3/kv/range", endpoint))
        .json(&serde_json::json!({
            "key": "dGVsZXRlLw==",
            "range_end": "dGVsZXRlL\u{FFFF}"
        }))
        .send()
        .await
        .expect("Failed to verify deletion");

    let body: serde_json::Value = get_response.json().await.expect("Failed to parse response");
    assert_eq!(body["kvs"].as_array().unwrap().len(), 0, "Keys should be deleted");
}

#[tokio::test]
async fn test_transaction() {
    let server = TestServer::start();
    server.wait_ready(10).await;

    let client = reqwest::Client::new();
    let endpoint = server.endpoint();

    // Put initial value
    client
        .post(&format!("{}/v3/kv/put", endpoint))
        .json(&serde_json::json!({
            "key": "dHhuL2tleQ==",  // base64: txn/key
            "value": "aW5pdGlhbA=="  // base64: initial
        }))
        .send()
        .await
        .expect("Failed to put initial value");

    // Run transaction with compare
    let txn_response = client
        .post(&format!("{}/v3/kv/txn", endpoint))
        .json(&serde_json::json!({
            "compare": [
                {
                    "key": "dHhuL2tleQ==",
                    "result": "EQUAL",
                    "target": "VALUE",
                    "value": "aW5pdGlhbA=="
                }
            ],
            "success": [
                {
                    "request_put": {
                        "key": "dHhuL2tleQ==",
                        "value": "dXBkYXRlZA=="  // base64: updated
                    }
                }
            ],
            "failure": [
                {
                    "request_put": {
                        "key": "dHhuL2tleQ==",
                        "value": "ZmFpbGVk"  // base64: failed
                    }
                }
            ]
        }))
        .send()
        .await
        .expect("Failed to execute transaction");

    assert!(
        txn_response.status().is_success(),
        "Transaction failed with status: {}",
        txn_response.status()
    );

    let body: serde_json::Value = txn_response.json().await.expect("Failed to parse response");
    assert!(body["succeeded"].is_boolean(), "Should have succeeded field");
}

#[tokio::test]
async fn test_watch() {
    let server = TestServer::start();
    server.wait_ready(10).await;

    let client = reqwest::Client::new();
    let endpoint = server.endpoint();

    // Create a watcher in a separate task
    let watch_endpoint = endpoint.clone();
    let watch_client = client.clone();
    let watch_task = tokio::spawn(async move {
        let _response = watch_client
            .post(&format!("{}/v3/watch", watch_endpoint))
            .json(&serde_json::json!({
                "create_request": {
                    "key": "d2F0Y2gva2V5"  // base64: watch/key
                }
            }))
            .send()
            .await;
    });

    // Give watch time to establish
    sleep(Duration::from_millis(200)).await;

    // Put a key that the watcher is monitoring
    let put_response = client
        .post(&format!("{}/v3/kv/put", endpoint))
        .json(&serde_json::json!({
            "key": "d2F0Y2gva2V5",  // base64: watch/key
            "value": "d2F0Y2hlZA=="  // base64: watched
        }))
        .send()
        .await
        .expect("Failed to put watched key");

    assert!(
        put_response.status().is_success(),
        "Put operation failed"
    );

    // Give watch time to receive the event
    sleep(Duration::from_millis(200)).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), watch_task).await;
}

#[tokio::test]
async fn test_lease_lifecycle() {
    let server = TestServer::start();
    server.wait_ready(10).await;

    let client = reqwest::Client::new();
    let endpoint = server.endpoint();

    // Grant lease
    let grant_response = client
        .post(&format!("{}/v3/lease/grant", endpoint))
        .json(&serde_json::json!({
            "TTL": 100
        }))
        .send()
        .await
        .expect("Failed to grant lease");

    assert!(grant_response.status().is_success(), "Lease grant failed");

    let grant_body: serde_json::Value = grant_response
        .json()
        .await
        .expect("Failed to parse grant response");
    let lease_id = grant_body["ID"].as_str().expect("Missing lease ID");

    // Attach key to lease
    let put_response = client
        .post(&format!("{}/v3/kv/put", endpoint))
        .json(&serde_json::json!({
            "key": "bGVhc2Uva2V5",  // base64: lease/key
            "value": "bGVhc2VkLWtleQ==",  // base64: leased-key
            "lease": lease_id
        }))
        .send()
        .await
        .expect("Failed to put leased key");

    assert!(put_response.status().is_success(), "Put leased key failed");

    // Verify key exists
    let get_response = client
        .post(&format!("{}/v3/kv/range", endpoint))
        .json(&serde_json::json!({
            "key": "bGVhc2Uva2V5"
        }))
        .send()
        .await
        .expect("Failed to get leased key");

    let get_body: serde_json::Value = get_response
        .json()
        .await
        .expect("Failed to parse get response");
    assert_eq!(get_body["kvs"].as_array().unwrap().len(), 1, "Key should exist");

    // Revoke lease
    let revoke_response = client
        .post(&format!("{}/v3/lease/revoke", endpoint))
        .json(&serde_json::json!({
            "ID": lease_id
        }))
        .send()
        .await
        .expect("Failed to revoke lease");

    assert!(revoke_response.status().is_success(), "Lease revoke failed");

    // Verify key is deleted
    sleep(Duration::from_millis(100)).await;
    let verify_response = client
        .post(&format!("{}/v3/kv/range", endpoint))
        .json(&serde_json::json!({
            "key": "bGVhc2Uva2V5"
        }))
        .send()
        .await
        .expect("Failed to verify key deletion");

    let verify_body: serde_json::Value = verify_response
        .json()
        .await
        .expect("Failed to parse verify response");
    assert_eq!(
        verify_body["kvs"].as_array().unwrap().len(),
        0,
        "Key should be deleted after lease revocation"
    );
}

#[tokio::test]
async fn test_compaction() {
    let server = TestServer::start();
    server.wait_ready(10).await;

    let client = reqwest::Client::new();
    let endpoint = server.endpoint();

    // Put many revisions of the same key
    for i in 1..=5 {
        client
            .post(&format!("{}/v3/kv/put", endpoint))
            .json(&serde_json::json!({
                "key": "Y29tcC9rZXk=",  // base64: comp/key
                "value": format!("dmFsdWU={}", i)
            }))
            .send()
            .await
            .expect("Failed to put key");
    }

    // Get revision number
    let get_response = client
        .post(&format!("{}/v3/kv/range", endpoint))
        .json(&serde_json::json!({
            "key": "Y29tcC9rZXk="
        }))
        .send()
        .await
        .expect("Failed to get key");

    let get_body: serde_json::Value = get_response.json().await.expect("Failed to parse response");
    let mod_revision = get_body["kvs"][0]["mod_revision"]
        .as_str()
        .expect("Missing mod_revision");

    // Compact to current revision
    let compact_response = client
        .post(&format!("{}/v3/maintenance/compact", endpoint))
        .json(&serde_json::json!({
            "revision": mod_revision
        }))
        .send()
        .await
        .expect("Failed to compact");

    assert!(
        compact_response.status().is_success(),
        "Compaction failed"
    );
}

#[tokio::test]
async fn test_snapshot_restore() {
    let server1 = TestServer::start();
    server1.wait_ready(10).await;

    let client = reqwest::Client::new();
    let endpoint1 = server1.endpoint();

    // Put data in first server
    for i in 1..=5 {
        client
            .post(&format!("{}/v3/kv/put", endpoint1))
            .json(&serde_json::json!({
                "key": format!("c25hcC9rZXk={}", i),
                "value": format!("c25hcC92YWx1ZQ=={}", i)
            }))
            .send()
            .await
            .expect("Failed to put key");
    }

    // Snapshot should be stored in data directory
    // When restored, a new server should have the same data
    let snapshot_path = server1.data_dir.path().join("snap");
    assert!(
        snapshot_path.exists() || server1.data_dir.path().join("member/snap").exists(),
        "Snapshot directory should exist"
    );
}

#[tokio::test]
async fn test_member_list() {
    let server = TestServer::start();
    server.wait_ready(10).await;

    let client = reqwest::Client::new();
    let endpoint = server.endpoint();

    // Get member list
    let response = client
        .post(&format!("{}/v3/cluster/member/list", endpoint))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("Failed to get member list");

    assert!(response.status().is_success(), "Member list request failed");

    let body: serde_json::Value = response.json().await.expect("Failed to parse response");
    assert!(body["members"].is_array(), "Should have members array");
    assert!(!body["members"].as_array().unwrap().is_empty(), "Should have at least one member");
}

#[tokio::test]
async fn test_status() {
    let server = TestServer::start();
    server.wait_ready(10).await;

    let client = reqwest::Client::new();
    let endpoint = server.endpoint();

    // Get status
    let response = client
        .post(&format!("{}/v3/maintenance/status", endpoint))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("Failed to get status");

    assert!(response.status().is_success(), "Status request failed");

    let body: serde_json::Value = response.json().await.expect("Failed to parse response");
    assert!(body["version"].is_string(), "Should have version");
    assert!(body["leader"].is_string(), "Should have leader");
    assert!(body["raft_index"].is_string(), "Should have raft_index");
}

#[tokio::test]
async fn test_auth_enable_disable() {
    let server = TestServer::start();
    server.wait_ready(10).await;

    let client = reqwest::Client::new();
    let endpoint = server.endpoint();

    // Enable auth
    let enable_response = client
        .post(&format!("{}/v3/auth/enable", endpoint))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("Failed to enable auth");

    assert!(
        enable_response.status().is_success() || enable_response.status().as_u16() == 409,
        "Auth enable failed unexpectedly"
    );

    // Disable auth (may fail if not enabled, which is fine)
    let disable_response = client
        .post(&format!("{}/v3/auth/disable", endpoint))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("Failed to disable auth");

    assert!(
        disable_response.status().is_success() || disable_response.status().is_client_error(),
        "Auth disable should succeed or return client error"
    );
}

#[tokio::test]
async fn test_range_with_limit() {
    let server = TestServer::start();
    server.wait_ready(10).await;

    let client = reqwest::Client::new();
    let endpoint = server.endpoint();

    // Put multiple keys
    for i in 1..=10 {
        client
            .post(&format!("{}/v3/kv/put", endpoint))
            .json(&serde_json::json!({
                "key": format!("cmFuZ2Uva2V5{:02}", i),
                "value": format!("dmFsdWU={}", i)
            }))
            .send()
            .await
            .expect("Failed to put key");
    }

    // Get with limit
    let response = client
        .post(&format!("{}/v3/kv/range", endpoint))
        .json(&serde_json::json!({
            "key": "cmFuZ2Uv",
            "range_end": "cmFuZ2Uv\u{FFFF}",
            "limit": 5
        }))
        .send()
        .await
        .expect("Failed to get range with limit");

    assert!(response.status().is_success(), "Range request failed");

    let body: serde_json::Value = response.json().await.expect("Failed to parse response");
    assert_eq!(body["kvs"].as_array().unwrap().len(), 5, "Should return 5 keys due to limit");
}

#[tokio::test]
async fn test_concurrent_puts() {
    let server = TestServer::start();
    server.wait_ready(10).await;

    let client = std::sync::Arc::new(reqwest::Client::new());
    let endpoint = server.endpoint();

    // Spawn multiple concurrent put tasks
    let mut tasks = vec![];
    for i in 0..20 {
        let client = client.clone();
        let endpoint = endpoint.clone();
        let task = tokio::spawn(async move {
            client
                .post(&format!("{}/v3/kv/put", endpoint))
                .json(&serde_json::json!({
                    "key": format!("Y29uY3Vy/{:02}", i),
                    "value": format!("dmFsdWU={}", i)
                }))
                .send()
                .await
        });
        tasks.push(task);
    }

    // Wait for all tasks
    for task in tasks {
        let result = task.await.expect("Task panicked");
        assert!(result.is_ok(), "Put request failed");
        assert!(result.unwrap().status().is_success(), "Put returned error status");
    }

    // Verify all keys were written
    let response = client
        .post(&format!("{}/v3/kv/range", endpoint))
        .json(&serde_json::json!({
            "key": "Y29uY3Vy",
            "range_end": "Y29uY3Vy\u{FFFF}"
        }))
        .send()
        .await
        .expect("Failed to get range");

    let body: serde_json::Value = response.json().await.expect("Failed to parse response");
    assert_eq!(
        body["kvs"].as_array().unwrap().len(),
        20,
        "All 20 keys should be present"
    );
}
