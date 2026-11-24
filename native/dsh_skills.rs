//! A Cordis plugin for DSH's global skill service, using the host's Node-API ABI.
//! All callbacks run on the DSH event-loop thread. Promise callbacks retain their
//! context via Function.bind; no native reference outlives its callback scope.

mod node_api;
use node_api::{Env, Handle, Read};
use std::{
    io::{self, Write},
    ptr::null_mut,
};
impl Env {
    fn then(&self, promise: Handle, callback: Handle) -> Read<()> {
        self.call(
            promise,
            c"then",
            &[callback, self.function(c"failed", failed)?],
        )?;
        Ok(())
    }
}

fn packet(bytes: &[u8]) {
    let mut output = io::stdout().lock();
    let _ = output.write_all(b"KAR-DSH-SKILLS ");
    let _ = output.write_all(bytes);
    let _ = output.write_all(b"\n");
    let _ = output.flush();
}
fn callback(env: Handle, action: impl FnOnce(&Env) -> Read<()>) -> Handle {
    if let Err(code) = action(&Env(env)) {
        Env(env).clear_exception();
        packet(if code == "native_query_unavailable" {
            br#"{"error":"native_query_unavailable"}"#
        } else {
            br#"{"error":"query_failed"}"#
        });
    }
    null_mut()
}

extern "C" fn failed(_: Handle, _: Handle) -> Handle {
    packet(br#"{"error":"query_failed"}"#);
    null_mut()
}
extern "C" fn completed(env: Handle, info: Handle) -> Handle {
    callback(env, |e| {
        let (_, [snapshot]) = e.args(info)?;
        packet(&e.json(snapshot)?);
        Ok(())
    })
}
extern "C" fn loaded(env: Handle, info: Handle) -> Handle {
    callback(env, |e| {
        let (ctx, [scope]) = e.args(info)?;
        let skills = e.call(ctx, c"get", &[e.text("skills")?])?;
        if e.kind(skills)? != 6 {
            return Err("native_query_unavailable");
        } // napi_object
        let snapshot = e.call(skills, c"snapshot", &[scope])?;
        e.then(snapshot, e.function(c"completed", completed)?)
    })
}
extern "C" fn apply(env: Handle, info: Handle) -> Handle {
    callback(env, |e| {
        let (_, [ctx, scope]) = e.args(info)?;
        let loader = e.call(ctx, c"get", &[e.text("loader")?])?;
        let ready = e.call(loader, c"await", &[])?;
        let bound = e.call(e.function(c"loaded", loaded)?, c"bind", &[ctx, scope])?;
        e.then(ready, bound)
    })
}

#[unsafe(no_mangle)]
extern "C" fn napi_register_module_v1(env: Handle, exports: Handle) -> Handle {
    packet(b"loaded");
    callback(env, |e| {
        let apply = e.function(c"apply", apply)?;
        e.set(exports, c"apply", apply)
    });
    exports
}
