// SPDX-License-Identifier: GPL-3.0-or-later
//! Everything of the engine that touches Bluetooth: the AAP socket,
//! BlueZ and the passive advertisement monitor. Kept apart from `podsd` so that the daemon logic is tested
//! against fakes ([`AapConnector`], a fake BlueZ on a private bus).

pub mod battery;
pub mod bluez;
pub mod l2cap;
pub mod monitor;
pub mod transport;

pub use battery::BatteryProvider;
pub use bluez::{Bluez, BluezDevice, BluezEvent};
pub use l2cap::forbid_real_bluetooth;
pub use monitor::BleEvent;
pub use transport::{AapConnector, AapLink, BoxFuture, L2capConnector};
