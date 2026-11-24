//! A workspace model picker uses Pi's session services and registered extensions, without an agent session.
use super::*;

fn promise(e: &Env, v: Handle) -> Read<Handle> {
    e.call(e.get(e.global()?, c"Promise")?, c"resolve", &[v])
}

pub(super) fn start(e: &Env, state: Handle) -> Read<()> {
    let settings = e.get(state, c"settings")?;
    let flags = std::env::var("PI_OFFLINE")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut packages = false;
    for key in [c"getGlobalSettings", c"getProjectSettings"] {
        let value = e.get(e.call(settings, key, &[])?, c"packages")?;
        packages |= e.kind(value)? == 6 && node_api::arrays::len(e, value)? > 0;
    }
    if !packages || matches!(flags.as_str(), "1" | "true" | "yes") {
        return services(e, state);
    }
    let cfg = e.get(state, c"config")?;
    let options = obj(e)?;
    e.set(
        options,
        c"cwd",
        e.call(e.get(e.global()?, c"process")?, c"cwd", &[])?,
    )?;
    prop(e, cfg, options, c"agentDir")?;
    e.set(options, c"settingsManager", settings)?;
    let class = e.get(e.get(state, c"package")?, c"DefaultPackageManager")?;
    let args = e.call(e.get(e.global()?, c"Array")?, c"of", &[options])?;
    let manager = e.call(
        e.get(e.global()?, c"Reflect")?,
        c"construct",
        &[class, args],
    )?;
    let p = e.call(
        manager,
        c"resolve",
        &[bound(e, state, c"skipPackage", skip_missing)?],
    )?;
    on_promise(e, state, p, checked)?;
    Ok(())
}

extern "C" fn skip_missing(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [_source]) = e.args(info)?;
        e.set(state, c"missing", jsparse(e, e.text("true")?)?)?;
        promise(e, e.text("skip")?)
    })
}
extern "C" fn checked(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [_]) = e.args(info)?;
        if e.kind(e.get(state, c"missing")?)? == 2 {
            frame("ERROR", b"missingPackages");
        } else {
            services(e, state)?;
        }
        Ok(null_mut())
    })
}
fn services(e: &Env, state: Handle) -> Read<()> {
    let cfg = e.get(state, c"config")?;
    let g = e.global()?;
    let opts = obj(e)?;
    e.set(opts, c"cwd", e.call(e.get(g, c"process")?, c"cwd", &[])?)?;
    prop(e, cfg, opts, c"agentDir")?;
    e.set(opts, c"settingsManager", e.get(state, c"settings")?)?;
    let flags = jsparse(
        e,
        e.text(
            r#"{"noSkills":true,"noPromptTemplates":true,"noThemes":true,"noContextFiles":true}"#,
        )?,
    )?;
    e.set(opts, c"resourceLoaderOptions", flags)?;
    let signal = e.call(
        e.get(g, c"AbortSignal")?,
        c"timeout",
        &[e.get(cfg, c"timeoutMs")?],
    )?;
    e.set(opts, c"modelRuntimeSignal", signal)?;
    e.set(state, c"signal", signal)?;
    let p = e.call(
        e.get(state, c"package")?,
        c"createAgentSessionServices",
        &[opts],
    )?;
    on_promise(e, state, p, available)?;
    Ok(())
}
extern "C" fn available(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [services]) = e.args(info)?;
        let rt = e.get(services, c"modelRuntime")?;
        e.set(state, c"runtime", rt)?;
        let opts = obj(e)?;
        prop(e, state, opts, c"signal")?;
        let p = e.call(
            rt,
            c"getAvailable",
            &[e.get(e.global()?, c"undefined")?, opts],
        )?;
        on_promise(e, state, p, publish)
    })
}
extern "C" fn row(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [model]) = e.args(info)?;
        let out = obj(e)?;
        for key in [c"id", c"provider", c"name"] {
            prop(e, model, out, key)?;
        }
        let rt = e.get(state, c"runtime")?;
        let provider = e.call(rt, c"getProvider", &[e.get(model, c"provider")?])?;
        if e.kind(provider)? == 6 {
            e.set(out, c"providerName", e.get(provider, c"name")?)?;
        }
        let reasoning = e.get(model, c"reasoning")?;
        if e.kind(reasoning)? == 2 && e.json(reasoning)? == b"true" {
            let levels = e.call(
                e.get(state, c"piAi")?,
                c"getSupportedThinkingLevels",
                &[model],
            )?;
            e.set(out, c"levels", levels)?;
        }
        Ok(out)
    })
}
extern "C" fn publish(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [models]) = e.args(info)?;
        let rt = e.get(state, c"runtime")?;
        let settings = e.get(state, c"settings")?;
        let out = obj(e)?;
        e.set(
            out,
            c"all",
            e.get(e.call(rt, c"getModels", &[])?, c"length")?,
        )?;
        e.set(
            out,
            c"defaultProvider",
            e.call(settings, c"getDefaultProvider", &[])?,
        )?;
        e.set(
            out,
            c"defaultModel",
            e.call(settings, c"getDefaultModel", &[])?,
        )?;
        let models = e.call(models, c"map", &[bound(e, state, c"pickRow", row)?])?;
        e.set(out, c"models", models)?;
        frame("PICKER", &e.json_with_limit(out, 16 * 1024 * 1024)?);
        Ok(null_mut())
    })
}
