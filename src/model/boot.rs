// ponytail: foundation types consumed by steps 2–4 of the ownership plan;
// dead until wired into the collector.
#![allow(dead_code)]

//! Boot identity and the platform boot fingerprint it resolves from.
//!
//! A `ProcessIdentity` is only meaningful while the machine boot it belongs
//! to is unambiguous. `pid`/`start_time` alone collide across reboots, so
//! every identity carries a `BootId`.

use std::fmt;
use std::io::{self, Read};

/// Stable identity of one machine boot.
///
/// Disambiguates identical PID/start-time combinations across reboots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BootId(u128);

impl BootId {
    pub fn from_u128(id: u128) -> Self {
        BootId(id)
    }

    pub fn from_le_bytes(b: [u8; 16]) -> Self {
        BootId(u128::from_le_bytes(b))
    }

    pub fn to_le_bytes(self) -> [u8; 16] {
        self.0.to_le_bytes()
    }

    /// Fresh, best-effort-unique id. Not security-critical: uniqueness across
    /// a machine's boots is all that matters. Prefers OS entropy; falls back
    /// to time+pid.
    pub fn random() -> Self {
        let mut b = [0u8; 16];
        let urandom = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
        if urandom.is_err() {
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
                ^ ((std::process::id() as u64) << 32);
            for chunk in b.chunks_mut(8) {
                chunk.copy_from_slice(&seed.to_le_bytes());
            }
        }
        BootId::from_le_bytes(b)
    }
}

impl fmt::Display for BootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// Platform boot fingerprint for the current boot.
///
/// Not itself a stable `BootId`: Linux's UUID is stable per boot and used
/// directly; macOS's `kern.boottime` is a timestamp that can collide and must
/// be mapped to a persisted UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BootEpoch {
    /// Linux `/proc/sys/kernel/random/boot_id`, as an unsigned 128-bit int.
    Linux(u128),
    /// macOS `kern.boottime`: seconds and microseconds since the Unix epoch.
    Macos { sec: i64, usec: i32 },
}

impl BootEpoch {
    /// Resolve this epoch to a stable `BootId`.
    ///
    /// Linux uses the boot UUID directly. macOS needs a persisted
    /// UUID↔epoch mapping, supplied through `resolve` (backed by
    /// `RuntimeStore` at step 7); `resolve` is not called on Linux.
    pub fn to_boot_id(
        self,
        mut resolve: impl FnMut(BootEpoch) -> io::Result<BootId>,
    ) -> io::Result<BootId> {
        match self {
            BootEpoch::Linux(id) => Ok(BootId::from_u128(id)),
            BootEpoch::Macos { .. } => resolve(self),
        }
    }

    /// Canonical byte form for persistence (tag + payload, little-endian).
    pub fn to_bytes(self) -> Vec<u8> {
        let mut v = Vec::with_capacity(17);
        match self {
            BootEpoch::Linux(id) => {
                v.push(0);
                v.extend_from_slice(&id.to_le_bytes());
            }
            BootEpoch::Macos { sec, usec } => {
                v.push(1);
                v.extend_from_slice(&sec.to_le_bytes());
                v.extend_from_slice(&usec.to_le_bytes());
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn linux_epoch_uses_uuid_directly() {
        let epoch = BootEpoch::Linux(0x0123456789abcdef0123456789abcdef);
        let mut resolved = false;
        let id = epoch
            .to_boot_id(|_| {
                resolved = true;
                Ok(BootId::random())
            })
            .unwrap();
        assert!(!resolved, "Linux must not consult the resolver");
        assert_eq!(id, BootId::from_u128(0x0123456789abcdef0123456789abcdef));
    }

    #[test]
    fn macos_epoch_routes_through_resolver_and_is_stable() {
        // Simulate the step-7 RuntimeStore: a persisted UUID per boot epoch.
        let mut map: HashMap<BootEpoch, BootId> = HashMap::new();
        let mut resolve = |epoch: BootEpoch| -> io::Result<BootId> {
            Ok(*map.entry(epoch).or_insert_with(BootId::random))
        };

        let boot_a = BootEpoch::Macos { sec: 1000, usec: 5 };
        let boot_b = BootEpoch::Macos { sec: 2000, usec: 0 };

        let a1 = boot_a.to_boot_id(&mut resolve).unwrap();
        let a2 = boot_a.to_boot_id(&mut resolve).unwrap();
        let b1 = boot_b.to_boot_id(&mut resolve).unwrap();

        assert_eq!(a1, a2, "same boot epoch → same BootId");
        assert_ne!(a1, b1, "different boot epoch → different BootId");
    }

    #[test]
    fn random_ids_are_distinct() {
        assert_ne!(BootId::random(), BootId::random());
    }
}
