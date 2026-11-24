use super::*;

pub(super) async fn controls(
    url: &str,
    host: &Client,
    project: &str,
    chat: Uuid,
    rpc: impl Fn(&str, Value) -> RequestBuilder,
) {
    let policy = format!("{url}/projects/{project}/chats/{chat}/model-tools/permissions");
    let call = |name: &str, input: Value| rpc("tools/call", json!({"name":name,"arguments":input}));
    let tab = Uuid::new_v4();
    let set = |mode: &str| {
        host.put(&policy)
            .json(&json!({"mode":mode,"preview_tools":{"tabs":[tab]}}))
    };
    assert!(server::json(call("kar_preview_list", json!({}))).await["error"].is_object());
    server::json(set("plan")).await;
    let list = server::json(call("kar_preview_list", json!({}))).await;
    assert_eq!(list["result"]["structuredContent"]["sessions"], json!([]));
    assert!(
        server::json(call("kar_preview_close", json!({"tab_id":tab}))).await["error"].is_object()
    );
    server::json(set("execute")).await;
    let close = call("kar_preview_close", json!({"tab_id":tab}));
    assert_eq!(server::json(close).await["result"]["isError"], true);
    let restricted = call("kar_preview_close", json!({"tab_id":tab}))
        .header("kar-mode-limit", "full-access/plan");
    assert!(server::json(restricted).await["error"].is_object());
    server::json(host.put(&policy).json(&json!({"mode":"plan"}))).await;
}

pub(super) async fn devices(
    url: &str,
    host: &Client,
    project: &str,
    chat: Uuid,
    rpc: impl Fn(&str, Value) -> RequestBuilder,
) {
    let policy = format!("{url}/projects/{project}/chats/{chat}/model-tools/permissions");
    let call = |name: &str, input: Value| rpc("tools/call", json!({"name":name,"arguments":input}));
    server::json(
        host.post(format!("{url}/devices/configure"))
            .json(&json!({"enabled":true,"agent_access_enabled":true})),
    )
    .await;
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"execute","preview_tools":{"device_hosts":["local"]}})),
    )
    .await;
    let catalog = server::json(call("kar_device_list", json!({}))).await;
    let udid = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    assert!(
        catalog["result"]["structuredContent"]["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["id"] == udid)
    );
    let target = json!({"host_id":"local","device_id":udid});
    let open = server::json(call("kar_device_open", target.clone())).await;
    let view = &open["result"]["structuredContent"];
    assert!(view["view_id"].is_string());
    let again = server::json(call("kar_device_open", target.clone())).await;
    assert_eq!(
        again["result"]["structuredContent"]["view_id"],
        view["view_id"]
    );
    let screenshot = server::json(call("kar_device_screenshot", json!({}))).await;
    assert_eq!(screenshot["result"]["content"][1]["type"], "image");
    assert_eq!(
        screenshot["result"]["structuredContent"]["screenshot"]["width"],
        1
    );
    assert_eq!(
        server::json(call("kar_device_screenshot", json!({"host_id":"another"}))).await["result"]["isError"],
        true
    );
    let cfg = view["agent_device"]["args"][2].as_str().unwrap();
    let checked = |path: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_kar"))
            .args(["device-agent", "--config", path, "detail"])
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(checked(cfg));
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"plan","preview_tools":{"device_hosts":["local"]}})),
    )
    .await;
    assert!(!checked(cfg), "old launcher retained broader authorization");
    assert!(server::json(call("kar_device_close", target.clone())).await["error"].is_object());
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"execute","preview_tools":{"device_hosts":["local"]}})),
    )
    .await;
    let closed = server::json(call("kar_device_close", target)).await;
    assert_eq!(closed["result"]["structuredContent"]["closed"], 1);
    assert!(!std::path::Path::new(cfg).exists());
    server::json(host.put(&policy).json(&json!({"mode":"plan"}))).await;
}

