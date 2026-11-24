//! Node-API values are valid only within the current vendor callback scope.
use std::ffi::{CStr, c_char, c_void};
use std::ptr::null_mut;

pub type Handle = *mut c_void;
type Callback = extern "C" fn(Handle, Handle) -> Handle;
type Status = i32;

// Node provides these symbols in the loading process. Windows needs to resolve
// them from the executable because there is no vendor import library to link.
#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> Handle;
    fn GetProcAddress(module: Handle, name: *const c_char) -> Handle;
}
macro_rules! napi {
    ($(fn $name:ident($($arg:ident: $ty:ty),*);)*) => {$(
        #[cfg(not(windows))]
        unsafe extern "C" { fn $name($($arg:$ty),*) -> Status; }
        #[cfg(windows)]
        unsafe fn $name($($arg:$ty),*) -> Status {
            let module = unsafe { GetModuleHandleW(std::ptr::null()) };
            let address = unsafe { GetProcAddress(module, concat!(stringify!($name), "\0").as_ptr().cast()) };
            if address.is_null() { return 9; }
            let call: unsafe extern "C" fn($($ty),*) -> Status = unsafe { std::mem::transmute(address) };
            unsafe { call($($arg),*) }
        }
    )*}
}
napi! {
    fn napi_get_cb_info(env: Handle, info: Handle, count: *mut usize, args: *mut Handle, this: *mut Handle, data: *mut Handle);
    fn napi_get_global(env: Handle, out: *mut Handle);
    fn napi_get_named_property(env: Handle, object: Handle, key: *const c_char, out: *mut Handle);
    fn napi_set_named_property(env: Handle, object: Handle, key: *const c_char, value: Handle);
    fn napi_typeof(env: Handle, value: Handle, kind: *mut i32);
    fn napi_create_string_utf8(env: Handle, bytes: *const c_char, len: usize, out: *mut Handle);
    fn napi_get_value_string_utf8(env: Handle, value: Handle, buf: *mut c_char, size: usize, copied: *mut usize);
    fn napi_create_function(env: Handle, name: *const c_char, len: usize, cb: Callback, data: Handle, out: *mut Handle);
    fn napi_call_function(env: Handle, receiver: Handle, function: Handle, count: usize, args: *const Handle, out: *mut Handle);
    fn napi_get_and_clear_last_exception(env: Handle, out: *mut Handle);
}
pub type Read<T> = Result<T, &'static str>;
pub struct Env(pub Handle);

fn ok(status: Status) -> Read<()> {
    if status == 0 {
        Ok(())
    } else {
        Err("query_failed")
    }
}
impl Env {
    pub fn args<const N: usize>(&self, info: Handle) -> Read<(Handle, [Handle; N])> {
        let mut args = [null_mut(); N];
        let mut this = null_mut();
        let mut count = N;
        ok(unsafe {
            napi_get_cb_info(
                self.0,
                info,
                &mut count,
                args.as_mut_ptr(),
                &mut this,
                null_mut(),
            )
        })?;
        if count < N {
            return Err("query_failed");
        }
        Ok((this, args))
    }
    pub fn get(&self, object: Handle, key: &CStr) -> Read<Handle> {
        let mut out = null_mut();
        ok(unsafe { napi_get_named_property(self.0, object, key.as_ptr(), &mut out) })?;
        Ok(out)
    }
    pub fn text(&self, bytes: &str) -> Read<Handle> {
        let mut out = null_mut();
        ok(unsafe {
            napi_create_string_utf8(self.0, bytes.as_ptr().cast(), bytes.len(), &mut out)
        })?;
        Ok(out)
    }
    pub fn function(&self, name: &CStr, cb: Callback) -> Read<Handle> {
        let mut out = null_mut();
        ok(unsafe {
            napi_create_function(
                self.0,
                name.as_ptr(),
                name.to_bytes().len(),
                cb,
                null_mut(),
                &mut out,
            )
        })?;
        Ok(out)
    }
    pub fn kind(&self, value: Handle) -> Read<i32> {
        let mut kind = 0;
        ok(unsafe { napi_typeof(self.0, value, &mut kind) })?;
        Ok(kind)
    }
    pub fn call(&self, receiver: Handle, name: &CStr, args: &[Handle]) -> Read<Handle> {
        let function = self.get(receiver, name)?;
        if self.kind(function)? != 7 {
            return Err("native_query_unavailable");
        } // napi_function
        let mut out = null_mut();
        ok(unsafe {
            napi_call_function(
                self.0,
                receiver,
                function,
                args.len(),
                args.as_ptr(),
                &mut out,
            )
        })?;
        Ok(out)
    }
    pub fn set(&self, object: Handle, key: &CStr, value: Handle) -> Read<()> {
        ok(unsafe { napi_set_named_property(self.0, object, key.as_ptr(), value) })
    }
    pub fn clear_exception(&self) {
        let mut exception = null_mut();
        unsafe { napi_get_and_clear_last_exception(self.0, &mut exception) };
    }
    pub fn global(&self) -> Read<Handle> {
        let mut global = null_mut();
        ok(unsafe { napi_get_global(self.0, &mut global) })?;
        Ok(global)
    }
    pub fn json(&self, value: Handle) -> Read<Vec<u8>> {
        self.json_with_limit(value, 8 * 1024 * 1024)
    }
    pub fn json_with_limit(&self, value: Handle, limit: usize) -> Read<Vec<u8>> {
        let text = self.call(self.get(self.global()?, c"JSON")?, c"stringify", &[value])?;
        self.string_bytes(text, limit)
    }
    pub fn string_bytes(&self, text: Handle, limit: usize) -> Read<Vec<u8>> {
        let mut len = 0;
        ok(unsafe { napi_get_value_string_utf8(self.0, text, null_mut(), 0, &mut len) })?;
        if len > limit {
            return Err("query_failed");
        }
        let mut bytes = vec![0; len + 1];
        let mut copied = 0;
        ok(unsafe {
            napi_get_value_string_utf8(
                self.0,
                text,
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                &mut copied,
            )
        })?;
        bytes.truncate(copied);
        Ok(bytes)
    }
}

