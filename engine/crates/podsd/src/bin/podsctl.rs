// SPDX-License-Identifier: GPL-3.0-or-later
//! `podsctl`: a small debug client of the engine's D-Bus API. It never
//! touches Bluetooth; everything goes through `io.github.d101c.Pods1`.

use std::collections::HashMap;
use std::process::ExitCode;

use zbus::Connection;
use zbus::wire::OwnedObjectPath;

const BUS_NAME: &str = "io.github.d101c.Pods";
const MANAGER: &str = "/io/github/d101c/Pods";
const MANAGER_IFACE: &str = "io.github.d101c.Pods1.Manager";
const DEVICE_IFACE: &str = "io.github.d101c.Pods1.Device";

const USAGE: &str = "usage: podsctl <command>

  status                     engine and AirPods state
  set-noise <mode>           off | transparency | anc | adaptive
  cycle                      next press-and-hold mode (a click on the tile)
  press-hold <mode> <mode>…  modes the stem press-and-hold cycles through
  rename <name>              rename the AirPods
  spatial <mode>             off | fixed | head
  recenter                   the head's direction becomes straight ahead
  follow-screen <on|off>     let the scene follow a lasting turn of the head

Commands act on the connected AirPods, else on the first known pair.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("podsctl: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(&args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("podsctl: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn proxy<'a>(conn: &Connection, path: &str, iface: &'a str) -> zbus::Result<zbus::Proxy<'a>> {
    zbus::proxy::Builder::new(conn)
        .destination(BUS_NAME)
        .path(path.to_string())
        .interface(iface)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
}

/// The active device, else the first one.
async fn target(conn: &Connection) -> zbus::Result<String> {
    let manager = proxy(conn, MANAGER, MANAGER_IFACE).await?;
    let active: OwnedObjectPath = manager.get_property("ActiveDevice").await?;
    if active.as_str() != "/" {
        return Ok(active.to_string());
    }
    let devices: Vec<OwnedObjectPath> = manager.get_property("Devices").await?;
    devices
        .first()
        .map(|p| p.to_string())
        .ok_or_else(|| zbus::Error::Failure("no AirPods known to the engine".into()))
}

fn error_text(e: zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, Some(msg), _) => format!("{name}: {msg}"),
        zbus::Error::MethodError(name, None, _) => name.to_string(),
        other => other.to_string(),
    }
}

async fn run(args: &[String]) -> Result<(), String> {
    let conn = Connection::session().await.map_err(error_text)?;
    match args[0].as_str() {
        "status" => status(&conn).await.map_err(error_text),
        "set-noise" => {
            let mode = args.get(1).ok_or("set-noise needs a mode")?;
            call(&conn, "SetNoiseMode", &(mode.as_str(),)).await
        }
        "cycle" => call(&conn, "CycleNoiseMode", &()).await,
        "press-hold" => {
            let modes: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            call(&conn, "SetPressHoldModes", &(modes,)).await
        }
        "rename" => {
            let name = args[1..].join(" ");
            call(&conn, "Rename", &(name.as_str(),)).await
        }
        "spatial" => {
            let mode = args.get(1).ok_or("spatial needs a mode")?;
            call(&conn, "SetSpatialMode", &(mode.as_str(),)).await
        }
        "recenter" => call(&conn, "Recenter", &()).await,
        "follow-screen" => {
            let on = match args.get(1).map(String::as_str) {
                Some("on") => true,
                Some("off") => false,
                _ => return Err("follow-screen needs on or off".into()),
            };
            call(&conn, "SetFollowScreen", &(on,)).await
        }
        other => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    }
}

async fn call<B>(conn: &Connection, method: &str, body: &B) -> Result<(), String>
where
    B: serde::Serialize + zbus::wire::DynamicType,
{
    let path = target(conn).await.map_err(error_text)?;
    let dev = proxy(conn, &path, DEVICE_IFACE).await.map_err(error_text)?;
    dev.call_method(method, body)
        .await
        .map(|_| ())
        .map_err(error_text)
}

async fn status(conn: &Connection) -> zbus::Result<()> {
    let manager = proxy(conn, MANAGER, MANAGER_IFACE).await?;
    let version: String = manager.get_property("EngineVersion").await?;
    let conflict: String = manager.get_property("Conflict").await?;
    let devices: Vec<OwnedObjectPath> = manager.get_property("Devices").await?;
    println!("engine {version}");
    if !conflict.is_empty() {
        println!("conflict: {conflict} is also talking to the AirPods");
    }
    if devices.is_empty() {
        println!("no AirPods");
    }
    for path in devices {
        let d = proxy(conn, path.as_str(), DEVICE_IFACE).await?;
        let name: String = d.get_property("Name").await?;
        let model: String = d.get_property("ModelName").await?;
        let address: String = d.get_property("Address").await?;
        let connected: bool = d.get_property("Connected").await?;
        println!("\n{name} ({model}, {address})");
        if !connected {
            println!("  not connected");
            continue;
        }
        let firmware: String = d.get_property("Firmware").await?;
        let battery: HashMap<String, (u8, bool)> = d.get_property("Battery").await?;
        let (left, right): (String, String) = d.get_property("Wearing").await?;
        let mode: String = d.get_property("NoiseMode").await?;
        let modes: Vec<String> = d.get_property("NoiseModes").await?;
        let cycle: Vec<String> = d.get_property("PressHoldModes").await?;
        let mut parts: Vec<String> = ["left", "right", "case"]
            .iter()
            .filter_map(|p| {
                battery
                    .get(*p)
                    .map(|(pct, ch)| format!("{p} {pct}%{}", if *ch { " charging" } else { "" }))
            })
            .collect();
        if parts.is_empty() {
            parts.push("unknown".into());
        }
        println!("  firmware   {firmware}");
        println!("  battery    {}", parts.join(", "));
        println!("  wearing    left {left}, right {right}");
        println!("  noise      {mode} (available: {})", modes.join(", "));
        println!("  stem cycle {}", cycle.join(" → "));
        let spatial: String = d.get_property("SpatialMode").await?;
        let spatial_status: String = d.get_property("SpatialStatus").await?;
        let head: String = d.get_property("HeadTracking").await?;
        let follow: bool = d.get_property("FollowScreen").await?;
        println!("  spatial    {spatial} ({spatial_status})");
        println!(
            "  head       {head}{}",
            if follow { ", follow screen" } else { "" }
        );
    }
    Ok(())
}
