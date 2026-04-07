//! Tests for the RPC records infrastructure.

use rstest::rstest;
use serde_json::json;

use crate::running::rpc_records::{
    normalize_json,
    MockRpcServer,
    RecordingProxy,
    RpcInteraction,
    RpcRecords,
};

#[rstest]
#[case::sorts_string_array(json!(["c", "a", "b"]), json!(["a", "b", "c"]))]
#[case::sorts_numeric_array(json!([3, 1, 2]), json!([1, 2, 3]))]
#[case::sorts_nested_arrays(
    json!({"outer": ["z", "a"], "nested": {"inner": [3, 1, 2]}}),
    json!({"outer": ["a", "z"], "nested": {"inner": [1, 2, 3]}})
)]
#[case::preserves_primitive(json!(42), json!(42))]
fn test_normalize_json(#[case] input: serde_json::Value, #[case] expected: serde_json::Value) {
    assert_eq!(normalize_json(&input), expected);
}

#[test]
fn test_rpc_records_save_and_load() {
    let records = RpcRecords {
        interactions: vec![RpcInteraction {
            method: "starknet_getNonce".to_string(),
            sorted_params: serde_json::json!({"block_id": "latest", "contract_address": "0x1"}),
            response: serde_json::json!({"jsonrpc": "2.0", "id": 0, "result": "0x0"}),
        }],
    };

    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("test_record.json");

    records.save(&path);
    let loaded = RpcRecords::load(&path);

    assert_eq!(loaded.interactions.len(), 1);
    assert_eq!(loaded.interactions[0].method, "starknet_getNonce");
}

#[tokio::test]
async fn test_mock_server_matches_rpc_request() {
    let records = RpcRecords {
        interactions: vec![RpcInteraction {
            method: "starknet_blockNumber".to_string(),
            sorted_params: serde_json::json!([]),
            response: serde_json::json!({
                "jsonrpc": "2.0",
                "id": 0,
                "result": 42
            }),
        }],
    };

    let server = MockRpcServer::new(&records).await;

    // Send a JSON-RPC request with different id/jsonrpc version (should still match).
    let client = reqwest::Client::new();
    let response = client
        .post(server.url())
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "starknet_blockNumber",
            "params": []
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["result"], 42);
}

#[tokio::test]
async fn test_mock_server_replays_duplicate_requests_in_recorded_order() {
    let records = RpcRecords {
        interactions: vec![
            RpcInteraction {
                method: "starknet_blockHashAndNumber".to_string(),
                sorted_params: serde_json::json!([]),
                response: serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": { "block_hash": "0x1", "block_number": 1 }
                }),
            },
            RpcInteraction {
                method: "starknet_blockHashAndNumber".to_string(),
                sorted_params: serde_json::json!([]),
                response: serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": { "block_hash": "0x2", "block_number": 2 }
                }),
            },
        ],
    };

    let server = MockRpcServer::new(&records).await;
    let client = reqwest::Client::new();
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "starknet_blockHashAndNumber",
        "params": []
    });

    let first = client.post(server.url()).json(&request).send().await.unwrap();
    let second = client.post(server.url()).json(&request).send().await.unwrap();

    assert_eq!(first.status(), 200);
    assert_eq!(second.status(), 200);

    let first_body: serde_json::Value = first.json().await.unwrap();
    let second_body: serde_json::Value = second.json().await.unwrap();

    assert_eq!(first_body["result"]["block_hash"], "0x1");
    assert_eq!(second_body["result"]["block_hash"], "0x2");
}

#[tokio::test]
async fn test_mock_server_replays_matching_request_out_of_order() {
    let records = RpcRecords {
        interactions: vec![
            RpcInteraction {
                method: "starknet_getNonce".to_string(),
                sorted_params: serde_json::json!({"block_id": "latest", "contract_address": "0x1"}),
                response: serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": "0x0"
                }),
            },
            RpcInteraction {
                method: "starknet_getClassHashAt".to_string(),
                sorted_params: serde_json::json!({"block_id": "latest", "contract_address": "0x1"}),
                response: serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": "0x123"
                }),
            },
        ],
    };

    let server = MockRpcServer::new(&records).await;
    let client = reqwest::Client::new();

    let response = client
        .post(server.url())
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "starknet_getClassHashAt",
            "params": {"block_id": "latest", "contract_address": "0x1"}
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["result"], "0x123");

    let next_response = client
        .post(server.url())
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "starknet_getNonce",
            "params": {"block_id": "latest", "contract_address": "0x1"}
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(next_response.status(), 200);
    let next_body: serde_json::Value = next_response.json().await.unwrap();
    assert_eq!(next_body["result"], "0x0");
}

/// End-to-end test: record interactions through proxy, save to file, load, and replay.
#[tokio::test]
async fn test_record_save_load_replay_round_trip() {
    // 1. Set up a mock backend.
    let mut backend = mockito::Server::new_async().await;
    let _mock = backend
        .mock("POST", "/")
        .match_body(mockito::Matcher::PartialJson(serde_json::json!({
            "method": "starknet_getNonce"
        })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"jsonrpc":"2.0","id":0,"result":"0x42"}"#)
        .create_async()
        .await;

    // 2. Record through proxy.
    let proxy = RecordingProxy::new(&backend.url()).await;
    let client = reqwest::Client::new();
    client
        .post(&proxy.url)
        .json(&serde_json::json!({
            "jsonrpc": "0.7",
            "id": 0,
            "method": "starknet_getNonce",
            "params": {"block_id": "latest", "contract_address": "0x1"}
        }))
        .send()
        .await
        .unwrap();

    let records = proxy.into_records();

    // 3. Save and reload.
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("recorded.json");
    records.save(&path);
    let loaded = RpcRecords::load(&path);

    // 4. Replay using mock server.
    let mock_server = MockRpcServer::new(&loaded).await;
    let response = client
        .post(mock_server.url())
        .json(&serde_json::json!({
            "jsonrpc": "0.7",
            "id": 0,
            "method": "starknet_getNonce",
            "params": {"block_id": "latest", "contract_address": "0x1"}
        }))
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["result"], "0x42");
}
