//! Password check through PAM. Only the auth stack runs, like other lockers:
//! `/etc/pam.d/nexus` when it exists, otherwise `login`.
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr,
};

const SUCCESS: c_int = 0;
const PERM_DENIED: c_int = 6;
const AUTH_ERR: c_int = 7;
const MAXTRIES: c_int = 11;
const BUF_ERR: c_int = 5;
const CONV_ERR: c_int = 19;
const REFRESH_CRED: c_int = 0x10;
const PROMPT_ECHO_OFF: c_int = 1;
const PROMPT_ECHO_ON: c_int = 2;
const ERROR_MSG: c_int = 3;
const TEXT_INFO: c_int = 4;
/// Linux-PAM never sends more messages at once.
const MAX_MESSAGES: c_int = 32;

#[repr(C)]
struct Message {
    style: c_int,
    msg: *const c_char,
}
#[repr(C)]
struct Response {
    resp: *mut c_char,
    retcode: c_int,
}
type Converse =
    unsafe extern "C" fn(c_int, *mut *const Message, *mut *mut Response, *mut c_void) -> c_int;
#[repr(C)]
struct Conversation {
    conv: Converse,
    data: *mut c_void,
}
#[repr(C)]
struct Handle {
    _private: [u8; 0],
}
#[link(name = "pam")]
unsafe extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conversation: *const Conversation,
        handle: *mut *mut Handle,
    ) -> c_int;
    fn pam_authenticate(handle: *mut Handle, flags: c_int) -> c_int;
    fn pam_setcred(handle: *mut Handle, flags: c_int) -> c_int;
    fn pam_strerror(handle: *mut Handle, status: c_int) -> *const c_char;
    fn pam_end(handle: *mut Handle, status: c_int) -> c_int;
}
// PAM frees the responses with free(), so they come from the C allocator.
unsafe extern "C" {
    fn calloc(count: usize, size: usize) -> *mut c_void;
    fn strdup(text: *const c_char) -> *mut c_char;
    fn free(memory: *mut c_void);
}

