//! Private, single-operation host for the installed Pi SDK. User credentials
//! are supplied on stdin and are resolved and stored only by Pi.
mod node_api;
mod pi_catalog;
mod pi_picker;
mod pipe;
use node_api::{Env, Handle, Read};
use std::{
    ffi::CStr,
    io::{self, BufRead, Read as _},
    ptr::null_mut,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};
const LIMIT: u64 = 1024 * 1024;
static DEVICE: OnceLock<Mutex<mpsc::Receiver<String>>> = OnceLock::new();
static NEXT: AtomicU64 = AtomicU64::new(1);

fn frame(kind: &str, data: &[u8]) {
    pipe::stdout(&[b"KAR-AUTH ", kind.as_bytes(), b" ", data, b"\n"].concat());
}
fn input_line() -> Option<String> {
    // Loading Pi can put fd 0 in nonblocking mode. Keep partial frames,
    // including a UTF-8 character split across two pipe writes.
    let mut input = io::stdin().lock().take(LIMIT + 1);
    let mut bytes = Vec::new();
    loop {
        match input.read_until(b'\n', &mut bytes) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(_) if bytes.len() <= LIMIT as usize && bytes.pop() == Some(b'\n') => {
                return String::from_utf8(bytes).ok();
            }
            _ => return None,
        }
    }
}
fn obj(e: &Env) -> Read<Handle> {
    e.call(e.global()?, c"Object", &[])
}
fn construct(e: &Env, name: &CStr, args: &[Handle]) -> Read<Handle> {
    let g = e.global()?;
    // Array.of preserves a single array argument (used by new Set(ids)).
    let argv = e.call(e.get(g, c"Array")?, c"of", args)?;
    e.call(
        e.get(g, c"Reflect")?,
        c"construct",
        &[e.get(g, name)?, argv],
    )
}
fn bound(
    e: &Env,
    state: Handle,
    name: &CStr,
    cb: extern "C" fn(Handle, Handle) -> Handle,
) -> Read<Handle> {
    e.call(e.function(name, cb)?, c"bind", &[state])
}
fn handle(env: Handle, f: impl FnOnce(&Env) -> Read<Handle>) -> Handle {
    let e = Env(env);
    f(&e).unwrap_or_else(|_| {
        e.clear_exception();
        frame("ERROR", b"protocol");
        null_mut()
    })
}
fn on_promise(
    e: &Env,
    state: Handle,
    p: Handle,
    cb: extern "C" fn(Handle, Handle) -> Handle,
) -> Read<Handle> {
    e.call(
        p,
        c"then",
        &[
            bound(e, state, c"success", cb)?,
            bound(e, state, c"failure", failed)?,
        ],
    )
}
fn jsparse(e: &Env, text: Handle) -> Read<Handle> {
    e.call(e.get(e.global()?, c"JSON")?, c"parse", &[text])
}
fn prop(e: &Env, from: Handle, to: Handle, key: &CStr) -> Read<()> {
    e.set(to, key, e.get(from, key)?)
}
extern "C" fn failed(_: Handle, _: Handle) -> Handle {
    frame("ERROR", b"rejected");
    null_mut()
}
extern "C" fn done(_: Handle, _: Handle) -> Handle {
    frame("OK", b"");
    null_mut()
}
extern "C" fn notify(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (_, [event]) = e.args(info)?;
        let public = obj(e)?;
        for key in [
            c"type",
            c"url",
            c"instructions",
            c"userCode",
            c"verificationUri",
            c"intervalSeconds",
            c"expiresInSeconds",
            c"message",
        ] {
            prop(e, event, public, key)?
        }
        frame("EVENT", &e.json(public)?);
        Ok(null_mut())
    })
}
fn finish_prompt(e: &Env, state: Handle, id: Handle, answer: Option<Handle>) -> Read<Handle> {
    let pending = e.get(state, c"pending")?;
    let gate = e.call(pending, c"get", &[id])?;
    if e.kind(gate)? == 0 {
        return Ok(null_mut());
    }
    e.call(pending, c"delete", &[id])?;
    let signal = e.get(gate, c"signal")?;
    if e.kind(signal)? == 6 {
        e.call(
            signal,
            c"removeEventListener",
            &[e.text("abort")?, e.get(gate, c"listener")?],
        )?;
    }
    match answer {
        Some(text) => e.call(gate, c"resolve", &[text]),
        None => {
            frame("DISMISS", &e.json(id)?);
            e.call(gate, c"reject", &[e.text("Pi dismissed the prompt")?])
        }
    }
}
extern "C" fn dismiss(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [id]) = e.args(info)?;
        finish_prompt(e, state, id, None)
    })
}
extern "C" fn prompt(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [request]) = e.args(info)?;
        let id = e.text(&NEXT.fetch_add(1, Ordering::Relaxed).to_string())?;
        let gate = e.call(e.get(e.global()?, c"Promise")?, c"withResolvers", &[])?;
        let pending = e.get(state, c"pending")?;
        e.call(pending, c"set", &[id, gate])?;
        let public = obj(e)?;
        e.set(public, c"id", id)?;
        for key in [c"type", c"message", c"placeholder", c"options"] {
            prop(e, request, public, key)?;
        }
        frame("PROMPT", &e.json(public)?);
        let signal = e.get(request, c"signal")?;
        if e.kind(signal)? == 6 {
            let listener = e.call(e.function(c"onDismiss", dismiss)?, c"bind", &[state, id])?;
            e.set(gate, c"signal", signal)?;
            e.set(gate, c"listener", listener)?;
            e.call(signal, c"addEventListener", &[e.text("abort")?, listener])?;
            if e.json(e.get(signal, c"aborted")?)? == b"true" {
                finish_prompt(e, state, id, None)?;
            }
        }
        e.get(gate, c"promise")
    })
}
extern "C" fn answer(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [line]) = e.args(info)?;
        let msg = jsparse(e, line)?;
        if e.kind(e.get(msg, c"cancel")?)? != 0 {
            return e.call(e.get(state, c"controller")?, c"abort", &[]);
        }
        finish_prompt(e, state, e.get(msg, c"id")?, Some(e.get(msg, c"answer")?))
    })
}
extern "C" fn device(env: Handle, _: Handle) -> Handle {
    frame("DEVICE", b"");
    DEVICE
        .get()
        .and_then(|rx| rx.lock().ok()?.recv().ok())
        .and_then(|s| Env(env).text(&s).ok())
        .unwrap_or(null_mut())
}
fn start_input(e: &Env, state: Handle) -> Read<()> {
    let dispatch = node_api::lines::Sender::new(e, bound(e, state, c"onInput", answer)?)?;
    let (tx, rx) = mpsc::channel();
    DEVICE.set(Mutex::new(rx)).map_err(|_| "duplicate input")?;
    std::thread::Builder::new()
        .name("pi-auth-input".into())
        .spawn(move || {
            while let Some(line) = input_line() {
                if let Some(id) = line.strip_prefix('D') {
                    if tx.send(id.to_owned()).is_err() {
                        break;
                    }
                } else if !dispatch.send(line) {
                    break;
                }
            }
            drop(tx);
            dispatch.send(r#"{"cancel":true}"#.into());
        })
        .map_err(|_| "input thread")?;
    Ok(())
}

extern "C" fn provider(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (_, [row]) = e.args(info)?;
        let result = obj(e)?;
        prop(e, row, result, c"id")?;
        prop(e, row, result, c"name")?;
        let auth = e.get(row, c"auth")?;
        for (key, fields) in [
            (c"oauth", &[c"name", c"isSubscription", c"loginLabel"][..]),
            (c"apiKey", &[c"name"][..]),
        ] {
            let native = e.get(auth, key)?;
            if e.kind(native)? != 6 {
                continue;
            }
            let method = obj(e)?;
            for field in fields {
                prop(e, native, method, field)?;
            }
            if key == c"apiKey" && e.kind(e.get(native, c"login")?)? == 0 {
                e.set(method, c"ambient", jsparse(e, e.text("true")?)?)?;
            }
            e.set(result, key, method)?;
        }
        Ok(result)
    })
}
extern "C" fn report_status(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [check]) = e.args(info)?;
        let id = e.get(e.get(state, c"config")?, c"provider")?;
        let runtime = e.get(state, c"runtime")?;
        let result = obj(e)?;
        e.set(result, c"providerId", id)?;
        let configured = e.kind(check)? == 6;
        let boolean = |v| jsparse(e, e.text(if v { "true" } else { "false" })?);
        e.set(result, c"configured", boolean(configured)?)?;
        if configured {
            let method = e.get(check, c"type")?;
            e.set(result, c"method", method)?;
            let status = e.call(runtime, c"getProviderAuthStatus", &[id])?;
            let label = e.get(status, c"label")?;
            if e.kind(label)? == 4 {
                e.set(result, c"label", label)?;
            } else {
                e.set(result, c"label", e.get(check, c"source")?)?;
            }
            let mut subscription = false;
            if e.json(method)? == br#""oauth""# {
                let selected = e.call(runtime, c"getProvider", &[id])?;
                if e.kind(selected)? == 6 {
                    let oauth = e.get(e.get(selected, c"auth")?, c"oauth")?;
                    if e.kind(oauth)? == 6 {
                        let flag = e.get(oauth, c"isSubscription")?;
                        subscription = e.kind(flag)? == 2 && e.json(flag)? == b"true";
                    }
                }
            }
            e.set(result, c"subscription", boolean(subscription)?)?;
        }
        frame("STATUS", &e.json(result)?);
        Ok(null_mut())
    })
}
extern "C" fn dispatch(env: Handle, info: Handle) -> Handle {
    handle(env, |e| {
        let (state, [runtime]) = e.args(info)?;
        let cfg = e.get(state, c"config")?;
        let op = e.json(e.get(cfg, c"op")?)?;
        if op == br#""catalog""# {
            return pi_catalog::open(e, state, runtime);
        }
        if op == br#""menu""# {
            let rows = e.call(runtime, c"getProviders", &[])?;
            frame(
                "MENU",
                &e.json(e.call(rows, c"map", &[e.function(c"publicProvider", provider)?])?)?,
            );
            return Ok(null_mut());
        }
        let id = e.get(cfg, c"provider")?;
        if op == br#""status""# {
            e.set(state, c"runtime", runtime)?;
            let p = e.call(runtime, c"checkAuth", &[id])?;
            return on_promise(e, state, p, report_status);
        }
        if op == br#""logout""# {
            let p = e.call(runtime, c"logout", &[id])?;
            return on_promise(e, state, p, done);
        }
        let method = e.get(cfg, c"method")?;
        let selected = e.call(runtime, c"getProvider", &[id])?;
        if e.kind(selected)? != 6 {
            frame("ERROR", b"unknown");
            return Ok(null_mut());
        }
        let key = if e.json(method)? == br#""oauth""# {
            c"oauth"
        } else {
            c"apiKey"
        };
        let entry = e.get(e.get(selected, c"auth")?, key)?;
        if e.kind(entry)? != 6 || (key == c"apiKey" && e.kind(e.get(entry, c"login")?)? == 0) {
            frame("ERROR", b"method");
            return Ok(null_mut());
        }
        let controller = construct(e, c"AbortController", &[])?;
        e.set(state, c"controller", controller)?;
        e.set(state, c"pending", construct(e, c"Map", &[])?)?;
        start_input(e, state)?;
        let interaction = obj(e)?;
        e.set(interaction, c"signal", e.get(controller, c"signal")?)?;
        e.set(interaction, c"notify", bound(e, state, c"onEvent", notify)?)?;
        e.set(
            interaction,
            c"prompt",
            bound(e, state, c"onPrompt", prompt)?,
        )?;
        let opts = obj(e)?;
        prop(e, cfg, opts, c"agentName")?;
        let device_fn = if e.json(e.get(cfg, c"hostDevice")?)? == b"true" {
            e.function(c"getDeviceId", device)?
        } else {
            let settings = e.get(state, c"settings")?;
            e.call(
                e.get(settings, c"getOrCreateDeviceId")?,
                c"bind",
                &[settings],
            )?
        };
        e.set(opts, c"getDeviceId", device_fn)?;
        let p = e.call(runtime, c"login", &[id, method, interaction, opts])?;
        on_promise(e, state, p, done)
    })
}

