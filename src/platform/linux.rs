//! Linux-specific process info unavailable from sysinfo.

/// Controlling terminal of `pid` (e.g. `"pts/3"`), from `/proc/<pid>/stat`.
/// No subprocess spawning.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn tty_of(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tty_nr = parse_tty_nr(&stat)?;
    name_of(tty_nr).or_else(|| fd0_dev_name(pid))
}

/// Field 7 of `/proc/<pid>/stat` after the comm in parentheses.
pub(crate) fn parse_tty_nr(stat: &str) -> Option<i32> {
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(4)?.parse().ok()
}

/// Linux packs tty major in bits 15–8; minor in 31–20 and 7–0.
pub(crate) fn decode_dev(tty_nr: i32) -> (u32, u32) {
    let n = tty_nr as u32;
    let major = (n >> 8) & 0xff;
    let minor = (n & 0xff) | ((n >> 20) & 0xfff) << 8;
    (major, minor)
}

fn name_of(tty_nr: i32) -> Option<String> {
    if tty_nr == 0 {
        return None;
    }
    let (major, minor) = decode_dev(tty_nr);
    Some(match major {
        // UNIX98 PTY slaves: /dev/pts/N
        136..=143 => format!("pts/{minor}"),
        // Virtual consoles /dev/ttyN and serial /dev/ttySN
        4 if minor < 64 => format!("tty{minor}"),
        4 => format!("ttyS{}", minor - 64),
        _ => return None,
    })
}

fn fd0_dev_name(pid: u32) -> Option<String> {
    let link = std::fs::read_link(format!("/proc/{pid}/fd/0")).ok()?;
    link.to_str()?.strip_prefix("/dev/").map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_comm_with_parens() {
        let stat = "42 (weird ) name) S 1 1 1 34818 1 0 0";
        assert_eq!(parse_tty_nr(stat), Some(34818));
    }

    #[test]
    fn zero_tty_is_none() {
        assert_eq!(name_of(0), None);
        assert_eq!(parse_tty_nr("1 (init) S 0 0 0 0 0"), Some(0));
    }

    #[test]
    fn pts_from_unix98_major() {
        // major 136, minor 2 → (136 << 8) | 2
        assert_eq!(name_of((136 << 8) | 2), Some("pts/2".into()));
        assert_eq!(decode_dev((136 << 8) | 2), (136, 2));
    }

    #[test]
    fn console_and_serial() {
        assert_eq!(name_of(4 << 8 | 1), Some("tty1".into()));
        assert_eq!(name_of(4 << 8 | 64), Some("ttyS0".into()));
    }
}
