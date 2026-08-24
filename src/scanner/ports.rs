use netstat2::{
    AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, SocketInfo, TcpState, get_sockets_info,
};

use crate::model::{ListeningPort, Protocol};
use crate::scanner::Result;

/// Listening TCP sockets via OS APIs — no `lsof`/`netstat` subprocess.
pub fn scan() -> Result<Vec<ListeningPort>> {
    let sockets = get_sockets_info(
        AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6,
        ProtocolFlags::TCP,
    )?;
    Ok(sockets.into_iter().flat_map(listening).collect())
}

fn listening(si: SocketInfo) -> Vec<ListeningPort> {
    let ProtocolSocketInfo::Tcp(tcp) = si.protocol_socket_info else {
        return Vec::new();
    };
    if tcp.state != TcpState::Listen {
        return Vec::new();
    }
    si.associated_pids
        .into_iter()
        .map(|pid| ListeningPort {
            protocol: Protocol::Tcp,
            address: tcp.local_addr,
            port: tcp.local_port,
            pid,
        })
        .collect()
}