pub(super) async fn html(
    url: &str,
    host: &Client,
    project: &str,
    chat: Uuid,
    root: &std::path::Path,
    rpc: impl Fn(&str, Value) -> RequestBuilder,
) {
    let policy = format!("{url}/projects/{project}/chats/{chat}/model-tools/permissions");
    let call = |name: &str, input: Value| rpc("tools/call", json!({"name":name,"arguments":input}));
    let page = "<!doctype html><html><body><h1>Kar HTML</h1><script>console.log('measured-by-chrome')</script></body></html>";
    let fp = root.join("mcp-page.html");
    fs::write(&fp, page).unwrap();
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"plan","preview_tools":{"html":true}})),
    )
    .await;
    let from_file = json!({"path":fp,"title":"HTML check","height":180});
    let rejected = server::json(call("kar_html_render", from_file.clone())).await;
    assert_eq!(rejected["result"]["isError"], true);
    if std::env::var_os("KAR_MCP_BROWSER_LIVE").is_some() {
        let preview =
            server::json(call("kar_html_preview", json!({"html":page,"width":390}))).await;
        assert_eq!(preview["result"]["structuredContent"]["width"], 390);
        assert!(
            preview["result"]["structuredContent"]["consoleMessages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["text"] == "measured-by-chrome")
        );
        assert_eq!(preview["result"]["content"][1]["type"], "image");
    }
    server::json(
        host.put(&policy).json(
            &json!({"mode":"plan","preview_tools":{"html":true,"html_workspace_files":true}}),
        ),
    )
    .await;
    let denied = call("kar_html_render", from_file.clone())
        .header("kar-mode-limit", "approval-required/plan");
    assert_eq!(server::json(denied).await["result"]["isError"], true);
    let saved = server::json(call("kar_html_render", from_file)).await;
    let item = &saved["result"]["structuredContent"]["htmlRender"];
    assert!(item["attachmentId"].is_string());
    let ticket = server::json(host.post(format!(
        "{url}{}",
        item["view_url_endpoint"].as_str().unwrap()
    )))
    .await;
    let view = Client::new()
        .get(format!("{url}{}", ticket["read_path"].as_str().unwrap()))
        .send()
        .await
        .unwrap();
    assert_eq!(view.headers()["content-type"], "text/html; charset=utf-8");
    let csp = view.headers()["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("sandbox allow-scripts") && !csp.contains("allow-same-origin"));
    assert!(view.text().await.unwrap().contains("Kar HTML"));
    server::json(host.put(&policy).json(&json!({"mode":"plan"}))).await;
}

pub(super) fn local_page() {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    fs::write(
        "model-preview-port",
        listener.local_addr().unwrap().port().to_string(),
    )
    .unwrap();
    for mut conn in listener.incoming().flatten() {
        let mut r = BufReader::new(conn.try_clone().unwrap());
        let mut line = String::new();
        while r.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
            line.clear();
        }
        let html=b"<!doctype html><html><title>Kar Model Preview</title><body><input id='name'><button id='hello' onclick='document.querySelector(\"h1\").textContent=document.querySelector(\"input\").value'>Apply</button><h1>initial</h1></body></html>";
        let _ = write!(
            conn,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n",
            html.len()
        );
        let _ = conn.write_all(html);
    }
}
pub(super) async fn browser(
    url: &str,
    host: &Client,
    project: &str,
    chat: Uuid,
    root: &std::path::Path,
    rpc: impl Fn(&str, Value) -> RequestBuilder,
) {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let policy = format!("{url}/projects/{project}/chats/{chat}/model-tools/permissions");
    let call = |name: &str, input: Value| rpc("tools/call", json!({"name":name,"arguments":input}));
    let terminal = Uuid::new_v4();
    let terms = format!("{url}/projects/{project}/terminals");
    server::json(host.post(&terms).json(&json!({"id":terminal}))).await;
    let exe = std::env::current_exe().unwrap();
    let line = format!(
        "'{}' --mcp-local-preview\n",
        exe.to_str().unwrap().replace('\'', "'\"'\"'")
    );
    server::json(
        host.post(format!("{terms}/{terminal}/input"))
            .json(&json!({"id":Uuid::new_v4(),"data_base64":STANDARD.encode(line)})),
    )
    .await;
    let port = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(port) = fs::read_to_string(root.join("model-preview-port"))
                .ok()
                .and_then(|s| s.parse::<u16>().ok())
            {
                break port;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap();
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"execute","preview_tools":{"terminals":[terminal]}})),
    )
    .await;
    let open = server::json(call(
        "kar_preview_open",
        json!({"terminal_id":terminal,"port":port}),
    ))
    .await;
    let view = &open["result"]["structuredContent"];
    let tab = view["tab_id"].as_str().expect("model-opened tab");
    let target = |name: &str, mut args: Value| {
        args["tab_id"] = tab.into();
        call(name, args)
    };
    let list = server::json(call("kar_preview_list", json!({}))).await;
    assert_eq!(
        list["result"]["structuredContent"]["sessions"][0]["tab_id"],
        tab
    );
    for (name, q) in [
        (
            "kar_preview_wait_for",
            json!({"selector":"#hello","timeout_ms":10000}),
        ),
        (
            "kar_preview_resize",
            json!({"viewport":{"_tag":"freeform","width":400,"height":300}}),
        ),
        ("kar_preview_set_appearance", json!({"appearance":"light"})),
        ("kar_preview_recording_start", json!({})),
        (
            "kar_preview_act",
            json!({"action":"type","selector":"#name","text":"Kar checked"}),
        ),
        (
            "kar_preview_act",
            json!({"action":"click","selector":"#hello"}),
        ),
        ("kar_preview_wait_for", json!({"text":"Kar checked"})),
    ] {
        let result = server::json(target(name, q)).await;
        assert!(
            result["result"]["structuredContent"].is_object(),
            "{name}: {result}"
        );
    }
    let val = server::json(target(
        "kar_preview_evaluate",
        json!({"expression":"document.querySelector('h1').textContent"}),
    ))
    .await;
    assert_eq!(val["result"]["structuredContent"]["value"], "Kar checked");
    let shot = server::json(target(
        "kar_preview_snapshot",
        json!({"include_image":true,"save":true}),
    ))
    .await;
    assert_eq!(shot["result"]["content"][1]["type"], "image");
    assert!(shot["result"]["structuredContent"]["attachment"]["id"].is_string());
    let video = server::json(target("kar_preview_recording_stop", json!({}))).await;
    let id = video["result"]["structuredContent"]["id"].as_str().unwrap();
    let signed = server::json(host.post(format!(
        "{url}/projects/{project}/chats/{chat}/attachments/{id}/url"
    )))
    .await;
    let bytes = Client::new()
        .get(format!("{url}{}", signed["read_path"].as_str().unwrap()))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(
        bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) && bytes.windows(5).any(|w| w == b"V_VP8")
    );
    assert_eq!(
        server::json(target(
            "kar_preview_navigate",
            json!({"url":"http://127.0.0.1:1/"})
        ))
        .await["result"]["isError"],
        true
    );
    let gone = server::json(target("kar_preview_close", json!({}))).await;
    assert_eq!(gone["result"]["structuredContent"]["closed"], tab);
    server::json(host.delete(format!("{terms}/{terminal}"))).await;
    server::json(host.put(&policy).json(&json!({"mode":"plan"}))).await;
}

