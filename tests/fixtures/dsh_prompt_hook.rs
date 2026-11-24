//! Real Cordis creation hook used by the native prompt regression tests.
#[path = "../../native/node_api.rs"]
mod node_api;
use node_api::{Env, Handle, Read};
use std::ptr::null_mut;

fn invoke(env: Handle, run: impl FnOnce(&Env) -> Read<()>) -> Handle {
    let e = Env(env);
    if let Err(error) = run(&e) {
        e.clear_exception();
        return e
            .call(
                e.get(e.global().unwrap(), c"Promise").unwrap(),
                c"reject",
                &[e.text(error).unwrap()],
            )
            .unwrap_or(null_mut());
    }
    null_mut()
}
extern "C" fn created(env: Handle, info: Handle) -> Handle {
    invoke(env, |e| {
        let (options, [event]) = e.args(info)?;
        let mode = e.get(options, c"mode")?;
        let agent = e.get(event, c"agent")?;
        let json = e.get(e.global()?, c"JSON")?;
        if e.string_bytes(mode, 32)? == b"conflict" {
            let prompt = e.get(e.get(e.get(agent, c"scope")?, c"ctx")?, c"systemPrompt")?;
            let section = e.call(json, c"parse", &[e.text(r#"{"name":"fixture:persona","order":1,"text":"Existing complete persona","complete":true}"#)?])?;
            e.call(prompt, c"section", &[section])?;
        }
        let message = e.call(json,c"parse",&[e.text(r#"{"role":"user","content":[{"type":"text","text":"DSH_PROMPT_AUTO"}],"source":{"kind":"user"}}"#)?])?;
        e.set(
            message,
            c"id",
            e.call(e.get(e.global()?, c"crypto")?, c"randomUUID", &[])?,
        )?;
        e.call(agent, c"followup", &[message])?;
        Ok(())
    })
}
extern "C" fn apply(env: Handle, info: Handle) -> Handle {
    invoke(env, |e| {
        let (_, [ctx, opts]) = e.args(info)?;
        let cb = e.call(e.function(c"created", created)?, c"bind", &[opts])?;
        e.call(ctx, c"on", &[e.text("agent/created")?, cb])?;
        Ok(())
    })
}
#[unsafe(no_mangle)]
extern "C" fn napi_register_module_v1(env: Handle, exports: Handle) -> Handle {
    invoke(env, |e| {
        e.set(exports, c"apply", e.function(c"apply", apply)?)
    });
    exports
}
