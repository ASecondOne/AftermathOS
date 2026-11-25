//! Rust PAM session module. Authentication/account policy belongs to PAM;
//! this module registers an authenticated session with the root engine.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

use aftermath_ctl::proto::{self, Request};
use serde_json::Value;

const SUCCESS: c_int = 0;
const SESSION_ERR: c_int = 14;
const KEY: &[u8] = b"aftermath.session.v1\0";

#[link(name = "pam")]
extern "C" {
    fn pam_get_item(handle: *const c_void, kind: c_int, value: *mut *const c_void) -> c_int;
    fn pam_get_data(handle: *const c_void, key: *const c_char, value: *mut *const c_void) -> c_int;
    fn pam_set_data(
        handle: *mut c_void,
        key: *const c_char,
        value: *mut c_void,
        cleanup: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, c_int)>,
    ) -> c_int;
    fn pam_putenv(handle: *mut c_void, assignment: *const c_char) -> c_int;
    fn pam_getenv(handle: *const c_void, name: *const c_char) -> *const c_char;
}

struct Data {
    id: String,
    owner: u32,
}

fn call(verb: &str, args: Vec<String>) -> Result<Value, String> {
    let r = proto::call_timeout(
        aftermath_ctl::SOCKET,
        &Request {
            subsystem: "sessions".into(),
            verb: verb.into(),
            args,
        },
        Duration::from_secs(5),
    )
    .map_err(|e| e.to_string())?;
    if r.ok {
        Ok(r.data)
    } else {
        Err(r
            .error
            .unwrap_or_else(|| "session registration refused".into()))
    }
}

unsafe fn item(handle: *mut c_void, kind: c_int) -> Result<String, String> {
    let mut value = std::ptr::null();
    // SAFETY: Linux-PAM owns the live handle and the returned NUL-terminated item.
    if unsafe { pam_get_item(handle, kind, &mut value) } != SUCCESS || value.is_null() {
        return Err("PAM user or terminal is missing".into());
    }
    unsafe { CStr::from_ptr(value.cast()) }
        .to_str()
        .map(str::to_owned)
        .map_err(|e| e.to_string())
}

unsafe fn data(handle: *mut c_void) -> Option<*const Data> {
    let mut value = std::ptr::null();
    // SAFETY: the key is static and data is only installed by this module.
    (unsafe { pam_get_data(handle, KEY.as_ptr().cast(), &mut value) } == SUCCESS
        && !value.is_null())
    .then_some(value.cast())
}

unsafe fn environment(handle: *mut c_void, key: &CStr) -> Option<String> {
    let value = unsafe { pam_getenv(handle, key.as_ptr()) };
    if value.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(value) }
            .to_str()
            .ok()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    }
}

unsafe extern "C" fn cleanup(_handle: *mut c_void, value: *mut c_void, _status: c_int) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if value.is_null() {
            return;
        }
        // SAFETY: this pointer came from Box::into_raw; PAM calls its cleanup once.
        let data = unsafe { Box::from_raw(value.cast::<Data>()) };
        // A forked login child may release an inherited PAM handle. Only the
        // original privileged session owner may close the engine registration.
        if data.owner == std::process::id() {
            let _ = call("close", vec![data.id.clone()]);
        }
    }));
}

fn boundary(work: impl FnOnce() -> Result<(), String>) -> c_int {
    match catch_unwind(AssertUnwindSafe(work)) {
        Ok(Ok(())) => SUCCESS,
        Ok(Err(e)) => {
            eprintln!("pam_aftermath: {e}");
            SESSION_ERR
        }
        Err(_) => {
            eprintln!("pam_aftermath: session module failed");
            SESSION_ERR
        }
    }
}

/// Open the session after authentication and account checks.
///
/// # Safety
/// Linux-PAM must supply a live PAM handle and valid argument pointers.
#[no_mangle]
pub unsafe extern "C" fn pam_sm_open_session(
    handle: *mut c_void,
    _flags: c_int,
    argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    boundary(|| {
        if handle.is_null() || argc != 0 {
            return Err("invalid PAM invocation or unsupported module options".into());
        }
        if unsafe { libc::geteuid() } != 0 {
            return Err("session registration requires the privileged PAM application".into());
        }
        if unsafe { data(handle) }.is_some() {
            return Ok(());
        }
        let user = unsafe { item(handle, 2) }?; // PAM_USER
        let tty = unsafe { item(handle, 3) }?; // PAM_TTY
        let native = unsafe { environment(handle, c"XDG_SESSION_ID") };
        let reply = if let Some(id) = native {
            call(
                "open-elogind",
                vec![user, std::process::id().to_string(), tty, id],
            )?
        } else {
            call("open", vec![user, std::process::id().to_string(), tty])?
        };
        let id = reply["id"]
            .as_str()
            .ok_or("engine reply has no session ID")?
            .to_owned();
        let result = (|| {
            let env = reply["environment"]
                .as_object()
                .ok_or("engine reply has no environment")?;
            for (key, value) in env {
                let value = value.as_str().ok_or("invalid session environment")?;
                let assignment =
                    CString::new(format!("{key}={value}")).map_err(|e| e.to_string())?;
                // pam_putenv copies this string; it does not retain its pointer.
                if unsafe { pam_putenv(handle, assignment.as_ptr()) } != SUCCESS {
                    return Err("PAM could not set the session environment".into());
                }
            }
            let pointer = Box::into_raw(Box::new(Data {
                id: id.clone(),
                owner: std::process::id(),
            }));
            if unsafe { pam_set_data(handle, KEY.as_ptr().cast(), pointer.cast(), Some(cleanup)) }
                != SUCCESS
            {
                unsafe {
                    drop(Box::from_raw(pointer));
                }
                return Err("PAM could not retain session state".into());
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = call("close", vec![id]);
        }
        result
    })
}

/// Close an existing session. Repeated closure is harmless in the engine.
///
/// # Safety
/// Linux-PAM must supply the live PAM handle used for session opening.
#[no_mangle]
pub unsafe extern "C" fn pam_sm_close_session(
    handle: *mut c_void,
    _flags: c_int,
    argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    boundary(|| {
        if handle.is_null() || argc != 0 {
            return Err("invalid PAM invocation".into());
        }
        if let Some(pointer) = unsafe { data(handle) } {
            let d = unsafe { &*pointer };
            if d.owner == std::process::id() {
                call("close", vec![d.id.clone()])?;
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ffi_boundary_rejects_invalid_handles_and_catches_panics() {
        assert_eq!(
            unsafe { pam_sm_open_session(std::ptr::null_mut(), 0, 0, std::ptr::null()) },
            SESSION_ERR
        );
        assert_eq!(
            unsafe { pam_sm_close_session(std::ptr::null_mut(), 0, 0, std::ptr::null()) },
            SESSION_ERR
        );
        assert_eq!(boundary(|| panic!("test panic")), SESSION_ERR);
        assert_eq!(boundary(|| Ok(())), SUCCESS);
    }
}
