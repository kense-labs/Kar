//! Local HTTP protocol peers. They never use Expo credentials or an installed Simlock gateway.
use super::*;
use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    response::IntoResponse,
    routing::any,
};
use base64::Engine as _;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
type Reply = axum::response::Response;
#[derive(Default)]
struct Peer {
    origin: String,
    recorded: StdMutex<Vec<(String, Value)>>,
    reject_create: AtomicBool,
    uncertain: AtomicBool,
    expo_stopped: AtomicBool,
}
fn response(v: Value) -> Reply {
    axum::Json(v).into_response()
}
fn png() -> Reply {
    ([("content-type","image/png")],base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aVxkAAAAASUVORK5CYII=").unwrap()).into_response()
}
async fn handle(State(peer): State<Arc<Peer>>, req: Request) -> Reply {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();
    let query = req.uri().query().unwrap_or("").to_owned();
    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let tenant = req.headers().get("x-agent-device-tenant").cloned();
    let lease = req.headers().get("x-agent-device-lease-id").cloned();
    let body = axum::body::to_bytes(req.into_body(), 256 * 1024)
        .await
        .unwrap();
    let v = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    peer.recorded
        .lock()
        .unwrap()
        .push((format!("{method} {path}"), v.clone()));
    match path.as_str() {
        "/v1/catalog" => {
            assert_eq!(token, "Bearer sim-secret");
            response(
                json!({"platforms":[{"platform":"ios","modelRuntimes":{"iPhone":["18","19"],"Watch":["9"],"Offline":["1"]},"modelClasses":{"iPhone":"phone","Watch":"watch"},"modelWorkers":{"Offline":[]}},{"platform":"android","modelRuntimes":{}}]}),
            )
        }
        "/v1/agent-device/rpc" => {
            assert_eq!(token, "Bearer sim-secret");
            let p = &v["params"];
            let data = match v["method"].as_str().unwrap() {
                "agent_device.lease.allocate" => {
                    assert_eq!(p["device"], "iPhone");
                    assert_eq!(p["backend"], "ios-instance");
                    assert_eq!(p["leaseId"].as_str().unwrap().len(), 32);
                    json!({"lease":{"leaseId":"gateway-id","tenantId":"proxy","runId":p["runId"],"clientId":p["clientId"],"backend":p["backend"],"deviceKey":"ios:mobile:UDID"}})
                }
                "agent_device.command" => {
                    assert_eq!(p["meta"]["leaseId"], "gateway-id");
                    assert_eq!(p["meta"]["deviceKey"], "ios:mobile:UDID");
                    if p["command"] == "screenshot" {
                        assert!(
                            p["positionals"][0]
                                .as_str()
                                .unwrap()
                                .starts_with("/tmp/agent-device-screenshot-")
                        );
                        json!({"artifacts":[{"field":"path","artifactId":"image"}]})
                    } else {
                        json!({})
                    }
                }
                _ => panic!("unexpected RPC"),
            };
            response(json!({"jsonrpc":"2.0","id":v["id"],"result":{"ok":true,"data":data}}))
        }
        "/v1/agent-device/artifacts/image" => {
            assert_eq!(token, "Bearer sim-secret");
            assert_eq!(tenant.unwrap(), "proxy");
            assert_eq!(lease.unwrap(), "gateway-id");
            png()
        }
        "/v1/leases/gateway%2Did/renew"
        | "/v1/leases/gateway%2Did"
        | "/v1/leases/gateway-id/renew"
        | "/v1/leases/gateway-id" => {
            assert_eq!(token, "Bearer sim-secret");
            assert!(body.is_empty());
            StatusCode::NO_CONTENT.into_response()
        }
        "/graphql" => {
            assert_eq!(token, "Bearer expo-secret");
            let vars = &v["variables"];
            let doc = v["query"].as_str().unwrap();
            if doc.contains("KarCreateSimulator") {
                let input = &vars["input"];
                assert_eq!(input["type"], "AGENT_DEVICE");
                assert_eq!(input["platform"], "ANDROID");
                assert_eq!(input["maxIdleTimeMinutes"], 10);
                assert_eq!(input["maxRunTimeMinutes"], 30);
                if peer.reject_create.load(Ordering::SeqCst) {
                    return response(
                        json!({"data":null,"errors":[{"extensions":{"errorType":"USER"}}]}),
                    );
                }
                if peer.uncertain.load(Ordering::SeqCst) {
                    return StatusCode::BAD_GATEWAY.into_response();
                }
                response(
                    json!({"data":{"deviceRunSession":{"createDeviceRunSession":{"id":"expo-id"}}}}),
                )
            } else if doc.contains("KarStopSimulator") {
                assert_eq!(vars["id"], "expo-id");
                peer.expo_stopped.store(true, Ordering::SeqCst);
                response(
                    json!({"data":{"deviceRunSession":{"ensureDeviceRunSessionStopped":{"id":"expo-id","status":"STOPPED"}}}}),
                )
            } else if doc.contains("KarActiveSimulators") {
                if peer.reject_create.load(Ordering::SeqCst) {
                    return response(
                        json!({"data":{"app":{"byId":{"deviceRunSessionsPaginated":{"edges":[],"pageInfo":{"hasNextPage":false}}}}}}),
                    );
                }
                let recorded = peer.recorded.lock().unwrap();
                let name = recorded
                    .iter()
                    .find(|(_, v)| {
                        v["query"]
                            .as_str()
                            .is_some_and(|q| q.contains("KarCreateSimulator"))
                    })
                    .unwrap()
                    .1["variables"]["input"]["name"]
                    .clone();
                response(
                    json!({"data":{"app":{"byId":{"deviceRunSessionsPaginated":{"edges":[{"node":{"id":"other","name":"someone-else"}},{"node":{"id":"expo-id","name":name}}],"pageInfo":{"hasNextPage":false}}}}}}),
                )
            } else {
                assert_eq!(vars["id"], "expo-id");
                response(
                    json!({"data":{"deviceRunSessions":{"byId":{"id":"expo-id","status":"IN_PROGRESS","remoteConfig":{"__typename":"AgentDeviceRunSessionRemoteConfig","agentDeviceRemoteSessionUrl":format!("{}/agent",peer.origin),"agentDeviceRemoteSessionToken":"agent-only","webPreviewToken":"preview-only","previewApiUrl":format!("{}/path",peer.origin)}}}}}),
                )
            }
        }
        "/agent/rpc" => {
            assert_eq!(token, "Bearer agent-only");
            let p = &v["params"];
            assert_eq!(p["flags"]["serial"], "device / shared");
            assert!(p["session"].as_str().unwrap().starts_with("kar-"));
            response(
                json!({"jsonrpc":"2.0","id":v["id"],"result":{"ok":true,"data":{"command":p["command"]}}}),
            )
        }
        "/vendor/serve-emu/api" => {
            assert_eq!(token, "Bearer preview-only");
            response(json!({"serial":"device / shared"}))
        }
        "/vendor/serve-emu/api/screenshot" => {
            assert_eq!(token, "Bearer preview-only");
            assert_eq!(
                url::form_urlencoded::parse(query.as_bytes())
                    .next()
                    .unwrap()
                    .1,
                "device / shared"
            );
            png()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
async fn peer() -> Arc<Peer> {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p = Arc::new(Peer {
        origin: format!("http://{}", socket.local_addr().unwrap()),
        ..Default::default()
    });
    let router = Router::new().fallback(any(handle)).with_state(p.clone());
    tokio::spawn(async move { axum::serve(socket, router).await.unwrap() });
    p
}
fn host(p: &Peer, expo: bool) -> Host {
    serde_json::from_value(if expo{json!({"id":"expo","kind":"eas","label":"Expo","token":"expo-secret","app_id":Uuid::new_v4(),"max_sessions":1})}else{json!({"id":"sim","kind":"simlock","label":"Sim","token":"sim-secret","gateway_url":p.origin,"max_sessions":1})}).unwrap()
}
#[tokio::test]
async fn simlock_owns_the_returned_lease_and_releases_it_after_restart() {
    let p = peer().await;
    let tmp = tempfile::tempdir().unwrap();
    let h = host(&p, false);
    h.validate().unwrap();
    let cloud = Cloud::open(tmp.path()).unwrap();
    let catalog = cloud.list(&h).await.unwrap();
    assert_eq!(catalog.len(), 1);
    let d = &catalog[0];
    assert_eq!(d.version, "iOS 19");
    assert!(d.cloud);
    let chat = Uuid::new_v4();
    let id = cloud.boot(&h, d, chat, tmp.path()).await.unwrap();
    assert_eq!(id, "gateway-id");
    assert_eq!(cloud.boot(&h, d, chat, tmp.path()).await.unwrap(), id);
    assert!(cloud.boot(&h, d, Uuid::new_v4(), tmp.path()).await.is_err());
    let current = cloud.owned(&h, &id).unwrap();
    assert!(
        cloud
            .screenshot(&current)
            .await
            .unwrap()
            .starts_with(b"\x89PNG")
    );
    assert!(
        simlock::renew(&cloud.client, &cloud.find(&h.id, &id).unwrap())
            .await
            .unwrap()
    );
    drop(cloud);
    let reopened = Cloud::open(tmp.path()).unwrap();
    assert_eq!(reopened.startup.len(), 1);
    reopened.shutdown(&h.id, &id).await.unwrap();
    assert!(reopened.db.lock().unwrap().is_empty());
    let calls = p.recorded.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|(_, v)| v["method"] == "agent_device.lease.allocate")
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(s, _)| s.starts_with("DELETE "))
            .count(),
        1
    );
}
#[tokio::test]
async fn expo_uses_workspace_billing_private_endpoints_and_a_confirmed_stop() {
    let p = peer().await;
    let tmp = tempfile::tempdir().unwrap();
    let mut cloud = Cloud::open(tmp.path()).unwrap();
    cloud.graph = format!("{}/graphql", p.origin);
    let h = host(&p, true);
    h.validate().unwrap();
    let workspace = Uuid::new_v4();
    let nested = tmp.path().join("apps/mobile");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("app.json"),
        json!({"expo":{"extra":{"eas":{"projectId":workspace}}}}).to_string(),
    )
    .unwrap();
    let d = cloud.list(&h).await.unwrap().pop().unwrap();
    let id = cloud
        .boot(&h, &d, Uuid::new_v4(), tmp.path())
        .await
        .unwrap();
    assert_eq!(id, "expo-id");
    let owned = cloud.owned(&h, &id).unwrap();
    assert!(
        cloud
            .screenshot(&owned)
            .await
            .unwrap()
            .starts_with(b"\x89PNG")
    );
    assert_eq!(
        cloud
            .agent_command(&owned, &json!({"type":"agentCommand","command":"snapshot"}))
            .await
            .unwrap()["command"],
        "snapshot"
    );
    assert!(
        cloud
            .agent_command(
                &owned,
                &json!({"type":"agentCommand","command":"shell","arguments":["id"]})
            )
            .await
            .is_err()
    );
    let commands: Vec<_> = p
        .recorded
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p == "POST /agent/rpc")
        .map(|(_, v)| v["params"]["command"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(commands, ["open", "snapshot", "close"]);
    let snap = Live {
        settings: Settings {
            cloud_hosts: vec![h.clone()],
            ..Default::default()
        },
        ..Default::default()
    }
    .snapshot()
    .to_string();
    assert!(!snap.contains("expo-secret"));
    assert!(snap.contains("has_credential"));
    assert!(
        p.recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(_, v)| v["variables"]["input"]["appId"] == workspace.to_string())
    );
    drop(cloud);
    let mut reopened = Cloud::open(tmp.path()).unwrap();
    reopened.graph = format!("{}/graphql", p.origin);
    reopened.shutdown(&h.id, &id).await.unwrap();
    assert!(p.expo_stopped.load(Ordering::SeqCst));
}
#[tokio::test]
async fn expo_recovers_an_unacknowledged_create_by_its_persisted_name() {
    let p = peer().await;
    p.uncertain.store(true, Ordering::SeqCst);
    let tmp = tempfile::tempdir().unwrap();
    let mut cloud = Cloud::open(tmp.path()).unwrap();
    cloud.graph = format!("{}/graphql", p.origin);
    let h = host(&p, true);
    let d = expo::catalog().pop().unwrap();
    assert!(
        cloud
            .boot(&h, &d, Uuid::new_v4(), tmp.path())
            .await
            .is_err()
    );
    assert!(p.expo_stopped.load(Ordering::SeqCst));
    assert!(cloud.db.lock().unwrap().is_empty());
    assert_eq!(
        p.recorded
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, v)| v["query"]
                .as_str()
                .is_some_and(|q| q.contains("KarCreateSimulator")))
            .count(),
        1
    );
}

#[tokio::test]
async fn expo_refusal_does_not_block_a_new_open_or_create_twice() {
    let p = peer().await;
    p.reject_create.store(true, Ordering::SeqCst);
    let tmp = tempfile::tempdir().unwrap();
    let mut cloud = Cloud::open(tmp.path()).unwrap();
    cloud.graph = format!("{}/graphql", p.origin);
    let h = host(&p, true);
    let d = expo::catalog().pop().unwrap();
    let chat = Uuid::new_v4();
    assert!(cloud.boot(&h, &d, chat, tmp.path()).await.is_err());
    assert_eq!(cloud.public_leases()[0]["counts_toward_limit"], false);
    p.reject_create.store(false, Ordering::SeqCst);
    let id = cloud.boot(&h, &d, chat, tmp.path()).await.unwrap();
    cloud.shutdown(&h.id, &id).await.unwrap();
}
