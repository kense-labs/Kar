//! Read Pi's live registry when an inventory-only RPC process finishes startup.
//! The host closes stdin after get_state succeeds. Pi then emits session_shutdown
//! with the registry and user extensions' final startup loadout still available.
mod node_api;
mod pipe;
use node_api::{Env, Handle, Read};
use std::ptr::null_mut;

fn packet(data: &[u8]) {
    pipe::stdout(&[b"OAR-PI-TOOLS ", data, b"\n"].concat());
}

fn report(env: Handle, action: impl FnOnce(&Env) -> Read<()>) -> Handle {
    let e = Env(env);
    if let Err(code) = action(&e) {
        e.clear_exception();
        packet(if code == "native_query_unavailable" {
            b"unsupported"
        } else {
            b"failed"
        });
    }
    null_mut()
}
extern "C" fn collect(env: Handle, info: Handle) -> Handle {
    report(env, |e| {
        let (pi, [event]) = e.args(info)?;
        if e.json(e.get(event, c"reason")?)? != br#""quit""# {
            return Ok(());
        }
        let snapshot = e.call(e.global()?, c"Object", &[])?;
        e.set(snapshot, c"tools", e.call(pi, c"getAllTools", &[])?)?;
        e.set(snapshot, c"active", e.call(pi, c"getActiveTools", &[])?)?;
        packet(&e.json(snapshot)?);
        Ok(())
    })
}
extern "C" fn factory(env: Handle, info: Handle) -> Handle {
    report(env, |e| {
        let (_, [pi]) = e.args(info)?;
        let bound = e.call(e.function(c"collect", collect)?, c"bind", &[pi])?;
        e.call(pi, c"on", &[e.text("session_shutdown")?, bound])?;
        packet(b"ready");
        Ok(())
    })
}
#[unsafe(no_mangle)]
extern "C" fn napi_register_module_v1(env: Handle, _exports: Handle) -> Handle {
    Env(env)
        .function(c"piToolInventory", factory)
        .unwrap_or(null_mut())
}
