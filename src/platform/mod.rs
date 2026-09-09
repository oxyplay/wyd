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

pub mod pgroup;

#[cfg(target_os = "linux")]
pub mod cgroup;

/// Is a delegated cgroup v2 subtree available to this process? Probed once and
/// cached: the answer cannot change while we run, and probing touches the
/// filesystem.
#[cfg(target_os = "linux")]
pub fn cgroup_root() -> Option<&'static cgroup::CgroupRoot> {
    static ROOT: std::sync::LazyLock<Option<cgroup::CgroupRoot>> =
        std::sync::LazyLock::new(|| cgroup::discover().ok());
    ROOT.as_ref()
}

/// Always false off Linux: there is no cgroup v2 there.
#[cfg(not(target_os = "linux"))]
pub fn cgroup_root() -> Option<&'static ()> {
    None
}

/// True when hard limits can be enforced through a cgroup subtree.
pub fn cgroup_delegated() -> bool {
    cgroup_root().is_some()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn tty_of(_pid: u32) -> Option<String> {
    None
}
