//! Install Pi's own MCP extension with the servers supplied for this session.
mod node_api;
use node_api::{Env, Handle, Read, arrays};
use std::{fs, path::PathBuf, ptr::null_mut};

fn directory() -> Read<PathBuf> {
    std::env::var_os("KAR_PI_MCP_DIRECTORY")
        .map(PathBuf::from)
        .ok_or("missing directory")
}
fn handle(env: Handle, f: impl FnOnce(&Env) -> Read<Handle>) -> Handle {
    let e = Env(env);
    f(&e).unwrap_or_else(|_| {
        e.clear_exception();
        failed(env, null_mut())
    })
}
extern "C" fn failed(_: Handle, _: Handle) -> Handle {
    if let Ok(dir) = directory() {
        let _ = fs::write(dir.join("receipt"), b"failed");
    }
    null_mut()
}
extern "C" fn empty_config(env: Handle, _: Handle) -> Handle {
    handle(env, |e| {
        let g = e.global()?;
        let obj = e.call(g, c"Object", &[])?;
        for key in [c"servers", c"errors"] {
            e.set(obj, key, e.call(g, c"Array", &[])?)?;
        }
        Ok(obj)
    })
}
extern "C" fn register(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, []) = e.args(info)?;
        if e.kind(e.get(state, c"initialized")?)? != 4 {
            return Err("Pi MCP initialization failed");
        }
        let pi = e.get(state, c"pi")?;
        let dir = directory()?;
        let config = fs::read_to_string(dir.join("servers.json")).map_err(|_| "missing config")?;
        let rows = e.call(e.get(e.global()?, c"JSON")?, c"parse", &[e.text(&config)?])?;
        for i in 0..arrays::len(e, rows)? {
            let row = e.call(rows, c"at", &[arrays::index(e, i)?])?;
            e.call(
                pi,
                c"registerMcpServer",
                &[e.get(row, c"name")?, e.get(row, c"config")?],
            )?;
        }
        fs::write(dir.join("receipt"), b"registered").map_err(|_| "receipt")?;
        Ok(null_mut())
    })
}
extern "C" fn initialized(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, []) = e.args(info)?;
        e.set(state, c"initialized", e.text("ready")?)?;
        Ok(null_mut())
    })
}
extern "C" fn factory(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (_, [pi]) = e.args(info)?;
        let sdk = std::env::var("KAR_PI_MCP_SDK").map_err(|_| "missing SDK")?;
        let g = e.global()?;
        let module = e.call(
            e.get(g, c"process")?,
            c"getBuiltinModule",
            &[e.text("node:module")?],
        )?;
        let require = e.call(module, c"createRequire", &[e.text(&sdk)?])?;
        let package = e.call(require, c"call", &[module, e.text(&sdk)?])?;
        let options = e.call(g, c"Object", &[])?;
        e.set(
            options,
            c"loadConfig",
            e.function(c"loadConfig", empty_config)?,
        )?;
        let ext = e.call(package, c"createMcpExtension", &[options])?;
        // User factories have registered their servers by session_start. Register
        // ours before Pi's own session_start handler connects the direct tools.
        let state = e.call(g, c"Object", &[])?;
        e.set(state, c"pi", pi)?;
        let bind = |name, callback| e.call(e.function(name, callback)?, c"bind", &[state]);
        let on_start = bind(c"register", register)?;
        e.call(pi, c"on", &[e.text("session_start")?, on_start])?;
        let started = e.call(ext, c"call", &[g, pi])?;
        let promise = e.call(e.get(g, c"Promise")?, c"resolve", &[started])?;
        let then = bind(c"initialized", initialized)?;
        e.call(promise, c"then", &[then, e.function(c"failed", failed)?])
    })
}
#[unsafe(no_mangle)]
extern "C" fn napi_register_module_v1(env: Handle, _exports: Handle) -> Handle {
    Env(env)
        .function(c"karPiMcp", factory)
        .unwrap_or(null_mut())
}
