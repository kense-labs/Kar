//! Wrap the root SDK agent before Pi starts extensions. The private socket only
//! serves the Rust controller. Neither RPC stdout nor Pi's history carries bodies.
mod node_api;
use node_api::{Env, Handle, Read};
use std::{
    io::{Read as _, Write},
    net::TcpStream,
    ptr::null_mut,
    sync::{Arc, Mutex, OnceLock},
};

enum Binding {
    Missing,
    UnsupportedApi,
    Ready,
}

struct Latest {
    generation: u64,
    binding: Binding,
    bytes: Arc<[u8]>,
}
static STATE: OnceLock<Mutex<Latest>> = OnceLock::new();

fn state() -> &'static Mutex<Latest> {
    STATE.get_or_init(|| {
        Mutex::new(Latest {
            generation: 0,
            binding: Binding::Missing,
            bytes: Arc::from(&b"null"[..]),
        })
    })
}

fn serve() -> Read<()> {
    let port: u16 = std::env::var("KAR_RS_PI_CAPTURE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or("port")?;
    let token = std::env::var("KAR_RS_PI_CAPTURE_TOKEN").map_err(|_| "token")?;
    let mut stream =
        TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).map_err(|_| "connect")?;
    stream
        .write_all(token.as_bytes())
        .map_err(|_| "handshake")?;
    std::thread::Builder::new()
        .name("kar-pi-requests".into())
        .spawn(move || {
            let mut query = [0];
            while stream.read_exact(&mut query).is_ok() {
                let bytes = {
                    let latest = state().lock().unwrap();
                    match query[0] {
                        b'V' if matches!(latest.binding, Binding::Missing) => {
                            Arc::from(&b"!unbound"[..])
                        }
                        b'V' if matches!(latest.binding, Binding::UnsupportedApi) => {
                            Arc::from(&b"!api"[..])
                        }
                        b'V' => Arc::from(&b"null"[..]),
                        b'R' => latest.bytes.clone(),
                        _ => break,
                    }
                };
                if stream
                    .write_all(&(bytes.len() as u64).to_be_bytes())
                    .is_err()
                    || stream.write_all(&bytes).is_err()
                {
                    break;
                }
            }
        })
        .map_err(|_| "reader")?;
    Ok(())
}

fn proxy(e: &Env, target: Handle, apply: Handle) -> Read<Handle> {
    let global = e.global()?;
    let handler = e.call(global, c"Object", &[])?;
    e.set(handler, c"apply", apply)?;
    let args = e.call(global, c"Array", &[target, handler])?;
    e.call(
        e.get(global, c"Reflect")?,
        c"construct",
        &[e.get(global, c"Proxy")?, args],
    )
}
fn invoke(e: &Env, target: Handle, receiver: Handle, args: Handle) -> Read<Handle> {
    e.call(
        e.get(e.global()?, c"Reflect")?,
        c"apply",
        &[target, receiver, args],
    )
}
fn supported(e: &Env, model: Handle) -> bool {
    let api = e.get(model, c"api").and_then(|v| e.json(v));
    match api {
        Ok(api) => matches!(
            api.as_slice(),
            br#""anthropic-messages""# | br#""openai-completions""# | br#""openai-responses""#
        ),
        Err(_) => {
            e.clear_exception();
            false
        }
    }
}

extern "C" fn after_transform(env: Handle, info: Handle) -> Handle {
    let e = Env(env);
    let Ok((generation, [payload, model, result])) = e.args(info) else {
        return null_mut();
    };
    let snapshot = || -> Read<Vec<u8>> {
        if !supported(&e, model) {
            return Ok(b"!api".to_vec());
        }
        let body = if e.kind(result)? == 0 {
            payload
        } else {
            result
        };
        let global = e.global()?;
        let meta = e.call(global, c"Object", &[])?;
        for (from, to) in [
            (c"api", c"api"),
            (c"provider", c"provider"),
            (c"id", c"model"),
        ] {
            e.set(meta, to, e.get(model, from)?)?;
        }
        e.set(meta, c"at", e.call(e.get(global, c"Date")?, c"now", &[])?)?;
        // Stringify the body by itself, as Pi does, including a custom toJSON.
        let mut bytes = e.json(meta)?;
        bytes.push(b'\n');
        bytes.extend(e.json_with_limit(body, usize::MAX)?);
        Ok(bytes)
    };
    let bytes = snapshot().unwrap_or_else(|_| {
        e.clear_exception();
        b"!json".to_vec()
    });
    if let Ok(gen_value) = e.get(generation, c"id").and_then(|v| e.json(v)) {
        let mut s = state().lock().unwrap();
        if gen_value == s.generation.to_string().as_bytes() {
            s.bytes = bytes.into();
        }
    }
    result
}
extern "C" fn unmodified(_: Handle, _: Handle) -> Handle {
    null_mut()
}

extern "C" fn payload(env: Handle, info: Handle) -> Handle {
    let e = Env(env);
    (|| {
        let (generation, [target, receiver, args]) = e.args(info)?;
        let payload = e.get(args, c"0")?;
        let model = e.get(args, c"1")?;
        let result = invoke(&e, target, receiver, args)?;
        let promise = e.call(e.get(e.global()?, c"Promise")?, c"resolve", &[result])?;
        let after = e.call(
            e.function(c"karAfterTransform", after_transform)?,
            c"bind",
            &[generation, payload, model],
        )?;
        e.call(promise, c"then", &[after])
    })()
    .unwrap_or(null_mut())
}

