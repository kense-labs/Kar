use js_sys::{Array, Function, Promise, Reflect};
use serde_json::Value;
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::{JsFuture, future_to_promise};

pub(super) type JsResult<T> = std::result::Result<T, JsValue>;
#[wasm_bindgen(module = "@earendil-works/chord/context")]
extern "C" {
    #[wasm_bindgen(thread_local_v2,js_name=BACKGROUND_CONTEXT)]
    static BACKGROUND: JsValue;
}
#[wasm_bindgen(module = "@earendil-works/pi-durable")]
extern "C" {
    #[wasm_bindgen(thread_local_v2,js_name=AgentDoc)]
    static AGENT_DOC: JsValue;
    #[wasm_bindgen(thread_local_v2,js_name=LiveDoc)]
    static LIVE_DOC: JsValue;
    #[wasm_bindgen(catch,js_name=configure)]
    fn configure_native(tx: &JsValue, id: &JsValue, change: &JsValue) -> JsResult<Promise>;
}
#[wasm_bindgen(module = "@earendil-works/pi-ai/models")]
extern "C" {
    #[wasm_bindgen(catch,js_name=getSupportedThinkingLevels)]
    fn levels(model: &JsValue) -> JsResult<Array>;
}
pub(super) fn error(text: impl AsRef<str>) -> JsValue {
    js_sys::Error::new(text.as_ref()).into()
}
fn missing_model(selected: &str) -> JsValue {
    let e = js_sys::Error::new(&format!("unregistered Pi Durable model {selected}"));
    e.set_name("RuntimeFailureError");
    if let Err(error) = set(&e, "failure", &"model_unavailable".into()) {
        return error;
    }
    e.into()
}
pub(super) fn get(object: &JsValue, key: &str) -> JsResult<JsValue> {
    Reflect::get(object, &key.into())
}
pub(super) fn set(object: &JsValue, key: &str, v: &JsValue) -> JsResult<()> {
    Reflect::set(object, &key.into(), v).map(drop)
}
pub(super) fn ctx() -> JsValue {
    BACKGROUND.with(Clone::clone)
}
pub(super) fn js(v: &Value) -> JsResult<JsValue> {
    js_sys::JSON::parse(&v.to_string())
}
pub(super) fn json(v: &JsValue) -> JsResult<Value> {
    if v.is_undefined() {
        return Ok(Value::Null);
    }
    let text = js_sys::JSON::stringify(v)?
        .as_string()
        .ok_or_else(|| error("native value is not JSON"))?;
    serde_json::from_str(&text).map_err(|e| error(e.to_string()))
}
pub(super) fn call(obj: &JsValue, method: &str, args: &[JsValue]) -> JsResult<JsValue> {
    let f = get(obj, method)?
        .dyn_into::<Function>()
        .map_err(|_| error(format!("host object has no method {method}")))?;
    let array = Array::new();
    for arg in args {
        array.push(arg);
    }
    f.apply(obj, &array)
}
pub(super) async fn invoke(obj: &JsValue, method: &str, args: &[JsValue]) -> JsResult<JsValue> {
    JsFuture::from(Promise::resolve(&call(obj, method, args)?)).await
}
pub(super) fn summary(e: &JsValue) -> Value {
    serde_json::json!({"name":get(e,"name").ok().and_then(|s|s.as_string()),"message":get(e,"message").ok().and_then(|s|s.as_string()).or_else(||e.as_string())})
}
pub(super) async fn lookup(h: &JsValue, id: &JsValue, input: String) -> JsResult<JsValue> {
    let conv = id.clone();
    let read = Closure::once(move |tx: JsValue| -> JsResult<JsValue> {
        call(&tx, "submissionByRequest", &[conv, input.into()])
    });
    invoke(h, "commit", &[read.as_ref().clone(), ctx()]).await
}

