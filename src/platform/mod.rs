#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::tty_of;

#[cfg(not(target_os = "macos"))]
pub fn tty_of(_pid: u32) -> Option<String> {
    None
}
