//! DSH can queue its first input before its transport discloses the new
//! session ID. Hold the native step until the host verifies that root's scope.
use super::dsh_agent_controls::{directory, rejected, same, selected};
use super::dsh_prompt::service;
use crate::node_api::{Env, Handle, Read, arrays};
use std::ptr::null_mut;

pub(super) fn install(e: &Env, state: Handle, ctx: Handle) -> Read<()> {
    let global = e.global()?;
    let promise = e.get(global, c"Promise")?;
    e.set(state, c"barriers", e.call(global, c"Array", &[])?)?;
    e.set(state, c"gate", e.call(promise, c"withResolvers", &[])?)?;
    let cb = e.call(e.function(c"awaitRoot", await_root)?, c"bind", &[state])?;
    let options = e.call(
        e.get(global, c"JSON")?,
        c"parse",
        &[e.text(r#"{"global":true,"prepend":true}"#)?],
    )?;
    e.call(
        ctx,
        c"on",
        &[e.text("system-prompt/assemble")?, cb, options],
    )?;
    Ok(())
}

pub(super) fn pending(e: &Env, state: Handle) -> Read<bool> {
    Ok(e.kind(e.get(state, c"gate")?)? == 6 && e.kind(e.get(state, c"confirmed")?)? == 0)
}

pub(super) fn release(e: &Env, state: Handle, ok: bool) -> Read<()> {
    let gate = e.get(state, c"gate")?;
    if e.kind(gate)? == 6 {
        let barriers = e.get(state, c"barriers")?;
        while arrays::len(e, barriers)? > 0 {
            let dispose = e.call(barriers, c"pop", &[])?;
            e.call(dispose, c"call", &[])?;
        }
        let result = arrays::index(e, u32::from(ok))?;
        e.set(state, c"confirmed", result)?;
        e.call(gate, c"resolve", &[result])?;
    }
    Ok(())
}

fn candidate(e: &Env, state: Handle, agent: Handle) -> Read<bool> {
    if let Some(target) = selected(e, state)? {
        return same(e, target, e.get(agent, c"id")?);
    }
    if !directory()?.join("agent-opening").is_file() {
        return Ok(false);
    }
    let header = e.get(e.get(agent, c"session")?, c"header")?;
    Ok(!same(e, e.get(header, c"origin")?, e.text("subagent")?)?
        && same(
            e,
            e.get(header, c"cwd")?,
            e.get(e.get(state, c"opts")?, c"cwd")?,
        )?)
}

// With step-based tool selection, DSH snapshots available schemas before its
// prompt waterfall. Let the native activation phase wait too. This temporary
// scope-local condition always returns false, and is disposed before the
// awaiting resolver can publish its step's tools.
pub(super) fn hold_tools(e: &Env, state: Handle, agent: Handle) -> Read<()> {
    let opts = e.get(state, c"opts")?;
    if arrays::len(e, e.get(e.get(opts, c"tools")?, c"deny")?)? == 0 || !candidate(e, state, agent)?
    {
        return Ok(());
    }
    let tools = e.get(e.get(e.get(agent, c"scope")?, c"ctx")?, c"tools")?;
    if e.kind(e.get(tools, c"resolve")?)? != 7 {
        return Ok(());
    }
    let global = e.global()?;
    let definition = e.call(e.get(global,c"JSON")?, c"parse",&[e.text(r#"{"description":"","parameters":{"type":"object","properties":{}},"output":{"schema":{"type":"string"}}}"#)?])?;
    e.set(definition, c"name", e.get(opts, c"barrierName")?)?;
    let unreachable = e.function(c"unavailable", unavailable)?;
    e.set(definition, c"execute", unreachable)?;
    e.set(e.get(definition, c"output")?, c"render", unreachable)?;
    let registration = e.call(global, c"Object", &[])?;
    let cb = e.call(
        e.function(c"waitForRoot", wait_for_root)?,
        c"bind",
        &[state],
    )?;
    e.set(registration, c"when", cb)?;
    let dispose = e.call(tools, c"register", &[definition, registration])?;
    e.call(e.get(state, c"barriers")?, c"push", &[dispose])?;
    Ok(())
}

extern "C" fn unavailable(env: Handle, _: Handle) -> Handle {
    Env(env)
        .global()
        .and_then(|global| Env(env).call(global, c"Boolean", &[]))
        .unwrap_or(null_mut())
}

extern "C" fn wait_for_root(env: Handle, info: Handle) -> Handle {
    let e = Env(env);
    (|| {
        let (state, []) = e.args(info)?;
        let gate = e.get(e.get(state, c"gate")?, c"promise")?;
        e.call(gate, c"then", &[e.function(c"unavailable", unavailable)?])
    })()
    .unwrap_or_else(|_| {
        e.clear_exception();
        rejected(&e)
    })
}

// Creation listeners may enqueue input before ACP session/new or Web fork
// returns its new ID. Park only model assemblies in this startup window;
// native readback (which has no turn signal) can still finish.
extern "C" fn await_root(env: Handle, info: Handle) -> Handle {
    let e = Env(env);
    let result = (|| {
        let (state, [_, context, next]) = e.args(info)?;
        let proceed = || e.call(next, c"call", &[]);
        if e.kind(e.get(context, c"signal")?)? == 0 {
            return proceed();
        }
        let agent = e.get(context, c"agent")?;
        if e.kind(agent)? != 6 {
            return proceed();
        }
        let confirmed = e.get(state, c"confirmed")?;
        if e.kind(confirmed)? != 0 {
            if e.json(confirmed)? == b"0" && candidate(&e, state, agent)? {
                return Err("root settings failed");
            }
            return proceed();
        }
        if !candidate(&e, state, agent)? {
            return proceed();
        }
        let resume = e.call(
            e.function(c"resumeAssembly", resume_assembly)?,
            c"bind",
            &[state, context, next],
        )?;
        e.call(
            e.get(e.get(state, c"gate")?, c"promise")?,
            c"then",
            &[resume],
        )
    })();
    result.unwrap_or_else(|_| {
        e.clear_exception();
        rejected(&e)
    })
}

extern "C" fn resume_assembly(env: Handle, info: Handle) -> Handle {
    let e = Env(env);
    let result = (|| {
        let (state, [context, next, ok]) = e.args(info)?;
        let agent = e.get(context, c"agent")?;
        let target = selected(&e, state)?.ok_or("missing target")?;
        if !same(&e, e.get(agent, c"id")?, target)? {
            return e.call(next, c"call", &[]);
        }
        if e.json(ok)? != b"1" {
            return Err("root prompt failed");
        }
        // The in-flight assembly sampled the old native sections. Rebuild
        // against the same turn, scope, model and cancellation signal.
        e.call(service(&e, agent)?, c"assemble", &[context])
    })();
    result.unwrap_or_else(|_| {
        e.clear_exception();
        rejected(&e)
    })
}
