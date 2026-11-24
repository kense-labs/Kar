//! Call Pi's metadata-only resolver before an RPC child or workspace picker loads extensions.
use super::*;

pub(super) fn check(e: &Env, state: Handle, rpc: bool) -> Read<()> {
    let settings = e.get(state, c"settings")?;
    let missing = construct(e, c"Array", &[])?;
    e.set(state, c"missing", missing)?;
    if rpc {
        e.set(state, c"rpcPreflight", jsparse(e, e.text("true")?)?)?;
    }
    let project = e.get(e.call(settings, c"getProjectSettings", &[])?, c"packages")?;
    let user = e.get(e.call(settings, c"getGlobalSettings", &[])?, c"packages")?;
    let count = |v| {
        if e.kind(v)? == 6 {
            node_api::arrays::len(e, v)
        } else {
            Ok(0)
        }
    };
    let project_count = count(project)?;
    let offline = std::env::var("PI_OFFLINE")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"));
    if (project_count == 0 && count(user)? == 0) || (offline && (!rpc || project_count == 0)) {
        return complete(e, state);
    }
    let options = obj(e)?;
    e.set(
        options,
        c"cwd",
        e.call(e.get(e.global()?, c"process")?, c"cwd", &[])?,
    )?;
    prop(e, e.get(state, c"config")?, options, c"agentDir")?;
    e.set(options, c"settingsManager", settings)?;
    let class = e.get(e.get(state, c"package")?, c"DefaultPackageManager")?;
    let args = e.call(e.get(e.global()?, c"Array")?, c"of", &[options])?;
    let manager = e.call(
        e.get(e.global()?, c"Reflect")?,
        c"construct",
        &[class, args],
    )?;
    if offline {
        // Native lookup in explicit offline mode does not attempt a download or version replacement.
        for i in 0..project_count {
            let entry = e.call(project, c"at", &[node_api::arrays::index(e, i)?])?;
            let source = if e.kind(entry)? == 4 {
                entry
            } else {
                e.get(entry, c"source")?
            };
            let path = e.call(manager, c"getInstalledPath", &[source, e.text("project")?])?;
            if e.kind(path)? == 0 {
                e.call(missing, c"push", &[source])?;
            }
        }
        return complete(e, state);
    }
    let p = e.call(
        manager,
        c"resolve",
        &[bound(e, state, c"skipPackage", skip_missing)?],
    )?;
    on_promise(e, state, p, checked)?;
    Ok(())
}

fn complete(e: &Env, state: Handle) -> Read<()> {
    let missing = e.get(state, c"missing")?;
    if e.kind(e.get(state, c"rpcPreflight")?)? == 2 {
        frame("PACKAGES", &e.json(missing)?);
        Ok(())
    } else if node_api::arrays::len(e, missing)? > 0 {
        frame("ERROR", b"missingPackages");
        Ok(())
    } else {
        pi_picker::services(e, state)
    }
}
extern "C" fn skip_missing(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [source]) = e.args(info)?;
        e.call(e.get(state, c"missing")?, c"push", &[source])?;
        e.call(
            e.get(e.global()?, c"Promise")?,
            c"resolve",
            &[e.text("skip")?],
        )
    })
}
extern "C" fn checked(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [_]) = e.args(info)?;
        complete(e, state)?;
        Ok(null_mut())
    })
}
