use super::*;
use cdp::Connection;
use std::path::PathBuf;
use tokio::time::{Instant, sleep};
fn invalid(msg: &str) -> Error {
    Error::Refused(msg.into())
}
fn text<'a>(q: &'a Value, k: &str) -> Result<&'a str> {
    q[k].as_str()
        .filter(|s| !s.is_empty() && s.len() <= 64000)
        .ok_or_else(|| invalid(&format!("supply {k}")))
}
async fn evaluate(c: &mut Connection, s: &str, expression: &str, wait: bool) -> Result<Value> {
    let r=c.request("Runtime.evaluate",json!({"expression":expression,"returnByValue":true,"awaitPromise":wait,"userGesture":false}),Some(s)).await?;
    if let Some(e) = r.get("exceptionDetails") {
        return Err(invalid(
            e.pointer("/exception/description")
                .or_else(|| e.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("page evaluation failed"),
        ));
    }
    Ok(r["result"]["value"].clone())
}
async fn same_page(c: &mut Connection, s: &str, grant: &Grant) -> Result<()> {
    let frame = c.request("Page.getFrameTree", json!({}), Some(s)).await?;
    let url = frame
        .pointer("/frameTree/frame/url")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("missing preview document"))?;
    let parsed = reqwest::Url::parse(url).map_err(|_| invalid("invalid preview URL"))?;
    let root =
        reqwest::Url::parse(&grant.info.url).map_err(|_| invalid("invalid preview grant"))?;
    if parsed.origin() != root.origin() {
        return Err(invalid("page is outside the granted preview origin"));
    }
    Ok(())
}
fn selector(q: &Value) -> Result<&str> {
    if q.get("selector").is_some() && q.get("locator").is_some() {
        return Err(invalid("specify one selector or locator"));
    }
    q["selector"]
        .as_str()
        .or_else(|| q["locator"].as_str())
        .filter(|s| !s.trim().is_empty() && s.len() <= 4000)
        .ok_or_else(|| invalid("supply a CSS selector or locator"))
}
async fn point(c: &mut Connection, s: &str, q: &Value) -> Result<(f64, f64)> {
    if let (Some(x), Some(y)) = (q["x"].as_f64(), q["y"].as_f64()) {
        if q.get("selector").is_some() || q.get("locator").is_some() {
            return Err(invalid("specify coordinates or a locator"));
        }
        return Ok((x, y));
    }
    let target = json!(selector(q)?);
    let expr = format!(
        "(()=>{{let e=document.querySelector({target});if(!e)throw Error('selector not found');e.scrollIntoView({{block:'center',inline:'center'}});let r=e.getBoundingClientRect();return [r.x+r.width/2,r.y+r.height/2];}})()"
    );
    let pt = evaluate(c, s, &expr, false).await?;
    Ok((
        pt[0].as_f64().ok_or_else(|| invalid("missing x"))?,
        pt[1].as_f64().ok_or_else(|| invalid("missing y"))?,
    ))
}
async fn mouse(
    c: &mut Connection,
    s: &str,
    event: &str,
    pt: (f64, f64),
    pressed: bool,
) -> Result<()> {
    let mut body = json!({"type":event,"x":pt.0,"y":pt.1,"buttons":if pressed{1}else{0}});
    if event != "mouseMoved" {
        body["button"] = "left".into();
        body["clickCount"] = 1.into();
    }
    c.request("Input.dispatchMouseEvent", body, Some(s)).await?;
    Ok(())
}
async fn act(c: &mut Connection, s: &str, q: &Value) -> Result<Value> {
    match text(q, "action")? {
        "click" | "hover" => {
            let p = point(c, s, q).await?;
            mouse(c, s, "mouseMoved", p, false).await?;
            if q["action"] == "click" {
                mouse(c, s, "mousePressed", p, true).await?;
                mouse(c, s, "mouseReleased", p, false).await?;
            }
        }
        "drag" => {
            let a = point(c, s, &json!({"locator":text(q,"source")?})).await?;
            let b = point(c, s, &json!({"locator":text(q,"target")?})).await?;
            mouse(c, s, "mouseMoved", a, false).await?;
            mouse(c, s, "mousePressed", a, true).await?;
            mouse(
                c,
                s,
                "mouseMoved",
                ((a.0 + b.0) / 2., (a.1 + b.1) / 2.),
                true,
            )
            .await?;
            mouse(c, s, "mouseMoved", b, true).await?;
            mouse(c, s, "mouseReleased", b, false).await?;
        }
        "type" | "press" => {
            if q.get("selector").is_some() || q.get("locator").is_some() {
                let sel = json!(selector(q)?);
                evaluate(c,s,&format!("(()=>{{const e=document.querySelector({sel});if(!e)throw Error('selector not found');e.focus()}})()"),false).await?;
            }
            if q["action"] == "type" {
                c.request(
                    "Input.insertText",
                    json!({"text":q["text"].as_str().ok_or_else(||invalid("supply text"))?}),
                    Some(s),
                )
                .await?;
            } else {
                let key = text(q, "key")?;
                let code = match key {
                    "Enter" => 13,
                    "Tab" => 9,
                    "Escape" => 27,
                    "Backspace" => 8,
                    "Delete" => 46,
                    "ArrowLeft" => 37,
                    "ArrowUp" => 38,
                    "ArrowRight" => 39,
                    "ArrowDown" => 40,
                    " " => 32,
                    _ if key.len() == 1 => key.to_ascii_uppercase().as_bytes()[0] as u32,
                    _ => return Err(invalid("unsupported key")),
                };
                for ty in ["keyDown", "keyUp"] {
                    let mut d = json!({"type":ty,"key":key,"windowsVirtualKeyCode":code});
                    if ty == "keyDown" && (key.len() == 1 || key == "Enter") {
                        d["text"] = json!(if key == "Enter" { "\r" } else { key });
                    }
                    c.request("Input.dispatchKeyEvent", d, Some(s)).await?;
                }
            }
        }
        "scroll" => {
            let dx = q["delta_x"].as_f64();
            let dy = q["delta_y"].as_f64();
            if dx.is_none() && dy.is_none() {
                return Err(invalid("supply delta_x or delta_y"));
            }
            let p = if q.get("selector").is_some()
                || q.get("locator").is_some()
                || q.get("x").is_some()
            {
                point(c, s, q).await?
            } else {
                (1., 1.)
            };
            c.request("Input.dispatchMouseEvent",json!({"type":"mouseWheel","x":p.0,"y":p.1,"deltaX":dx.unwrap_or(0.),"deltaY":dy.unwrap_or(0.)}),Some(s)).await?;
        }
        "select" => {
            let values: Vec<String> = serde_json::from_value(q["values"].clone())?;
            if values.len() > 100 {
                return Err(invalid("too many options"));
            }
            let sel = json!(selector(q)?);
            let values = json!(values);
            let expr = format!(
                "(()=>{{const e=document.querySelector({sel});if(!(e instanceof HTMLSelectElement))throw Error('select not found');const v={values};if(v.some(k=>!Array.from(e.options).some(o=>o.value===k)))throw Error('option not found');for(const o of e.options)o.selected=v.includes(o.value);e.dispatchEvent(new Event('input',{{bubbles:true}}));e.dispatchEvent(new Event('change',{{bubbles:true}}));return Array.from(e.selectedOptions,o=>o.value)}})()"
            );
            return Ok(json!({"selected":evaluate(c,s,&expr,false).await?}));
        }
        _ => return Err(invalid("unknown page action")),
    }
    Ok(json!({"performed":q["action"]}))
}
impl Pool {
    pub(in super::super) async fn model_status(
        self: &Arc<Self>,
        grant: Arc<Grant>,
    ) -> Result<Value> {
        let handle = self.bind(&grant);
        let mut slot = handle.lock().await;
        let host = slot.attach(&grant).await?;
        let tab = &host.tabs[&grant.info.id];
        let info = host
            .browser
            .conn
            .request("Target.getTargetInfo", json!({"targetId":tab.target}), None)
            .await?;
        Ok(
            json!({"tab_id":grant.info.id,"preview":grant.snapshot(),"title":info["targetInfo"]["title"],"url":info["targetInfo"]["url"],"pending_dialog":host.browser.conn.dialogs.get(&tab.session)}),
        )
    }