struct Data {
    password: CString,
    /// What PAM had to say, such as a faillock notice.
    messages: Vec<String>,
}
/// Overwrites a secret before its memory is released.
fn wipe(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: a valid, exclusive reference; volatile so the write is kept.
        unsafe { ptr::write_volatile(b, 0) };
    }
}
/// Answers every prompt with the password and keeps the informational messages.
unsafe extern "C" fn converse(
    count: c_int,
    messages: *mut *const Message,
    responses: *mut *mut Response,
    data: *mut c_void,
) -> c_int {
    if !(1..=MAX_MESSAGES).contains(&count) || messages.is_null() || responses.is_null() {
        return CONV_ERR;
    }
    // SAFETY: PAM passes back the `Data` given to pam_start and `count` valid
    // messages; the replies are allocated as PAM expects to free them.
    unsafe {
        let data = &mut *(data as *mut Data);
        let out = calloc(count as usize, size_of::<Response>()) as *mut Response;
        if out.is_null() {
            return BUF_ERR;
        }
        for i in 0..count as usize {
            let message = &**messages.add(i);
            match message.style {
                PROMPT_ECHO_OFF | PROMPT_ECHO_ON => {
                    let reply = strdup(data.password.as_ptr());
                    if reply.is_null() {
                        release(out, i);
                        return BUF_ERR;
                    }
                    (*out.add(i)).resp = reply;
                }
                ERROR_MSG | TEXT_INFO if !message.msg.is_null() => {
                    let text = CStr::from_ptr(message.msg).to_string_lossy();
                    if !text.trim().is_empty() {
                        data.messages.push(text.trim().to_owned());
                    }
                }
                ERROR_MSG | TEXT_INFO => {}
                _ => {
                    release(out, i);
                    return CONV_ERR;
                }
            }
        }
        *responses = out;
    }
    SUCCESS
}
/// Frees the first `filled` replies after wiping them, then the array.
unsafe fn release(out: *mut Response, filled: usize) {
    // SAFETY: `out` holds `filled` replies from strdup, from `converse`.
    unsafe {
        for i in 0..filled {
            let reply = (*out.add(i)).resp;
            if !reply.is_null() {
                wipe(std::slice::from_raw_parts_mut(
                    reply as *mut u8,
                    CStr::from_ptr(reply).count_bytes(),
                ));
                free(reply as *mut c_void);
            }
        }
        free(out as *mut c_void);
    }
}
fn service() -> &'static CStr {
    if std::path::Path::new("/etc/pam.d/nexus").exists() {
        c"nexus"
    } else {
        c"login"
    }
}
/// Checks the current user's password; the error is ready to show.
pub fn authenticate(password: String) -> Result<(), String> {
    let mut bytes = password.into_bytes();
    let password = CString::new(bytes.clone());
    wipe(&mut bytes);
    let password = password.map_err(|_| "Wrong password".to_string())?;
    let user = CString::new(gtk::glib::user_name().into_encoded_bytes())
        .map_err(|_| "Unknown user".to_string())?;
    let mut data = Box::new(Data {
        password,
        messages: vec![],
    });
    let conversation = Conversation {
        conv: converse,
        data: &mut *data as *mut Data as *mut c_void,
    };
    let mut handle = ptr::null_mut();
    // SAFETY: every pointer outlives the PAM transaction, which ends below.
    let (status, detail) = unsafe {
        let started = pam_start(
            service().as_ptr(),
            user.as_ptr(),
            &conversation,
            &mut handle,
        );
        if started != SUCCESS || handle.is_null() {
            (started, String::new())
        } else {
            let status = pam_authenticate(handle, 0);
            if status == SUCCESS {
                // Renews credentials such as Kerberos tickets; not required to unlock.
                pam_setcred(handle, REFRESH_CRED);
            }
            let detail = CStr::from_ptr(pam_strerror(handle, status))
                .to_string_lossy()
                .into_owned();
            pam_end(handle, status);
            (status, detail)
        }
    };
    let data = *data;
    wipe(&mut data.password.into_bytes());
    match status {
        SUCCESS => Ok(()),
        _ if !data.messages.is_empty() => Err(data.messages.join("\n")),
        AUTH_ERR | PERM_DENIED => Err("Wrong password".into()),
        MAXTRIES => Err("Too many attempts".into()),
        _ if detail.is_empty() => Err(format!("Authentication failed ({status})")),
        _ => Err(format!("Authentication failed: {detail}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conversation_answers_prompts_and_keeps_messages() {
        let mut data = Data {
            password: c"secret".into(),
            messages: vec![],
        };
        let prompts = [
            Message {
                style: PROMPT_ECHO_OFF,
                msg: c"Password: ".as_ptr(),
            },
            Message {
                style: ERROR_MSG,
                msg: c"The account is locked due to 3 failed logins.".as_ptr(),
            },
        ];
        let mut list: Vec<*const Message> = prompts.iter().map(|m| m as *const _).collect();
        let mut out = ptr::null_mut();
        unsafe {
            let status = converse(
                2,
                list.as_mut_ptr(),
                &mut out,
                &mut data as *mut Data as *mut c_void,
            );
            assert_eq!(status, SUCCESS);
            assert_eq!(CStr::from_ptr((*out).resp), c"secret");
            assert!((*out.add(1)).resp.is_null());
            release(out, 2);
            assert_eq!(
                converse(0, list.as_mut_ptr(), &mut out, ptr::null_mut()),
                CONV_ERR
            );
        }
        assert_eq!(
            data.messages,
            ["The account is locked due to 3 failed logins."]
        );
    }
    #[test]
    fn unknown_prompts_are_refused() {
        let mut data = Data {
            password: c"secret".into(),
            messages: vec![],
        };
        let binary = Message {
            style: 7,
            msg: ptr::null(),
        };
        let mut list = [&binary as *const Message];
        let mut out = ptr::null_mut();
        let status = unsafe {
            converse(
                1,
                list.as_mut_ptr(),
                &mut out,
                &mut data as *mut Data as *mut c_void,
            )
        };
        assert_eq!(status, CONV_ERR);
        assert!(out.is_null());
    }
}