pub(super) async fn running(conv: &JsValue, id: &JsValue) -> JsResult<bool> {
    let id = id.clone();
    let read = Closure::once(move |tx: JsValue| {
        future_to_promise(async move {
            let live = invoke(&tx, "doc", &[LIVE_DOC.with(Clone::clone), id]).await?;
            let run = get(&live, "run")?;
            Ok((!run.is_null() && !run.is_undefined()).into())
        })
    });
    let value = invoke(conv, "commit", &[read.as_ref().clone(), ctx()]).await?;
    Ok(value.is_truthy())
}
pub(super) fn catalog(models: &JsValue) -> JsResult<JsValue> {
    if let Some(f) = models.dyn_ref::<Function>() {
        f.call0(&JsValue::UNDEFINED)
    } else {
        Ok(models.clone())
    }
}
pub(super) fn effort_valid(
    agent: &JsValue,
    models: &JsValue,
    effort: Option<&str>,
) -> JsResult<()> {
    let Some(effort) = effort else { return Ok(()) };
    let selected = get(agent, "model")?;
    if selected.is_null() || selected.is_undefined() {
        return Err(error("the conversation has no model for this effort"));
    }
    let model = call(
        models,
        "getModel",
        &[get(&selected, "provider")?, get(&selected, "modelId")?],
    )?;
    if model.is_undefined() || !levels(&model)?.includes(&effort.into(), 0) {
        return Err(error(format!("model does not support effort {effort}")));
    }
    Ok(())
}
pub(super) async fn conversation(
    h: &JsValue,
    models: &JsValue,
    o: &super::Options,
) -> JsResult<JsValue> {
    let mut change = serde_json::json!({"cwd":o.cwd});
    if let Some(selected) = &o.model {
        let (p, id) = selected
            .split_once('/')
            .filter(|(p, id)| !p.is_empty() && !id.is_empty())
            .ok_or_else(|| missing_model(selected))?;
        let model = call(models, "getModel", &[p.into(), id.into()])?;
        if model.is_null() || model.is_undefined() {
            return Err(missing_model(selected));
        }
        change["model"] = serde_json::json!({"provider":p,"modelId":id});
    }
    if let Some(effort) = &o.effort {
        change["thinkingLevel"] = effort.clone().into();
    }
    if let Some(prompt) = &o.append_system_prompt {
        change["instructions"] = prompt.clone().into();
    }
    let models = models.clone();
    let effort = o.effort.clone();
    let conv = if let Some(resume) = &o.resume {
        let n = resume
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0 && *n <= 9_007_199_254_740_991 && n.to_string() == *resume)
            .ok_or_else(|| error("expected a positive canonical conversation id"))?;
        let id = JsValue::from_f64(n as f64);
        let conv = invoke(h, "conversation", &[id.clone(), ctx()]).await?;
        if conv.is_undefined() {
            let e = js_sys::Error::new(&format!("unknown Pi Durable conversation {resume}"));
            e.set_name("SessionNotFoundError");
            set(&e, "sessionId", &resume.as_str().into())?;
            set(
                &e,
                "cause",
                &js(&serde_json::json!({"method":"Harness.conversation","native":{"id":n}}))?,
            )?;
            return Err(e.into());
        }
        if conv.is_null() {
            return Err(error("Pi Durable returned an invalid conversation handle"));
        }
        let transaction = Closure::once(move |tx: JsValue| {
            future_to_promise(async move {
                let old = invoke(&tx, "doc", &[AGENT_DOC.with(Clone::clone), id.clone()]).await?;
                let live = invoke(&tx, "doc", &[LIVE_DOC.with(Clone::clone), id.clone()]).await?;
                let old = json(&old)?;
                if !get(&live, "run")?.is_undefined()
                    && change
                        .as_object()
                        .unwrap()
                        .iter()
                        .any(|(k, v)| old[k] != *v)
                {
                    return Err(error("cannot reconfigure an active Pi Durable run"));
                }
                JsFuture::from(configure_native(&tx, &id, &js(&change)?)?).await?;
                let after = invoke(&tx, "doc", &[AGENT_DOC.with(Clone::clone), id]).await?;
                effort_valid(&after, &models, effort.as_deref())?;
                Ok(JsValue::UNDEFINED)
            })
        });
        invoke(&conv, "commit", &[transaction.as_ref().clone(), ctx()]).await?;
        conv
    } else {
        let opts = js(&serde_json::json!({"ownership":{"kind":"ownerless"},"agent":change}))?;
        let init = Closure::once(move |tx: JsValue, id: JsValue| {
            future_to_promise(async move {
                let agent = invoke(&tx, "doc", &[AGENT_DOC.with(Clone::clone), id]).await?;
                effort_valid(&agent, &models, effort.as_deref())?;
                Ok(JsValue::UNDEFINED)
            })
        });
        set(&opts, "init", init.as_ref())?;
        invoke(h, "createConversation", &[opts, ctx()]).await?
    };
    let agent = json(&invoke(&conv, "agent", &[ctx()]).await?)?;
    if let Some(model) = &o.model {
        let reported = agent["model"]["provider"]
            .as_str()
            .zip(agent["model"]["modelId"].as_str())
            .map(|(p, m)| format!("{p}/{m}"));
        if reported.as_deref() != Some(model) {
            let e = js_sys::Error::new(&format!("Pi Durable did not confirm model {model:?}"));
            e.set_name("ModelConfirmationError");
            set(&e, "requested", &JsValue::from_str(model))?;
            set(
                &e,
                "reported",
                &reported.map_or(JsValue::NULL, |m| JsValue::from_str(&m)),
            )?;
            return Err(e.into());
        }
    }
    if o.effort
        .as_ref()
        .is_some_and(|effort| agent["thinkingLevel"] != *effort)
    {
        return Err(error("Pi Durable selected a different effort"));
    }
    Ok(conv)
}
