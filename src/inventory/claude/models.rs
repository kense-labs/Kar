//! Claude stream-json model control; avoid running project hooks or starting MCP servers.
use crate::{
    Error, FailureClass, OpenError, Result, Runtime, SessionOptions, TurnFailure,
    account::AccountWire,
};
use serde_json::{Value, json};

pub(crate) async fn read(rt: &Runtime, opts: &SessionOptions) -> Result<Value> {
    let mut cmd = opts.command(&rt.program)?;
    crate::options::claude_environment(&mut cmd, &opts.env);
    cmd.args(&rt.args).args(&opts.launch_args).args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--settings",
        r#"{"disableAllHooks":true}"#,
        "--strict-mcp-config",
        "--mcp-config",
        r#"{"mcpServers":{}}"#,
    ]);
    let mut wire = AccountWire::open(cmd)?;
    let result = tokio::time::timeout(
        opts.handshake_timeout,
        wire.control_reply(json!({"subtype":"list_models"})),
    )
    .await;
    match result {
        Err(_) => {
            wire.close().await;
            Err(Error::Timeout("Claude list_models".into()))
        }
        Ok(reply) => {
            wire.finish().await;
            let mut reply = reply?;
            if reply["subtype"] == "success" {
                return reply["response"]
                    .get_mut("models")
                    .filter(|rows| rows.is_array())
                    .map(Value::take)
                    .ok_or_else(|| {
                        Error::Protocol("Claude list_models omitted its models array".into())
                    });
            }
            let reason = reply["error"]
                .as_str()
                .or_else(|| reply["message"].as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if [
                "login",
                "log in",
                "logged in",
                "loggedin",
                "auth",
                "api key",
            ]
            .iter()
            .any(|s| reason.contains(s))
            {
                return Err(OpenError::RuntimeFailure {
                    failure: TurnFailure::new(
                        FailureClass::Auth,
                        "Claude model listing requires authentication",
                    ),
                    code: None,
                    cause: None,
                }
                .into());
            }
            Err(Error::Refused(
                "Claude refused the list_models control".into(),
            ))
        }
    }
}
