//! Verify the requested plugins in the same Cordis process that will own the
//! session. The native loader settles even when individual plugins failed.
mod dsh_agent_controls;
mod dsh_agent_startup;
mod dsh_prompt;
mod dsh_title;
mod node_api;
use node_api::{Env, Handle, Read, arrays};
use std::{collections::HashSet, fs::OpenOptions, io::Write, ptr::null_mut};

fn report(confirmed: bool) {
    if let Some(path) = std::env::var_os("KAR_DSH_PROFILE_RECEIPT")
        && let Ok(mut file) = OpenOptions::new().create_new(true).write(true).open(path)
    {
        let _ = file.write_all(if confirmed { b"1" } else { b"0" });
    }
}

fn scope(env: Handle, run: impl FnOnce(&Env) -> Read<()>) -> Handle {
    let e = Env(env);
    if run(&e).is_err() {
        e.clear_exception();
        report(false);
    }
    null_mut()
}

extern "C" fn failed(_: Handle, _: Handle) -> Handle {
    report(false);
    null_mut()
}

fn after(e: &Env, ready: Handle, callback: Handle) -> Read<()> {
    e.call(ready, c"then", &[callback, e.function(c"failed", failed)?])?;
    Ok(())
}

extern "C" fn active(env: Handle, info: Handle) -> Handle {
    scope(env, |e| {
        let (fibers, []) = e.args(info)?;
        for i in 0..arrays::len(e, fibers)? {
            let fiber = e.call(fibers, c"at", &[arrays::index(e, i)?])?;
            if e.json_with_limit(e.get(fiber, c"state")?, 16)? != b"2" {
                return Err("requested plugin inactive");
            }
        }
        report(true);
        Ok(())
    })
}

extern "C" fn loaded(env: Handle, info: Handle) -> Handle {
    scope(env, |e| {
        let (loader, []) = e.args(info)?;
        let entries = e.call(loader, c"entries", &[])?;
        let rows = e.call(e.get(e.global()?, c"Array")?, c"from", &[entries])?;
        let required = std::env::var("KAR_DSH_REQUIRED_PLUGINS")
            .map_err(|_| "missing verification request")?;
        let mut missing: HashSet<&[u8]> = required
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::as_bytes)
            .collect();
        let check_upload = missing.contains(b"session-log-deepseek".as_slice());
        let promises = e.call(e.global()?, c"Array", &[])?;
        let fibers = e.call(e.global()?, c"Array", &[])?;
        for i in 0..arrays::len(e, rows)? {
            let row = e.call(rows, c"at", &[arrays::index(e, i)?])?;
            let opts = e.get(row, c"options")?;
            if e.kind(opts)? != 6 {
                continue;
            }
            let id = e.get(opts, c"id")?;
            if e.kind(id)? != 4 {
                continue;
            }
            let id = e.string_bytes(id, 64 * 1024)?;
            let name = e.get(opts, c"name")?;
            let name = if e.kind(name)? == 4 {
                e.string_bytes(name, 64 * 1024)?
            } else {
                Vec::new()
            };
            let official = name == b"@deepseek-ai/dsh-session-log-deepseek";
            let expected = missing.remove(id.as_slice());
            let uploader_id = id == b"session-log-deepseek";
            if expected && !uploader_id {
                if name != b"@deepseek-ai/dsh-mcp-client" {
                    return Err("wrong MCP plugin");
                }
            } else if check_upload && (official || uploader_id) {
                if !official {
                    return Err("wrong uploader");
                }
                let disabled = e.get(opts, c"disabled")?;
                if e.kind(disabled)? == 2 && e.json(disabled)? == b"true" {
                    continue;
                }
                let enabled = e.get(e.get(opts, c"config")?, c"enabled")?;
                if e.kind(enabled)? != 2 || e.json(enabled)? != b"false" {
                    return Err("uploader enabled");
                }
            } else {
                continue;
            }
            let fiber = e.get(row, c"fiber")?;
            e.call(fibers, c"push", &[fiber])?;
            e.call(promises, c"push", &[e.call(fiber, c"await", &[])?])?;
        }
        if !missing.is_empty() {
            return Err("missing requested plugin");
        }
        let ready = e.call(e.get(e.global()?, c"Promise")?, c"all", &[promises])?;
        let cb = e.call(e.function(c"active", active)?, c"bind", &[fibers])?;
        after(e, ready, cb)
    })
}

extern "C" fn apply(env: Handle, info: Handle) -> Handle {
    scope(env, |e| {
        let (_, [ctx]) = e.args(info)?;
        dsh_agent_controls::install(e, ctx)?;
        let loader = e.call(ctx, c"get", &[e.text("loader")?])?;
        let ready = e.call(loader, c"await", &[])?;
        let cb = e.call(e.function(c"loaded", loaded)?, c"bind", &[loader])?;
        after(e, ready, cb)
    })
}

#[unsafe(no_mangle)]
extern "C" fn napi_register_module_v1(env: Handle, exports: Handle) -> Handle {
    scope(env, |e| {
        e.set(exports, c"apply", e.function(c"apply", apply)?)
    });
    exports
}
