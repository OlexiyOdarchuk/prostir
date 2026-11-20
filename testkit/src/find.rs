//! Asking for PCs as a headset does (see `prostir_proto::discovery`): a query to an
//! address, and the answer, if one comes.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use prostir_proto::discovery::{Answer, Query};

use crate::Keys;

/// A query for every PC `keys` trusts and has the find secret of (each PC sends it in
/// the session), with the headset's `name` (empty unless it looks for a PC to pair
/// with).
pub fn query(keys: &Keys, name: &str) -> Query {
    let secrets = keys
        .trusted
        .entries()
        .iter()
        .filter_map(|(k, _)| keys.find_secrets.get(k));
    Query::new(secrets, name)
}

/// Sends `datagram` to `to` and waits up to `timeout` for what comes back; `None`
/// when nothing does.
pub fn send(to: SocketAddr, datagram: &[u8], timeout: Duration) -> Option<Vec<u8>> {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).ok()?;
    socket.send_to(datagram, to).ok()?;
    let until = Instant::now() + timeout;
    let mut buf = [0u8; 1500];
    loop {
        let left = until.checked_duration_since(Instant::now())?;
        socket
            .set_read_timeout(Some(left.max(Duration::from_millis(1))))
            .ok()?;
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if from == to => return Some(buf[..n].to_vec()),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// Asks the PC at `to` with `query`: its answer, if it gives one within `timeout`.
pub fn ask(to: SocketAddr, query: &Query, timeout: Duration) -> Option<Answer> {
    let back = send(to, &query.encode(), timeout)?;
    let answer = Answer::decode(&back)?;
    (answer.nonce == query.nonce).then_some(answer)
}
