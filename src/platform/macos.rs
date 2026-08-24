//! macOS-specific process info unavailable from sysinfo.

/// Controlling terminal of `pid` (e.g. `"ttys003"`), via `proc_pidinfo`.
/// No subprocess spawning.
pub fn tty_of(pid: u32) -> Option<String> {
    use std::ffi::CStr;
    use std::mem::{MaybeUninit, size_of};

    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let wanted = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is an out-buffer of the exact `proc_bsdinfo` size;
    // flavor is `PROC_PIDTBSDINFO`. A stale pid returns `got != wanted`.
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            wanted,
        )
    };
    if got != wanted {
        return None;
    }

    // SAFETY: kernel wrote the full struct (`got == wanted`).
    let dev = unsafe { info.assume_init() }.e_tdev;
    if dev == u32::MAX {
        // NODEV: no controlling terminal.
        return None;
    }

    // SAFETY: `dev` is a character-device id. `devname` returns a pointer
    // into a libc-owned buffer valid until the next `devname` call on this
    // thread. The scanner is single-threaded; we copy the bytes immediately.
    let ptr = unsafe { libc::devname(dev as libc::dev_t, libc::S_IFCHR as libc::mode_t) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: non-null, NUL-terminated C string from `devname`.
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}
