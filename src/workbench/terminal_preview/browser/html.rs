//! Measure an isolated page in a real sandboxed browser with its own ephemeral profile.
use super::cdp::Browser;
use crate::{Error, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::sync::LazyLock;
static BROWSERS: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(2));
pub(in crate::workbench) async fn capture(
    html: &str,
    widths: &[u32],
    dark: bool,
    preview: bool,
) -> Result<Value> {
    let _permit = BROWSERS
        .acquire()
        .await
        .map_err(|e| Error::Closed(e.to_string()))?;
    let dir = tempfile::tempdir()?;
    let mut browser = Browser::open(dir.path()).await?;
    let c = &mut browser.conn;
    c.observe_console = preview;
    let target = c
        .request("Target.createTarget", json!({"url":"about:blank"}), None)
        .await?;
    let tab = c
        .request(
            "Target.attachToTarget",
            json!({"targetId":target["targetId"],"flatten":true}),
            None,
        )
        .await?;
    let sid = Some(
        tab["sessionId"]
            .as_str()
            .ok_or_else(|| Error::Protocol("missing HTML browser session".into()))?,
    );
    c.request("Page.enable", json!({}), sid).await?;
    c.request("Runtime.enable", json!({}), sid).await?;
    c.request(
        "Emulation.setEmulatedMedia",
        json!({"features":[{"name":"prefers-color-scheme","value":if dark{"dark"}else{"light"}}]}),
        sid,
    )
    .await?;
    let url = format!(
        "data:text/html;charset=utf-8;base64,{}",
        STANDARD.encode(html)
    );
    let mut heights = Vec::new();
    for (index, &width) in widths.iter().enumerate() {
        c.request(
            "Emulation.setDeviceMetricsOverride",
            json!({"width":width,"height":800,"deviceScaleFactor":1,"mobile":false}),
            sid,
        )
        .await?;
        if index == 0 {
            let load = c.request("Page.navigate", json!({"url":url}), sid).await?;
            if load.get("errorText").is_some() {
                return Err(Error::Refused(
                    "Chromium could not load the HTML page".into(),
                ));
            }
        }
        let measured=c.request("Runtime.evaluate",json!({"expression":r#"(async()=>{if(!location.href.startsWith('data:text/html'))return null;if(document.readyState!=='complete')await new Promise(r=>addEventListener('load',r,{once:true}));await document.fonts.ready;await new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(r)));return Math.ceil((document.documentElement.scrollHeight>document.documentElement.clientHeight?document.documentElement.scrollHeight:document.documentElement.getBoundingClientRect().height));})()"#,"awaitPromise":true,"returnByValue":true}),sid).await?;
        let height = measured
            .pointer("/result/value")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                Error::Protocol("Chromium omitted the measured content height".into())
            })?;
        heights.push([width as u64, height]);
    }
    if preview {
        let [width, height] = heights[0];
        let captured = height.clamp(1, 4000);
        let shot=c.request("Page.captureScreenshot",json!({"format":"png","captureBeyondViewport":true,"clip":{"x":0,"y":0,"width":width,"height":captured,"scale":1}}),sid).await?;
        let data = shot["data"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Protocol("Chromium returned no PNG".into()))?;
        Ok(
            json!({"width":width,"contentHeight":height,"capturedHeight":captured,"consoleMessages":c.console,"screenshot":{"mimeType":"image/png","data":data,"width":width,"height":captured}}),
        )
    } else {
        Ok(json!(heights))
    }
}
