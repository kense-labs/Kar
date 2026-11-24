//! Keep the installed SDK's ModelRuntime alive across scoped refreshes. Only
//! the upstream public catalog fields cross the parent process pipe.
use super::*;

pub(super) fn open(e: &Env, state: Handle, runtime: Handle) -> Read<Handle> {
    e.set(state, c"runtime", runtime)?;
    let process = e.get(e.global()?, c"process")?;
    let readline = e.call(process, c"getBuiltinModule", &[e.text("node:readline")?])?;
    let options = obj(e)?;
    e.set(options, c"input", e.get(process, c"stdin")?)?;
    let reader = e.call(readline, c"createInterface", &[options])?;
    e.call(
        reader,
        c"on",
        &[
            e.text("line")?,
            bound(e, state, c"catalogCommand", command)?,
        ],
    )?;
    e.call(
        reader,
        c"on",
        &[e.text("close")?, bound(e, state, c"catalogClose", close)?],
    )?;
    snapshot(e, state)
}
fn snapshot(e: &Env, state: Handle) -> Read<Handle> {
    let p = e.call(e.get(state, c"runtime")?, c"listCredentials", &[])?;
    on_promise(e, state, p, publish)
}
extern "C" fn credential_id(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (_, [entry]) = e.args(info)?;
        e.get(entry, c"providerId")
    })
}
extern "C" fn project_model(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [model]) = e.args(info)?;
        let id = e.get(model, c"provider")?;
        let seen = e.get(state, c"seen")?;
        if e.json(e.call(seen, c"has", &[id])?)? == b"false" {
            e.call(seen, c"add", &[id])?;
            let runtime = e.get(state, c"runtime")?;
            let provider = obj(e)?;
            e.set(provider, c"id", id)?;
            let native = e.call(runtime, c"getProvider", &[id])?;
            let name = if e.kind(native)? == 6 {
                e.get(native, c"name")?
            } else {
                id
            };
            e.set(provider, c"name", name)?;
            let stored = e.call(e.get(state, c"stored")?, c"has", &[id])?;
            let configured = if e.json(stored)? == b"true" {
                stored
            } else {
                e.get(
                    e.call(runtime, c"getProviderAuthStatus", &[id])?,
                    c"configured",
                )?
            };
            e.set(provider, c"configured", configured)?;
            let opts = obj(e)?;
            e.set(opts, c"modelRuntime", runtime)?;
            e.set(opts, c"cliProvider", id)?;
            let resolved = e.call(e.get(state, c"package")?, c"resolveCliModel", &[opts])?;
            let default = e.get(resolved, c"model")?;
            e.set(
                provider,
                c"defaultModel",
                e.get(
                    if e.kind(default)? == 6 {
                        default
                    } else {
                        model
                    },
                    c"id",
                )?,
            )?;
            e.call(e.get(state, c"providers")?, c"push", &[provider])?;
        }
        let row = obj(e)?;
        for key in [
            c"id",
            c"baseUrl",
            c"contextWindow",
            c"maxTokens",
            c"reasoning",
            c"input",
        ] {
            prop(e, model, row, key)?;
        }
        e.set(row, c"providerId", id)?;
        e.set(row, c"wire", e.get(model, c"api")?)?;
        Ok(row)
    })
}
extern "C" fn publish(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [credentials]) = e.args(info)?;
        let ids = e.call(
            credentials,
            c"map",
            &[e.function(c"credentialId", credential_id)?],
        )?;
        e.set(state, c"stored", construct(e, c"Set", &[ids])?)?;
        e.set(state, c"seen", construct(e, c"Set", &[])?)?;
        let providers = e.call(e.global()?, c"Array", &[])?;
        e.set(state, c"providers", providers)?;
        let all = e.call(e.get(state, c"runtime")?, c"getModels", &[])?;
        let count = node_api::arrays::len(e, all)?;
        let mapper = bound(e, state, c"projectModel", project_model)?;
        for start in (0..count).step_by(128) {
            let end = start.saturating_add(128).min(count);
            let part = e.call(
                all,
                c"slice",
                &[
                    node_api::arrays::index(e, start)?,
                    node_api::arrays::index(e, end)?,
                ],
            )?;
            let rows = e.call(part, c"map", &[mapper])?;
            frame("MODELS", &e.json_with_limit(rows, usize::MAX - 1)?);
        }
        let result = obj(e)?;
        e.set(result, c"providers", providers)?;
        e.set(result, c"count", node_api::arrays::index(e, count)?)?;
        prop(e, state, result, c"refresh")?;
        frame("CATALOG", &e.json_with_limit(result, usize::MAX - 1)?);
        Ok(null_mut())
    })
}
extern "C" fn error_entry(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (_, [pair]) = e.args(info)?;
        e.call(
            e.get(e.global()?, c"Array")?,
            c"of",
            &[e.get(pair, c"0")?, e.get(e.get(pair, c"1")?, c"message")?],
        )
    })
}
extern "C" fn refreshed(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [result]) = e.args(info)?;
        let g = e.global()?;
        let report = obj(e)?;
        prop(e, result, report, c"aborted")?;
        let entries = e.call(
            e.get(g, c"Array")?,
            c"from",
            &[
                e.get(result, c"errors")?,
                e.function(c"errorEntry", error_entry)?,
            ],
        )?;
        let errors = e.call(e.get(g, c"Object")?, c"fromEntries", &[entries])?;
        e.set(report, c"errors", errors)?;
        e.set(state, c"refresh", report)?;
        snapshot(e, state)
    })
}
fn abort(e: &Env, state: Handle) -> Read<Handle> {
    let active = e.get(state, c"controller")?;
    if e.kind(active)? == 6 {
        e.call(active, c"abort", &[])?;
    }
    Ok(null_mut())
}
extern "C" fn close(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, []) = e.args(info)?;
        abort(e, state)
    })
}
extern "C" fn command(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [line]) = e.args(info)?;
        let request = jsparse(e, line)?;
        if e.kind(e.get(request, c"cancel")?)? != 0 {
            return abort(e, state);
        }
        let ctrl = construct(e, c"AbortController", &[])?;
        e.set(state, c"controller", ctrl)?;
        if e.json(e.get(request, c"aborted")?)? == b"true" {
            e.call(ctrl, c"abort", &[])?;
        }
        let options = e.get(request, c"refresh")?;
        e.set(options, c"signal", e.get(ctrl, c"signal")?)?;
        let p = e.call(e.get(state, c"runtime")?, c"refresh", &[options])?;
        on_promise(e, state, p, refreshed)
    })
}
