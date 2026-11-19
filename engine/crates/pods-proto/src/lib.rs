// SPDX-License-Identifier: GPL-3.0-or-later
//! Apple Accessory Protocol (AAP) as spoken by AirPods over L2CAP PSM 0x1001.
//!
//! Pure code: bytes in, values out, values in, bytes out. Nothing here
//! opens a socket or touches the system, so all of it is tested on frames
//! captured from real AirPods (see `tests/m0_pro1.rs`).
//!
//! Origins (details in `engine/NOTICE`): the frame layout, battery and ear
//! decoding, AES, address resolution and the clear advertisement come from
//! podctl (MIT OR Apache-2.0); renaming, listening-mode configuration, the
//! device information layout and the encrypted advertisement are protocol
//! facts documented by LibrePods (GPL-3.0-or-later); the Pro 1 specifics
//! were measured on real AirPods Pro (1st generation) on 19 November 2026.

pub mod aes;
pub mod ble;
pub mod control;
pub mod frame;
pub mod info;
pub mod message;
pub mod model;
pub mod packet;

pub use ble::{Advert, ProximityKeys};
pub use frame::Frame;
pub use info::DeviceInfo;
pub use message::{BatteryComponent, Message, decode};
pub use model::{
    Capability, ModelSpec, NoiseMode, Part, Side, Wearing, model_for_number, model_for_product,
    product_from_modalias,
};

/// L2CAP PSM of the AAP channel.
pub const AAP_PSM: u16 = 0x1001;

/// Service UUID that BlueZ lists in `Device1.UUIDs` for devices that speak AAP.
pub const AAP_UUID: &str = "74ec2172-0bad-4d01-8f77-997b2be0722a";
