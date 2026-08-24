use std::net::IpAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
}

/// A listening socket associated with a PID.
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
}
