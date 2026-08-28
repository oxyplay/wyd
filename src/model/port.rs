use std::net::IpAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Protocol {
    Tcp,
}

impl Protocol {
    /// Stable lowercase protocol string for JSON/web output. NOT Rust Debug.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListeningPort {
    pub protocol: Protocol,
    pub address: IpAddr,
    pub port: u16,
    pub pid: u32,
}

impl ListeningPort {
    pub fn label(&self) -> String {
        format!(":{}", self.port)
    }

    /// Loopback/unspecified → 127.0.0.1 so the browser opens the local server.
    pub fn url(&self) -> String {
        let host = match self.address {
            IpAddr::V4(a) if a.is_unspecified() || a.is_loopback() => "127.0.0.1".into(),
            IpAddr::V6(a) if a.is_unspecified() || a.is_loopback() => "127.0.0.1".into(),
            IpAddr::V6(a) => format!("[{a}]"),
            IpAddr::V4(a) => a.to_string(),
        };
        format!("http://{host}:{}", self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn p(addr: IpAddr, port: u16) -> ListeningPort {
        ListeningPort {
            protocol: Protocol::Tcp,
            address: addr,
            port,
            pid: 1,
        }
    }

    #[test]
    fn url_is_clickable_localhost() {
        assert_eq!(
            p(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 5555).url(),
            "http://127.0.0.1:5555"
        );
        assert_eq!(
            p(IpAddr::V4(Ipv4Addr::LOCALHOST), 3000).url(),
            "http://127.0.0.1:3000"
        );
        assert_eq!(
            p(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 5173).url(),
            "http://127.0.0.1:5173"
        );
    }
}