pub(super) async fn desktop(
    url: &str,
    host: &Client,
    project: &str,
    chat: Uuid,
    rpc: impl Fn(&str, Value) -> RequestBuilder,
) {
    let policy = format!("{url}/projects/{project}/chats/{chat}/model-tools/permissions");
    let call = |name: &str, input: Value| rpc("tools/call", json!({"name":name,"arguments":input}));
    let pair = server::json(host.post(format!("{url}/access/pairings"))).await;
    let remote = Client::new();
    let device = server::json(
        remote
            .post(format!("{url}/access/pair"))
            .json(&json!({"credential":pair["credential"],"label":"preview desktop"})),
    )
    .await;
    let tab = Uuid::new_v4();
    let report = format!("{url}/projects/{project}/chats/{chat}/desktop-previews/{tab}");
    let nav = json!({"navigation":{"nav_status":{"_tag":"Success","url":"https://example.org/","title":"Shared tab"},"can_go_back":false,"can_go_forward":false}});
    assert!(
        host.put(&report)
            .json(&nav)
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    let push = || {
        remote
            .put(&report)
            .bearer_auth(device["token"].as_str().unwrap())
            .json(&nav)
    };
    assert_eq!(server::json(push()).await["runtime"], "desktop");
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"execute","preview_tools":{"tabs":[tab]}})),
    )
    .await;
    let listed = server::json(call("kar_preview_list", json!({}))).await;
    assert_eq!(
        listed["result"]["structuredContent"]["sessions"][0]["runtime"],
        "desktop"
    );
    assert_eq!(
        server::json(call(
            "kar_preview_evaluate",
            json!({"tab_id":tab,"expression":"1+1"})
        ))
        .await["result"]["isError"],
        true
    );
    assert_eq!(
        server::json(call("kar_preview_close", json!({"tab_id":tab}))).await["result"]["structuredContent"]
            ["closed"],
        tab.to_string()
    );
    let listed = server::json(call("kar_preview_list", json!({}))).await;
    assert!(
        listed["result"]["structuredContent"]["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(push().send().await.unwrap().status().is_client_error());
    server::json(host.put(&policy).json(&json!({"mode":"plan"}))).await;
}
pub(super) async fn public_page(
    url: &str,
    host: &Client,
    project: &str,
    chat: Uuid,
    rpc: impl Fn(&str, Value) -> RequestBuilder,
) {
    let policy = format!("{url}/projects/{project}/chats/{chat}/model-tools/permissions");
    let call = |name: &str, input: Value| rpc("tools/call", json!({"name":name,"arguments":input}));
    let local = "http://127.0.0.1:39427";
    server::json(
        host.put(&policy)
            .json(&json!({"mode":"execute","preview_tools":{"public_origins":[local]}})),
    )
    .await;
    let refused = server::json(call("kar_preview_open", json!({"url":format!("{local}/")}))).await;
    assert_eq!(refused["result"]["isError"], true);
    if let Ok(public) = std::env::var("KAR_MCP_PUBLIC_URL") {
        let origin = reqwest::Url::parse(&public)
            .unwrap()
            .origin()
            .ascii_serialization();
        server::json(host.put(&policy).json(
            &json!({"mode":"execute","preview_tools":{"public_origins":[origin],"html":true}}),
        ))
        .await;
        let page = server::json(call("kar_preview_open", json!({"url":public}))).await;
        let tab = &page["result"]["structuredContent"]["tab_id"];
        assert!(tab.is_string(), "{page}");
        let snapshot = server::json(call(
            "kar_preview_snapshot",
            json!({"tab_id":tab,"include_image":true}),
        ))
        .await;
        assert_eq!(snapshot["result"]["content"][1]["type"], "image");
        server::json(call("kar_preview_close", json!({"tab_id":tab}))).await;
        if let Ok(image) = std::env::var("KAR_MCP_PUBLIC_IMAGE") {
            let asset = reqwest::Url::parse(&image).unwrap();
            assert_eq!(asset.origin().ascii_serialization(), origin);
            let html = format!("<img src='{}'>", asset);
            let render = server::json(call(
                "kar_html_render",
                json!({"html":html,"title":"Public asset","height":200}),
            ))
            .await;
            let view = render["result"]["structuredContent"]["htmlRender"]["view_url_endpoint"]
                .as_str()
                .unwrap();
            let signed = server::json(host.post(format!("{url}{view}"))).await;
            let bytes = Client::new()
                .get(format!("{url}{}", signed["read_path"].as_str().unwrap()))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(bytes.contains("data:image/") && !bytes.contains(asset.as_str()));
        }
    }
    server::json(host.put(&policy).json(&json!({"mode":"plan"}))).await;
}
