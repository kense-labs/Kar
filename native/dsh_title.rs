//! Pin the title only after other root controls pass. The ACP controller can
//! exit immediately after open, so wait for the native durable writer.
use super::dsh_agent_controls::{same, settle};
use crate::node_api::{Env, Handle, Read};
use std::ptr::null_mut;

fn apply_title(e: &Env, state: Handle) -> Read<Option<Handle>> {
    let options = e.get(state, c"opts")?;
    let requested = e.get(options, c"name")?;
    if e.kind(requested)? != 4 {
        return Ok(None);
    }
    let service = e.call(e.get(state, c"ctx")?, c"get", &[e.text("sessionTitle")?])?;
    let session = e.get(e.get(state, c"configured")?, c"session")?;
    if e.json(e.get(options, c"resuming")?)? == b"true" {
        let current = e.call(service, c"get", &[session])?;
        if e.kind(current)? != 0 {
            if e.kind(e.get(current, c"title")?)? != 4 {
                return Err("invalid saved DSH title");
            }
            return Ok(None);
        }
    }
    let persistence = e.call(
        e.get(state, c"ctx")?,
        c"get",
        &[e.text("sessionPersistence")?],
    )?;
    if e.kind(e.get(persistence, c"flush")?)? != 7 {
        return Err("no native persistence flush");
    }
    let accepted = e.call(service, c"rename", &[session, requested])?;
    let saved = e.call(service, c"get", &[session])?;
    let title = e.get(accepted, c"title")?;
    let seq = e.get(accepted, c"eventSeq")?;
    if e.kind(title)? != 4
        || e.kind(seq)? != 3
        || !same(e, title, e.get(saved, c"title")?)?
        || !same(e, seq, e.get(saved, c"eventSeq")?)?
    {
        return Err("native title was not recorded");
    }
    e.call(persistence, c"flush", &[]).map(Some)
}

/// Returns true when the flush callbacks own the startup receipt.
pub(super) fn confirm(e: &Env, state: Handle) -> Read<bool> {
    let Some(flush) = apply_title(e, state)? else {
        return Ok(false);
    };
    let ok = e.call(e.function(c"titleSaved", saved)?, c"bind", &[state])?;
    let fail = e.call(e.function(c"titleFailed", failed)?, c"bind", &[state])?;
    e.call(flush, c"then", &[ok, fail])?;
    Ok(true)
}

fn report(env: Handle, info: Handle, code: u8) -> Handle {
    let e = Env(env);
    if let Ok((state, [])) = e.args(info)
        && settle(&e, state, code).is_err()
    {
        e.clear_exception();
    }
    null_mut()
}
extern "C" fn saved(env: Handle, info: Handle) -> Handle {
    report(env, info, b'1')
}
extern "C" fn failed(env: Handle, info: Handle) -> Handle {
    report(env, info, b'N')
}