extern "C" fn our_extension(env: Handle, info: Handle) -> Handle {
    let e = Env(env);
    (|| {
        let (config, [extension]) = e.args(info)?;
        e.call(
            e.get(e.global()?, c"Object")?,
            c"is",
            &[e.get(extension, c"resolvedPath")?, e.get(config, c"file")?],
        )
    })()
    .unwrap_or(null_mut())
}
extern "C" fn bind_root(env: Handle, info: Handle) -> Handle {
    let e = Env(env);
    let Ok((config, [target, session, args])) = e.args(info) else {
        return null_mut();
    };
    let install = || -> Read<bool> {
        let bindings = e.get(args, c"0")?;
        if e.kind(bindings)? != 6 {
            return Ok(false);
        }
        let mode = e.get(bindings, c"mode")?;
        if e.kind(mode)? != 4 || e.json(mode)? != br#""rpc""# {
            return Ok(false);
        }
        let exts = e.call(e.get(session, c"resourceLoader")?, c"getExtensions", &[])?;
        let matcher = e.call(
            e.function(c"karExtension", our_extension)?,
            c"bind",
            &[config],
        )?;
        if e.json(e.call(e.get(exts, c"extensions")?, c"some", &[matcher])?)? != b"true" {
            return Ok(false);
        }
        let agent = e.get(session, c"agent")?;
        let installed = e.get(config, c"agents")?;
        if e.json(e.call(installed, c"has", &[agent])?)? == b"true" {
            return Ok(false);
        }
        let model = e.get(session, c"model")?;
        let ready = e.kind(model)? == 6 && supported(&e, model);
        let generation = {
            let mut s = state().lock().unwrap();
            s.generation += 1;
            s.binding = if ready {
                Binding::Missing
            } else {
                Binding::UnsupportedApi
            };
            s.bytes = Arc::from(&b"null"[..]);
            s.generation
        };
        if !ready {
            return Ok(true);
        }
        let global = e.global()?;
        let label = e.call(global, c"Object", &[])?;
        e.set(
            label,
            c"id",
            e.call(global, c"Number", &[e.text(&generation.to_string())?])?,
        )?;
        let apply = e.call(e.function(c"karRootPayload", payload)?, c"bind", &[label])?;
        let original = e.get(agent, c"onPayload")?;
        let transform = if e.kind(original)? == 0 {
            e.function(c"karUnmodifiedPayload", unmodified)?
        } else {
            original
        };
        let wrapped = proxy(&e, transform, apply)?;
        e.set(agent, c"onPayload", wrapped)?;
        e.call(installed, c"add", &[agent])?;
        state().lock().unwrap().binding = Binding::Ready;
        Ok(false)
    };
    match install() {
        // Opening reports Unsupported before an extension can prompt with this model.
        Ok(true) => e
            .global()
            .and_then(|g| e.get(g, c"Promise"))
            .and_then(|p| e.call(p, c"resolve", &[]))
            .unwrap_or(null_mut()),
        result => {
            if result.is_err() {
                e.clear_exception();
            }
            invoke(&e, target, session, args).unwrap_or(null_mut())
        }
    }
}

extern "C" fn factory(env: Handle, info: Handle) -> Handle {
    let e = Env(env);
    static CONNECTED: OnceLock<bool> = OnceLock::new();
    let install = || -> Read<()> {
        let (config, []) = e.args(info)?;
        if !*CONNECTED.get_or_init(|| serve().is_ok()) {
            return Err("socket");
        }
        let global = e.global()?;
        if e.kind(e.get(config, c"file")?)? == 0 {
            let sdk = e.text(&std::env::var("KAR_RS_PI_CAPTURE_SDK").map_err(|_| "sdk")?)?;
            let process = e.get(global, c"process")?;
            let vars = e.get(process, c"env")?;
            let reflect = e.get(global, c"Reflect")?;
            for key in ["KAR_RS_PI_CAPTURE_PORT", "KAR_RS_PI_CAPTURE_TOKEN"] {
                e.call(reflect, c"deleteProperty", &[vars, e.text(key)?])?;
            }
            let module = e.call(process, c"getBuiltinModule", &[e.text("node:module")?])?;
            let require = e.call(module, c"createRequire", &[sdk])?;
            let exports = e.call(require, c"call", &[module, sdk])?;
            let prototype = e.get(e.get(exports, c"AgentSession")?, c"prototype")?;
            let agents = e.call(
                e.get(global, c"Reflect")?,
                c"construct",
                &[e.get(global, c"WeakSet")?, e.call(global, c"Array", &[])?],
            )?;
            e.set(config, c"agents", agents)?;
            let apply = e.call(e.function(c"karBindRoot", bind_root)?, c"bind", &[config])?;
            let binding = proxy(&e, e.get(prototype, c"bindExtensions")?, apply)?;
            e.set(prototype, c"bindExtensions", binding)?;
            // Check the exact extension file registered on this resource loader.
            // Other agents in the same SDK process can also bind in RPC mode.
            let file = std::env::var("KAR_RS_PI_CAPTURE_PLUGIN").map_err(|_| "plugin")?;
            e.set(config, c"file", e.text(&file)?)?;
        }
        Ok(())
    };
    if install().is_err() {
        e.clear_exception();
    }
    null_mut()
}

#[unsafe(no_mangle)]
extern "C" fn napi_register_module_v1(env: Handle, _: Handle) -> Handle {
    let e = Env(env);
    (|| {
        let config = e.call(e.global()?, c"Object", &[])?;
        e.call(
            e.function(c"piRequestObserver", factory)?,
            c"bind",
            &[config],
        )
    })()
    .unwrap_or(null_mut())
}
