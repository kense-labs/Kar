//! Session-scoped capability query for DSH Web. Its model picker omits input modalities.
mod node_api;
use node_api::{Env, Handle, Read};
use std::{ffi::CStr, ptr::null_mut};

type Callback = extern "C" fn(Handle, Handle) -> Handle;

fn bound(e: &Env, state: Handle, name: &CStr, callback: Callback) -> Read<Handle> {
    e.call(e.function(name, callback)?, c"bind", &[state])
}

fn response(e: &Env, value: Handle) -> Read<Handle> {
    e.call(e.get(e.global()?, c"Response")?, c"json", &[value])
}

fn run(env: Handle, f: impl FnOnce(&Env) -> Read<Handle>) -> Handle {
    let e = Env(env);
    f(&e).unwrap_or_else(|_| {
        e.clear_exception();
        e.global()
            .and_then(|g| e.call(g, c"Object", &[]))
            .and_then(|empty| response(&e, empty))
            .unwrap_or(null_mut())
    })
}

extern "C" fn unavailable(env: Handle, _: Handle) -> Handle {
    run(env, |_| Err("image route unavailable"))
}
extern "C" fn resolved(env: Handle, info: Handle) -> Handle {
    run(env, |e| {
        let (limits, [model]) = e.args(info)?;
        let yes = e.call(
            e.get(model, c"inputModalities")?,
            c"includes",
            &[e.text("image")?],
        )?;
        if e.json(yes)? != b"true" {
            return Err("model is text-only");
        }
        response(e, limits)
    })
}
extern "C" fn query(env: Handle, info: Handle) -> Handle {
    run(env, |e| {
        let (ctx, [body]) = e.args(info)?;
        let service = |name| e.call(ctx, c"get", &[e.text(name)?]);
        let attachments = service("attachments")?;
        if e.kind(e.get(attachments, c"saveImages")?)? != 7 {
            return Err("no native image store");
        }
        let limits = e.get(attachments, c"imageLimits")?;
        let agent = e.call(service("agents")?, c"get", &[e.get(body, c"sessionId")?])?;
        let mut route = e.get(body, c"fallback")?;
        if e.kind(agent)? == 6 {
            let model = e.get(agent, c"model")?;
            if e.kind(model)? == 6 && e.kind(e.get(model, c"current")?)? == 7 {
                let current = e.call(model, c"current", &[])?;
                if e.kind(current)? == 6 {
                    route = current;
                }
            }
        }
        if e.kind(route)? != 6 {
            route = e.call(service("agentDefaultModel")?, c"currentSelection", &[])?;
        }
        let result = e.call(
            service("llm")?,
            c"resolveModelInfo",
            &[e.get(route, c"provider")?, e.get(route, c"model")?],
        )?;
        let promise = e.call(e.get(e.global()?, c"Promise")?, c"resolve", &[result])?;
        e.call(
            promise,
            c"then",
            &[
                bound(e, limits, c"imageLimits", resolved)?,
                e.function(c"unavailable", unavailable)?,
            ],
        )
    })
}
extern "C" fn fetch(env: Handle, info: Handle) -> Handle {
    run(env, |e| {
        let (ctx, [request]) = e.args(info)?;
        e.call(
            e.call(request, c"json", &[])?,
            c"then",
            &[
                bound(e, ctx, c"queryImageTypes", query)?,
                e.function(c"invalid", unavailable)?,
            ],
        )
    })
}
extern "C" fn register(env: Handle, info: Handle) -> Handle {
    run(env, |e| {
        let (ctx, []) = e.args(info)?;
        let definition = e.call(e.global()?, c"Object", &[])?;
        e.set(definition, c"path", e.text("/api/kar/imageTypes")?)?;
        e.set(
            definition,
            c"methods",
            e.call(e.global()?, c"Array", &[e.text("POST")?])?,
        )?;
        e.set(definition, c"requestBody", e.text("buffered")?)?;
        e.set(definition, c"fetch", bound(e, ctx, c"imageTypes", fetch)?)?;
        let connection = e.call(ctx, c"get", &[e.text("connection")?])?;
        e.call(e.get(connection, c"fetch")?, c"register", &[definition])
    })
}
extern "C" fn apply(env: Handle, info: Handle) -> Handle {
    run(env, |e| {
        let (_, [ctx]) = e.args(info)?;
        e.call(
            ctx,
            c"effect",
            &[
                bound(e, ctx, c"registerImageTypes", register)?,
                e.text("Kar image input capability")?,
            ],
        )?;
        Ok(null_mut())
    })
}

#[unsafe(no_mangle)]
extern "C" fn napi_register_module_v1(env: Handle, exports: Handle) -> Handle {
    run(env, |e| {
        let deps = e.call(e.global()?, c"Array", &[])?;
        for name in [
            "connection",
            "agents",
            "llm",
            "attachments",
            "agentDefaultModel",
        ] {
            e.call(deps, c"push", &[e.text(name)?])?;
        }
        e.set(exports, c"inject", deps)?;
        e.set(exports, c"apply", e.function(c"apply", apply)?)?;
        Ok(exports)
    })
}
