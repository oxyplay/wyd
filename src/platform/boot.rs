//! Platform boot fingerprint (`BootEpoch`) for resolving a stable `BootId`.
//!
//! Follows the platform split of `tty_of`: Linux reads `/proc`, macOS uses
//! `sysctl`. No subprocess spawning.

use crate::model::boot::BootEpoch;
use std::io;

/// Supplies the platform boot fingerprint. Resolution to a stable `BootId`
/// (and any persistence) is left to the caller, so the `RuntimeStore` plugs
/// in later without touching platform code.
pub trait BootIdentityProvider {
    fn current_boot_epoch(&self) -> io::Result<BootEpoch>;
}

#[cfg(target_os = "linux")]
pub struct SystemBoot;

#[cfg(target_os = "linux")]
impl BootIdentityProvider for SystemBoot {
    fn current_boot_epoch(&self) -> io::Result<BootEpoch> {
        let raw = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        Ok(BootEpoch::Linux(parse_boot_id(&raw)?))
    }
}

#[cfg(target_os = "macos")]
pub struct SystemBoot;

#[cfg(target_os = "macos")]
impl BootIdentityProvider for SystemBoot {
    fn current_boot_epoch(&self) -> io::Result<BootEpoch> {
        // kern.bootsessionuuid is a stable per-boot UUID, NOT clock-derived —
        // an NTP clock adjustment no longer reads as a new boot. Fall back to
        // kern.boottime (clock-derived, needs the persisted mapping) only if
        // the UUID is unavailable.
        if let Ok(uuid) = read_bootsession_uuid() {
            return Ok(BootEpoch::MacosUuid(uuid));
        }
        read_kern_boottime().map(|(sec, usec)| BootEpoch::Macos { sec, usec })
    }
}

/// Stable per-boot UUID from `kern.bootsessionuuid`.
#[cfg(target_os = "macos")]
fn read_bootsession_uuid() -> io::Result<u128> {
    let mut buf = [0u8; 64];
    let mut len = buf.len() as libc::size_t;
    // SAFETY: `buf` is a valid out-buffer; `len` is its capacity and is
    // updated with the bytes written.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let s = std::ffi::CStr::from_bytes_until_nul(&buf[..len])
        .map_err(|_| io::Error::other("malformed bootsessionuuid"))?;
    parse_boot_id(s.to_string_lossy().as_ref())
}

/// Clock-derived `kern.boottime` (fallback only).
#[cfg(target_os = "macos")]
fn read_kern_boottime() -> io::Result<(i64, i32)> {
    let mut tv: libc::timeval = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::timeval>() as libc::size_t;
    // SAFETY: `tv` is a valid out-buffer of exactly the returned size.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&mut tv as *mut libc::timeval).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((tv.tv_sec, tv.tv_usec))
}

/// Parse a hyphenated 128-bit UUID (Linux `boot_id`, macOS
/// `bootsessionuuid`) into an unsigned integer.
pub(crate) fn parse_boot_id(s: &str) -> io::Result<u128> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed boot_id",
        ));
    }
    u128::from_str_radix(&hex, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "malformed boot_id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real platform provider must be stable within a single run (a
    /// machine does not reboot mid-scan) and must be resolvable.
    #[test]
    fn system_boot_is_stable_within_run() {
        let a = SystemBoot.current_boot_epoch().unwrap();
        let b = SystemBoot.current_boot_epoch().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn parses_hyphenated_boot_uuid() {
        let id = parse_boot_id("6fbe5d5c-a1a2-4b3c-9d4e-5f6a7b8c9d0e\n").unwrap();
        assert_eq!(id, 0x6fbe5d5ca1a24b3c9d4e5f6a7b8c9d0e);
    }

    #[test]
    fn rejects_malformed_uuid() {
        assert!(parse_boot_id("zzzz").is_err());
        assert!(parse_boot_id("").is_err());
        // 31 hex digits, not 32.
        assert!(parse_boot_id("0123456789abcdef0123456789abcde").is_err());
    }

    #[test]
    fn parses_unhyphenated_uuid() {
        let id = parse_boot_id("6fbe5d5ca1a24b3c9d4e5f6a7b8c9d0e").unwrap();
        assert_eq!(id, 0x6fbe5d5ca1a24b3c9d4e5f6a7b8c9d0e);
    }

    /// NTP regression: `kern.boottime` is clock-derived (clock − uptime), so
    /// an NTP clock adjustment used to read as a new boot — ending every
    /// recorded session. The provider must use the stable per-boot
    /// `kern.bootsessionuuid`; the clock-derived fallback is only a fallback.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_prefers_stable_bootsessionuuid_over_boottime() {
        let epoch = SystemBoot
            .current_boot_epoch()
            .expect("boot epoch readable");
        assert!(
            matches!(epoch, BootEpoch::MacosUuid(_)),
            "macOS must report the stable per-boot UUID, got clock-derived {epoch:?}"
        );
    }
}
