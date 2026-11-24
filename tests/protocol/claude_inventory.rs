use super::super::{SECRET, opts};
use kar::inventory::InventoryOptions;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufRead},
};

pub(super) fn peer(case: &str, args: &[&str]) {
    assert_eq!(
        args,
        [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence"
        ]
    );
    assert!(std::env::var_os("CLAUDECODE").is_none());
    let mut polls = 0;
    for line in io::stdin().lock().lines() {
        let req: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let mut reply = json!({"subtype":"success","request_id":req["request_id"]});
        let body = match req["request"]["subtype"].as_str().unwrap() {
            "initialize" if case == "bad" => {
                json!({"commands":[{"name":"bad","aliases":"not-a-list","body":SECRET}]})
            }
            "initialize" => json!({"commands":[
                {"name":"visible","aliases":["old-filename"],"description":"A folded description (project)","argumentHint":"FILE"},
                {"name":"collision","description":"A different file (project)"},
                {"name":"shadow","description":"Duplicate one (user)"},{"name":"shadow","description":"Duplicate two (user)"},
                {"name":"hidden","description":"Only a command (user)"},{"name":"native","builtin":true,"description":"Built-in","argumentHint":""}]}),
            "get_context_usage" => {
                assert_eq!(req["request"]["detail"], "summary");
                json!({"skills":{"includedSkills":4,"totalSkills":6,"skillFrontmatter":[
                    {"name":"visible","source":"projectSettings","path":"old-filename/SKILL.md"},
                    {"name":"collision","source":"projectSettings","path":"collision.md"},
                    {"name":"shadow","source":"userSettings"},{"name":"only-model","source":"userSettings"}]}})
            }
            "mcp_status" if case == "unsupported" => {
                reply["subtype"] = "error".into();
                reply["error"] = format!("Unknown control: {SECRET}").into();
                json!({})
            }
            "mcp_status" => {
                polls += 1;
                if polls == 1 {
                    json!({"mcpServers":[{"name":"a","status":"pending"}]})
                } else {
                    json!({"mcpServers":[{"name":"a","status":"connected","scope":"user","tools":[{"name":"find","description":"A search","private":SECRET}],"config":{"env":SECRET}},{"name":"b","status":"failed","error":SECRET}]})
                }
            }
            _ => panic!("unexpected Claude request"),
        };
        reply["response"] = body;
        println!("{}", json!({"type":"control_response","response":reply}));
    }
}
pub(super) async fn exercise() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("old-filename")).unwrap();
    fs::write(
        tmp.path().join("old-filename/SKILL.md"),
        "\u{feff}---\ndescription: >-\n  A folded\n  description\n---\nPrivate body",
    )
    .unwrap();
    fs::write(
        tmp.path().join("collision.md"),
        "---\ndescription: A different file\nuser-invocable: false\n---\nPrivate",
    )
    .unwrap();
    let mut rt = kar::runtime("claude").unwrap();
    rt.program = std::env::current_exe().unwrap();
    let settings = |name, case| {
        let o = opts(tmp.path(), name, case);
        InventoryOptions {
            installation: o.installation,
            timeout: o.timeout,
        }
    };
    let opt = settings("claude", "skills_full");
    let s = serde_json::to_value(rt.skills(&opt).await.unwrap()).unwrap();
    let items = s["items"].as_array().unwrap();
    let item = |name: &str| items.iter().find(|r| r["name"] == name).unwrap();
    assert_eq!(s["partial"], true);
    assert_eq!(item("visible")["description"], "A folded description");
    assert_eq!(item("visible")["userInvocable"], true);
    for name in ["shadow", "collision"] {
        assert!(item(name).get("userInvocable").is_none());
    }
    assert_eq!(item("only-model")["userInvocable"], false);
    assert_eq!(
        item("hidden"),
        &json!({"name":"hidden","description":"Only a command","scope":"user","userInvocable":true})
    );
    let cmd = serde_json::to_value(rt.commands(&opt).await.unwrap()).unwrap();
    assert_eq!(cmd["items"][0]["argumentHint"], "FILE");
    assert_eq!(cmd["items"][0]["aliases"], json!(["old-filename"]));
    assert!(cmd["items"][5].get("argumentHint").is_none());
    let srv = serde_json::to_value(rt.mcp_servers(&opt).await.unwrap()).unwrap();
    assert_eq!(srv["partial"], true);
    assert_eq!(srv["items"][0]["toolCount"], 1);
    assert_eq!(srv["items"][1]["error"], "Native MCP connection failed");
    let tools = serde_json::to_value(rt.tools(&opt).await.unwrap()).unwrap();
    assert_eq!(
        tools["items"],
        json!([{"name":"find","description":"A search","mcpServerId":"a","source":"mcp"}])
    );
    let bad = serde_json::to_value(
        rt.commands(&settings("claude", "skills_bad"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(bad["code"], "query_failed");
    let missing = serde_json::to_value(
        rt.tools(&settings("claude", "skills_unsupported"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(missing["code"], "native_query_unavailable");
    for result in [s, cmd, srv, tools, bad, missing] {
        assert!(!result.to_string().contains(SECRET));
    }
    let mut rt = kar::runtime("grok").unwrap();
    rt.program = std::env::current_exe().unwrap();
    let opt = settings("grok", "skills_full");
    let skills = serde_json::to_value(rt.skills(&opt).await.unwrap()).unwrap();
    assert_eq!(skills["items"][0]["plugin"], "tools");
    assert_eq!(skills["items"][0]["userInvocable"], false);
    let servers = serde_json::to_value(rt.mcp_servers(&opt).await.unwrap()).unwrap();
    assert_eq!(
        servers["items"],
        json!([{"id":"example","name":"example","source":"project"}])
    );
}
