#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::tty_of;

#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::tty_of;

mod boot;
pub use boot::{BootIdentityProvider, SystemBoot};

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn tty_of(_pid: u32) -> Option<String> {
    None
}
