//! Bind the native scoped policy before the host admits prompts. ACP assigns
//! its root ID after creating the agent, so also check the live registry once
//! the host has selected it. Later agent replacements use the lifecycle hook.
use crate::node_api::{Env, Handle, Read, arrays};
use crate::{dsh_agent_startup as startup, dsh_prompt};
use std::{fs, io::Write, path::PathBuf, ptr::null_mut};

pub(super) fn directory() -> Read<PathBuf> {
    let file = std::env::var_os("KAR_DSH_PROFILE_RECEIPT").ok_or("missing receipt")?;
    PathBuf::from(file)
        .parent()
        .map(ToOwned::to_owned)
        .ok_or("missing directory")
}

pub(super) fn install(e: &Env, ctx: Handle) -> Read<()> {
    let raw = match fs::read_to_string(directory()?.join("agent-options")) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("unreadable agent options"),
    };
    let global = e.global()?;
    let opts = e.call(e.get(global, c"JSON")?, c"parse", &[e.text(&raw)?])?;
    let state = e.call(global, c"Object", &[])?;
    e.set(state, c"opts", opts)?;
    e.set(state, c"ctx", ctx)?;
    startup::install(e, state, ctx)?;
    let cb = e.call(e.function(c"created", created)?, c"bind", &[state])?;
    let options = e.call(
        e.get(global, c"JSON")?,
        c"parse",
        &[e.text(r#"{"global":true}"#)?],
    )?;
    e.call(ctx, c"on", &[e.text("agent/created")?, cb, options])?;
    let tick = e.call(e.function(c"checkRoot", check_root)?, c"bind", &[state])?;
    let timer = e.call(global, c"setInterval", &[tick, arrays::index(e, 20)?])?;
    e.set(state, c"timer", timer)?;
    e.call(timer, c"unref", &[])?;
    Ok(())
}

pub(super) fn selected(e: &Env, state: Handle) -> Read<Option<Handle>> {
    let target = e.get(state, c"target")?;
    if e.kind(target)? == 4 {
        return Ok(Some(target));
    }
    match fs::read_to_string(directory()?.join("agent-target")) {
        Ok(id) => {
            let target = e.text(&id)?;
            e.set(state, c"target", target)?;
            Ok(Some(target))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("unreadable agent identity"),
    }
}

pub(super) fn same(e: &Env, a: Handle, b: Handle) -> Read<bool> {
    let eq = e.call(e.get(e.global()?, c"Object")?, c"is", &[a, b])?;
    Ok(e.json(eq)? == b"true")
}

fn configure(e: &Env, state: Handle, agent: Handle) -> Read<()> {
    let ctx = e.get(e.get(agent, c"scope")?, c"ctx")?;
    let opts = e.get(state, c"opts")?;
    let filter = e.get(opts, c"tools")?;
    let deny = e.get(filter, c"deny")?;
    let count = arrays::len(e, deny)?;
    if count > 0 {
        let tools = e.get(ctx, c"tools")?;
        e.call(tools, c"restrict", &[filter])?;
        for i in 0..count {
            let name = e.call(deny, c"at", &[arrays::index(e, i)?])?;
            if e.kind(e.call(tools, c"get", &[name, agent])?)? != 0 {
                return Err("excluded tool remains available");
            }
        }
    }
    let directories = e.get(opts, c"directories")?;
    let count = arrays::len(e, directories)?;
    if count > 0 {
        let policy = e.call(ctx, c"get", &[e.text("sandboxPolicy")?])?;
        let session = e.get(agent, c"session")?;
        for i in 0..count {
            let directory = e.call(directories, c"at", &[arrays::index(e, i)?])?;
            let dispose = e.call(policy, c"registerWritableRoot", &[session, directory])?;
            if e.kind(dispose)? != 7 {
                return Err("native writable root did not return a disposer");
            }
            let effect = e.call(e.function(c"rootEffect", root_effect)?, c"bind", &[dispose])?;
            e.call(ctx, c"effect", &[effect])?;
        }
    }
    Ok(())
}

// Cordis calls the effect on registration and its returned disposer when the
// selected agent leaves its scope.
extern "C" fn root_effect(env: Handle, info: Handle) -> Handle {
    Env(env)
        .args::<0>(info)
        .map(|(dispose, [])| dispose)
        .unwrap_or(null_mut())
}

pub(super) fn finish(e: &Env, state: Handle, code: u8) -> Read<()> {
    if code == b'1' && startup::pending(e, state)? {
        match super::dsh_title::confirm(e, state) {
            Ok(true) => return Ok(()),
            Ok(false) => (),
            Err(err) => {
                e.clear_exception();
                settle(e, state, b'N')?;
                return Err(err);
            }
        }
    }
    settle(e, state, code)
}

pub(super) fn settle(e: &Env, state: Handle, code: u8) -> Read<()> {
    startup::release(e, state, code == b'1')?;
    let timer = e.get(state, c"timer")?;
    if e.kind(timer)? != 0 {
        e.call(e.global()?, c"clearInterval", &[timer])?;
    }
    if let Ok(mut file) = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(directory()?.join("agent-result"))
    {
        file.write_all(&[code])
            .map_err(|_| "failed agent receipt")?;
    }
    Ok(())
}

fn apply_selected(e: &Env, state: Handle, agent: Handle) -> Read<Handle> {
    if same(e, agent, e.get(state, c"configured")?)? {
        return e.get(state, c"work");
    }
    let attempt = (|| {
        configure(e, state, agent)?;
        e.set(state, c"configured", agent)?;
        let section = e.get(e.get(state, c"opts")?, c"prompt")?;
        if e.kind(section)? == 6 {
            let work = dsh_prompt::verify(e, state, agent, section)?;
            e.set(state, c"work", work)?;
            return Ok(work);
        }
        finish(e, state, b'1')?;
        Ok(null_mut())
    })();
    if attempt.is_err() {
        e.clear_exception();
        finish(e, state, b'0')?;
    }
    attempt
}

extern "C" fn handled(_: Handle, _: Handle) -> Handle {
    null_mut()
}

extern "C" fn check_root(env: Handle, info: Handle) -> Handle {
    let e = Env(env);
    let result = (|| {
        let (state, []) = e.args(info)?;
        if startup::pending(&e, state)? && !directory()?.join("agent-bound").is_file() {
            return Ok(());
        }
        if let Some(target) = selected(&e, state)? {
            let agents = e.call(e.get(state, c"ctx")?, c"get", &[e.text("agents")?])?;
            let agent = e.call(agents, c"get", &[target])?;
            if e.kind(agent)? == 6 {
                let work = apply_selected(&e, state, agent)?;
                if !work.is_null() && e.kind(work)? == 6 {
                    e.call(work, c"catch", &[e.function(c"handled", handled)?])?;
                    e.call(e.global()?, c"clearInterval", &[e.get(state, c"timer")?])?;
                }
            }
        }
        Ok::<(), &'static str>(())
    })();
    if result.is_err() {
        e.clear_exception();
    }
    null_mut()
}

extern "C" fn created(env: Handle, info: Handle) -> Handle {
    let e = Env(env);
    let result = (|| {
        let (state, [event]) = e.args(info)?;
        let agent = e.get(event, c"agent")?;
        if startup::pending(&e, state)? {
            startup::hold_tools(&e, state, agent)?;
        } else if let Some(target) = selected(&e, state)?
            && same(&e, target, e.get(agent, c"id")?)?
        {
            return apply_selected(&e, state, agent);
        }
        Ok(null_mut())
    })();
    result.unwrap_or_else(|_| {
        e.clear_exception();
        rejected(&e)
    })
}
pub(super) fn rejected(e: &Env) -> Handle {
    (|| {
        e.call(
            e.get(e.global()?, c"Promise")?,
            c"reject",
            &[e.text("DSH root settings could not be applied")?],
        )
    })()
    .unwrap_or(null_mut())
}
