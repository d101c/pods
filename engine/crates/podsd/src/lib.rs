// SPDX-License-Identifier: GPL-3.0-or-later
//! `podsd`, the pods engine, as a library so that its tests can drive it
//! on a private bus with a fake BlueZ and fake AirPods.

pub mod alerts;
pub mod api;
pub mod case;
mod casecard;
pub mod config;
pub mod conflict;
pub mod engine;
pub mod keys;
pub mod link;
pub mod media;
pub mod spatial;
pub mod state;

pub use engine::{Engine, EngineDeps};
pub use link::Timing;
pub use spatial::{AudioMonitor, PipeWireRenderer, PipeWireSinks, Renderer, Rendering, SinkState};