#[unsafe(no_mangle)]
extern "C" fn napi_register_module_v1(env: Handle, exports: Handle) -> Handle {
    handle(env, |e| {
        let sdk = std::env::var("KAR_RS_PI_AUTH_SDK").map_err(|_| "sdk")?;
        let g = e.global()?;
        let process = e.get(g, c"process")?;
        let module = e.call(process, c"getBuiltinModule", &[e.text("node:module")?])?;
        let require = e.call(module, c"createRequire", &[e.text(&sdk)?])?;
        let load = |s: &str| e.call(require, c"call", &[module, e.text(s)?]);
        let package = load(&sdk)?;
        let model = e.get(package, c"ModelRuntime")?;
        if e.kind(e.get(model, c"create")?)? != 7 {
            return Err("sdk");
        }
        let http = load("./core/http-dispatcher.js")?;
        frame("READY", b"");
        let start = input_line().ok_or("configuration")?;
        let cfg = jsparse(e, e.text(&start)?)?;
        let op = e.json(e.get(cfg, c"op")?)?;
        if op == br#""menu""# {
            // Enumerating login methods needs the models configuration only.
            // Opening the real AuthStorage would create an empty auth.json.
            let storage = load("./core/auth-storage.js")?;
            let credentials = e.call(e.get(storage, c"AuthStorage")?, c"inMemory", &[])?;
            e.set(cfg, c"credentials", credentials)?;
        }
        let settings = e.call(
            e.get(package, c"SettingsManager")?,
            c"create",
            &[
                e.call(process, c"cwd", &[])?,
                e.get(cfg, c"agentDir")?,
                jsparse(
                    e,
                    e.text(if op == br#""picker""# {
                        r#"{"projectTrusted":true}"#
                    } else {
                        r#"{"projectTrusted":false}"#
                    })?,
                )?,
            ],
        )?;
        let global = e.call(settings, c"getGlobalSettings", &[])?;
        e.call(
            http,
            c"applyHttpProxySettings",
            &[e.get(global, c"httpProxy")?],
        )?;
        e.call(
            http,
            c"configureHttpDispatcher",
            &[e.call(settings, c"getHttpIdleTimeoutMs", &[])?],
        )?;
        let state = obj(e)?;
        e.set(state, c"config", cfg)?;
        e.set(state, c"settings", settings)?;
        e.set(state, c"package", package)?;
        if op == br#""picker""# {
            e.set(state, c"piAi", load("@earendil-works/pi-ai")?)?;
            pi_picker::start(e, state)?;
            return Ok(exports);
        }
        let p = e.call(model, c"create", &[cfg])?;
        on_promise(e, state, p, dispatch)?;
        Ok(exports)
    })
}
