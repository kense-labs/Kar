//! DSH's Cordis services can list and resolve model routes without creating an agent or session.
use super::*;
use std::ffi::CStr;

fn obj(e: &Env) -> Read<Handle> {
    e.call(e.global()?, c"Object", &[])
}
fn bound(e: &Env, state: Handle, cb: extern "C" fn(Handle, Handle) -> Handle) -> Read<Handle> {
    e.call(e.function(c"catalogCallback", cb)?, c"bind", &[state])
}
fn all(e: &Env, items: Handle) -> Read<Handle> {
    e.call(e.get(e.global()?, c"Promise")?, c"all", &[items])
}
fn settled(e: &Env, value: Handle) -> Read<Handle> {
    e.call(e.get(e.global()?, c"Promise")?, c"resolve", &[value])
}
fn prop(e: &Env, from: Handle, to: Handle, key: &CStr) -> Read<()> {
    e.set(to, key, e.get(from, key)?)
}
fn transform(env: Handle, f: impl FnOnce(&Env) -> Read<Handle>) -> Handle {
    let e = Env(env);
    f(&e).unwrap_or_else(|_| {
        e.clear_exception();
        e.global()
            .and_then(|g| e.get(g, c"Promise"))
            .and_then(|p| e.call(p, c"reject", &[]))
            .unwrap_or(null_mut())
    })
}
extern "C" fn detail(env: Handle, info: Handle) -> Handle {
    transform(env, |e| {
        let (row, [resolved]) = e.args(info)?;
        prop(e, resolved, row, c"reasoning")?;
        Ok(row)
    })
}
extern "C" fn model(env: Handle, info: Handle) -> Handle {
    transform(env, |e| {
        let (state, [model]) = e.args(info)?;
        let row = obj(e)?;
        prop(e, model, row, c"id")?;
        prop(e, model, row, c"name")?;
        let p = e.call(
            e.get(state, c"llm")?,
            c"resolveModelInfo",
            &[e.get(e.get(state, c"row")?, c"id")?, e.get(model, c"id")?],
        )?;
        e.call(settled(e, p)?, c"then", &[bound(e, row, detail)?])
    })
}
extern "C" fn entries(env: Handle, info: Handle) -> Handle {
    transform(env, |e| {
        let (state, [models]) = e.args(info)?;
        all(e, e.call(models, c"map", &[bound(e, state, model)?])?)
    })
}
extern "C" fn listed(env: Handle, info: Handle) -> Handle {
    transform(env, |e| {
        let (row, [models]) = e.args(info)?;
        e.set(row, c"models", models)?;
        Ok(row)
    })
}
extern "C" fn unavailable(env: Handle, info: Handle) -> Handle {
    transform(env, |e| {
        let (row, [_]) = e.args(info)?;
        e.set(row, c"failure", e.text("provider query failed")?)?;
        Ok(row)
    })
}
extern "C" fn provider(env: Handle, info: Handle) -> Handle {
    transform(env, |e| {
        let (llm, [provider]) = e.args(info)?;
        let row = obj(e)?;
        prop(e, provider, row, c"id")?;
        prop(e, provider, row, c"name")?;
        let state = obj(e)?;
        e.set(state, c"row", row)?;
        e.set(state, c"llm", llm)?;
        let p = match e.call(llm, c"listModels", &[e.get(row, c"id")?]) {
            Ok(p) => p,
            Err(_) => {
                e.clear_exception();
                e.set(row, c"failure", e.text("provider query failed")?)?;
                return Ok(row);
            }
        };
        let p = e.call(settled(e, p)?, c"then", &[bound(e, state, entries)?])?;
        e.call(
            p,
            c"then",
            &[bound(e, row, listed)?, bound(e, row, unavailable)?],
        )
    })
}
extern "C" fn emit(env: Handle, info: Handle) -> Handle {
    callback(env, |e| {
        let (result, [rows]) = e.args(info)?;
        e.set(result, c"providers", rows)?;
        packet(&e.json_with_limit(result, 8 * 1024 * 1024)?);
        Ok(())
    })
}
pub(super) fn read(e: &Env, ctx: Handle) -> Read<()> {
    let llm = e.call(ctx, c"get", &[e.text("llm")?])?;
    if e.kind(llm)? != 6 {
        return Err("native_query_unavailable");
    }
    for name in [c"listProviders", c"listModels", c"resolveModelInfo"] {
        if e.kind(e.get(llm, name)?)? != 7 {
            return Err("native_query_unavailable");
        }
    }
    let defaults = e.call(ctx, c"get", &[e.text("agentDefaultModel")?])?;
    let result = obj(e)?;
    if e.kind(defaults)? == 6 && e.kind(e.get(defaults, c"currentSelection")?)? == 7 {
        e.set(
            result,
            c"default",
            e.call(defaults, c"currentSelection", &[])?,
        )?;
    }
    let providers = e.call(llm, c"listProviders", &[])?;
    let p = all(e, e.call(providers, c"map", &[bound(e, llm, provider)?])?)?;
    e.then(p, bound(e, result, emit)?)
}