    pub(in super::super) async fn upload(
        self: &Arc<Self>,
        grant: Arc<Grant>,
        q: &Value,
        dir: tempfile::TempDir,
    ) -> Result<Value> {
        let handle = self.bind(&grant);
        let mut slot = handle.lock().await;
        let h = slot.attach(&grant).await?;
        let tab = h.tabs.get_mut(&grant.info.id).unwrap();
        let s = tab.session.as_str();
        let c = &mut h.browser.conn;
        same_page(c, s, &grant).await?;
        let doc = c.request("DOM.getDocument", json!({}), Some(s)).await?;
        let node = c
            .request(
                "DOM.querySelector",
                json!({"nodeId":doc["root"]["nodeId"],"selector":selector(q)?}),
                Some(s),
            )
            .await?;
        if node["nodeId"].as_u64().unwrap_or(0) == 0 {
            return Err(invalid("upload field not found"));
        }
        let paths: Vec<PathBuf> = serde_json::from_value(q["paths"].clone())?;
        c.request(
            "DOM.setFileInputFiles",
            json!({"nodeId":node["nodeId"],"files":paths}),
            Some(s),
        )
        .await?;
        tab.uploads.push(dir);
        Ok(json!({"uploaded":paths.len()}))
    }
    pub(in super::super) async fn action(
        self: &Arc<Self>,
        grant: Arc<Grant>,
        name: &str,
        q: &Value,
    ) -> Result<Value> {
        let handle = self.bind(&grant);
        let mut slot = handle.lock().await;
        let h = slot.attach(&grant).await?;
        let tab = &h.tabs[&grant.info.id];
        let s = tab.session.as_str();
        let c = &mut h.browser.conn;
        if name == "kar_preview_dialog" {
            c.request("Page.handleJavaScriptDialog",json!({"accept":q["accept"].as_bool().ok_or_else(||invalid("supply accept"))?,"promptText":q["prompt_text"].as_str().unwrap_or_default()}),Some(s)).await?;
            return Ok(json!({"tab_id":grant.info.id,"accepted":q["accept"]}));
        }
        if name == "kar_preview_navigate" {
            let root = reqwest::Url::parse(&grant.info.url).unwrap();
            let target = root
                .join(text(q, "url")?)
                .map_err(|_| invalid("invalid page URL"))?;
            if target.origin() != root.origin()
                || !target.username().is_empty()
                || target.password().is_some()
            {
                return Err(invalid("navigation is outside the granted preview origin"));
            }
            let r = c
                .request("Page.navigate", json!({"url":target.as_str()}), Some(s))
                .await?;
            if let Some(e) = r["errorText"].as_str() {
                return Err(invalid(e));
            }
            grant.native_status(
                json!({"_tag":"Loading","url":target.as_str(),"title":""}),
                false,
                false,
            );
            return Ok(
                json!({"tab_id":grant.info.id,"url":target.as_str(),"loader_id":r["loaderId"]}),
            );
        }
        same_page(c, s, &grant).await?;
        match name {
            "kar_preview_evaluate" => Ok(
                json!({"value":evaluate(c,s,text(q,"expression")?,q["await_promise"].as_bool().unwrap_or(true)).await?}),
            ),
            "kar_preview_act" => act(c, s, q).await,
            "kar_preview_snapshot" => {
                let r=evaluate(c,s,r#"(()=>({url:location.href,title:document.title,text:(document.body?.innerText||'').slice(0,32000),width:innerWidth,height:innerHeight,elements:Array.from(document.querySelectorAll('a,button,input,textarea,select,[role=button]')).filter(e=>e.getClientRects().length).slice(0,150).map(e=>({tag:e.localName,role:e.getAttribute('role'),text:(e.getAttribute('aria-label')||e.innerText||e.getAttribute('placeholder')||'').slice(0,200),selector:e.id?'#'+CSS.escape(e.id):null}))}))()"#,false).await?;
                Ok(
                    json!({"tab_id":grant.info.id,"page":r,"screenshot":{"width":r["width"],"height":r["height"]}}),
                )
            }
            "kar_preview_wait_for" => {
                let selector = q["locator"].as_str().or(q["selector"].as_str());
                if [selector, q["text"].as_str(), q["url_includes"].as_str()]
                    .iter()
                    .all(|v| v.is_none_or(str::is_empty))
                {
                    return Err(invalid("at least one wait condition is required"));
                }
                let params = json!([selector, q["text"], q["url_includes"]]);
                let expr = format!(
                    "(([s,t,u])=>(!s||!!document.querySelector(s))&&(!t||document.body.innerText.includes(t))&&(!u||location.href.includes(u)))({params})"
                );
                let deadline = Instant::now()
                    + Duration::from_millis(
                        q["timeout_ms"].as_u64().unwrap_or(5000).clamp(1, 30000),
                    );
                loop {
                    if *grant.revocation().borrow() {
                        return Err(invalid("preview was closed"));
                    }
                    if evaluate(c, s, &expr, false).await? == true {
                        return Ok(json!({"matched":true,"tab_id":grant.info.id}));
                    }
                    if Instant::now() >= deadline {
                        return Err(Error::Timeout("preview wait condition".into()));
                    }
                    sleep(Duration::from_millis(60)).await;
                }
            }
            _ => Err(invalid("unknown Chromium operation")),
        }
    }
    pub(super) async fn recording_frame(self: &Arc<Self>, grant: &Arc<Grant>) -> Result<Vec<u8>> {
        let handle = self.bind(grant);
        let mut slot = handle.lock().await;
        let h = slot.attach(grant).await?;
        let s = &h.tabs[&grant.info.id].session;
        same_page(&mut h.browser.conn, s, grant).await?;
        let img = h
            .browser
            .conn
            .request(
                "Page.captureScreenshot",
                json!({"format":"webp","quality":75,"fromSurface":true}),
                Some(s),
            )
            .await?;
        STANDARD
            .decode(
                img["data"]
                    .as_str()
                    .ok_or_else(|| invalid("Chromium omitted recording frame"))?,
            )
            .map_err(|_| invalid("invalid recording frame"))
    }
}
