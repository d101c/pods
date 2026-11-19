// SPDX-License-Identifier: GPL-3.0-or-later
//! The seam between the engine and the AAP socket, so the engine can be
//! driven by a fake pair of AirPods in tests.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::time::Duration;

use crate::l2cap::L2capSocket;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An open AAP channel: one packet per call each way.
pub trait AapLink: Send + Sync {
    fn send<'a>(&'a self, packet: &'a [u8]) -> BoxFuture<'a, io::Result<()>>;
    /// The next packet. End of stream is an `UnexpectedEof` error. Must be
    /// cancel-safe: dropping the future loses no packet.
    fn recv(&self) -> BoxFuture<'_, io::Result<Vec<u8>>>;
}

/// Opens AAP channels.
pub trait AapConnector: Send + Sync {
    fn connect<'a>(&'a self, address: &'a str) -> BoxFuture<'a, io::Result<Box<dyn AapLink>>>;
}

/// Connect deadline of the real socket.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// The real thing: L2CAP PSM 0x1001.
#[derive(Debug, Clone)]
pub struct L2capConnector {
    pub timeout: Duration,
}

impl Default for L2capConnector {
    fn default() -> Self {
        Self {
            timeout: CONNECT_TIMEOUT,
        }
    }
}

impl AapConnector for L2capConnector {
    fn connect<'a>(&'a self, address: &'a str) -> BoxFuture<'a, io::Result<Box<dyn AapLink>>> {
        Box::pin(async move {
            let socket = L2capSocket::connect(address, pods_proto::AAP_PSM, self.timeout).await?;
            Ok(Box::new(socket) as Box<dyn AapLink>)
        })
    }
}

impl AapLink for L2capSocket {
    fn send<'a>(&'a self, packet: &'a [u8]) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(L2capSocket::send(self, packet))
    }

    fn recv(&self) -> BoxFuture<'_, io::Result<Vec<u8>>> {
        Box::pin(L2capSocket::recv(self))
    }
}
