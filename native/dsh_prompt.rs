//! Register a literal section in DSH's agent-specific prompt and check the
//! native assembly after applying user and deployment plugins.
use super::dsh_agent_controls::{finish, rejected, same};
use crate::node_api::{Env, Handle, Read, arrays};
use std::ptr::null_mut;

pub(super) fn service(e: &Env, agent: Handle) -> Read<Handle> {
    e.get(e.get(e.get(agent, c"scope")?, c"ctx")?, c"systemPrompt")
}

pub(super) fn verify(e: &Env, state: Handle, agent: Handle, section: Handle) -> Read<Handle> {
    let prompt = service(e, agent)?;
    let base = e.call(
        prompt,
        c"getSectionOrder",
        &[e.text("DEPLOYMENT_PERSONA_SUFFIX")?],
    )?;
    e.set(
        section,
        c"order",
        crate::node_api::numbers::increment(e, base)?,
    )?;
    e.call(prompt, c"section", &[section])?;
    let context = e.call(e.global()?, c"Object", &[])?;
    e.set(context, c"agent", agent)?;
    e.set(context, c"scope", agent)?;
    let assembly = e.call(prompt, c"assemble", &[context])?;
    let ok = e.call(
        e.function(c"verifySections", verify_sections)?,
        c"bind",
        &[state],
    )?;
    let fail = e.call(
        e.function(c"assemblyFailed", assembly_failed)?,
        c"bind",
        &[state],
    )?;
    e.call(assembly, c"then", &[ok, fail])
}

extern "C" fn assembly_failed(env: Handle, info: Handle) -> Handle {
    let e = Env(env);
    if let Ok((state, [])) = e.args(info) {
        let code = (|| {
            let requested = e.get(e.get(state, c"opts")?, c"prompt")?;
            Ok::<_, &'static str>(if e.kind(e.get(requested, c"complete")?)? == 2 {
                b'R'
            } else {
                b'A'
            })
        })()
        .unwrap_or(b'0');
        let _ = finish(&e, state, code);
    }
    rejected(&e)
}

extern "C" fn verify_sections(env: Handle, info: Handle) -> Handle {
    let e = Env(env);
    let result = (|| {
        let (state, [assembly]) = e.args(info)?;
        let expected = e.get(e.get(state, c"opts")?, c"prompt")?;
        let sections = e.get(assembly, c"sections")?;
        let count = arrays::len(&e, sections)?;
        if e.kind(e.get(expected, c"complete")?)? == 0 || count == 1 {
            for i in 0..count {
                let section = e.call(sections, c"at", &[arrays::index(&e, i)?])?;
                if same(&e, e.get(section, c"name")?, e.get(expected, c"name")?)?
                    && same(&e, e.get(section, c"text")?, e.get(expected, c"text")?)?
                    && same(
                        &e,
                        e.get(section, c"interpolate")?,
                        e.get(expected, c"interpolate")?,
                    )?
                {
                    return finish(&e, state, b'1');
                }
            }
        }
        Err("DSH omitted or changed the literal prompt section")
    })();
    if result.is_err() {
        e.clear_exception();
        return assembly_failed(env, info);
    }
    null_mut()
}