#[allow(dead_code)]
pub mod numbers {
    use super::*;
    napi! {
        fn napi_get_value_double(env:Handle, value:Handle, out:*mut f64);
        fn napi_create_double(env:Handle, value:f64, out:*mut Handle);
    }
    pub fn increment(e: &Env, value: Handle) -> Read<Handle> {
        let mut base = 0.0;
        ok(unsafe { napi_get_value_double(e.0, value, &mut base) })?;
        let order = base + 1.0;
        if !order.is_finite() || order <= base {
            return Err("invalid native prompt order");
        }
        let mut out = null_mut();
        ok(unsafe { napi_create_double(e.0, order, &mut out) })?;
        Ok(out)
    }
}

#[allow(dead_code)]
pub mod arrays {
    use super::*;
    napi! {
        fn napi_get_array_length(env:Handle,array:Handle,out:*mut u32);
        fn napi_create_uint32(env:Handle,value:u32,out:*mut Handle);
    }
    pub fn len(e: &Env, array: Handle) -> Read<u32> {
        let mut length = 0;
        ok(unsafe { napi_get_array_length(e.0, array, &mut length) })?;
        Ok(length)
    }
    pub fn index(e: &Env, i: u32) -> Read<Handle> {
        let mut n = null_mut();
        ok(unsafe { napi_create_uint32(e.0, i, &mut n) })?;
        Ok(n)
    }
}

// Only the auth host needs cross-thread delivery. Node invokes `deliver` on its
// own event loop, with a fresh handle scope. The input thread never holds a JS value.
#[allow(dead_code)]
pub mod lines {
    use super::*;
    type Deliver = extern "C" fn(Handle, Handle, Handle, Handle);
    napi! {
        fn napi_create_threadsafe_function(env:Handle, callback:Handle, resource:Handle, name:Handle, limit:usize, threads:usize, data:Handle, finalizer:Handle, context:Handle, deliver:Deliver, out:*mut Handle);
        fn napi_call_threadsafe_function(function:Handle,data:Handle,mode:i32);
        fn napi_release_threadsafe_function(function:Handle,mode:i32);
    }
    pub struct Sender(usize);
    impl Sender {
        pub fn new(e: &Env, cb: Handle) -> Read<Self> {
            let mut out = null_mut();
            ok(unsafe {
                napi_create_threadsafe_function(
                    e.0,
                    cb,
                    null_mut(),
                    e.text("oar-pi-auth-input")?,
                    8,
                    1,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    deliver,
                    &mut out,
                )
            })?;
            Ok(Self(out as usize))
        }
        pub fn send(&self, line: String) -> bool {
            let data = Box::into_raw(Box::new(line));
            if unsafe { napi_call_threadsafe_function(self.0 as Handle, data.cast(), 1) } == 0 {
                return true;
            }
            drop(unsafe { Box::from_raw(data) });
            false
        }
    }
    impl Drop for Sender {
        fn drop(&mut self) {
            unsafe { napi_release_threadsafe_function(self.0 as Handle, 0) };
        }
    }
    extern "C" fn deliver(env: Handle, cb: Handle, _context: Handle, data: Handle) {
        if data.is_null() {
            return;
        }
        let line = unsafe { Box::from_raw(data.cast::<String>()) };
        if env.is_null() {
            return;
        }
        let e = Env(env);
        let _ = (|| e.call(cb, c"call", &[e.global()?, e.text(&line)?]))();
    }
}
