//! Names and constants shared by both modes. Nothing is written to the
//! user's configuration: the spatial nodes live as long as the engine's
//! connection to PipeWire.

use crate::Error;

/// `node.name` of the virtual sink, in both modes.
pub const SINK_NODE_NAME: &str = "spatial_airpods";
/// `node.name` of the renderer filter feeding the AirPods.
pub const OUTPUT_NODE_NAME: &str = "spatial_airpods.output";
/// Radius of the HRIR lookups (KEMAR is measured at 1.4 m).
pub const RADIUS_M: f32 = 1.4;

/// The default KEMAR profile shipped by libmysofa.
pub const DEFAULT_SOFA: &str = "/usr/share/libmysofa/MIT_KEMAR_normal_pinna.sofa";

/// Node names are written unquoted-safe into SPA JSON and matched by
/// WirePlumber; accept only what PipeWire itself produces for BlueZ nodes
/// (`bluez_output.XX_XX_XX_XX_XX_XX.1`) and simple test names.
pub fn validate_node_name(name: &str) -> Result<(), Error> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'));
    if ok { Ok(()) } else { Err(Error::InvalidTarget(name.to_owned())) }
}

/// Quotes a string for SPA JSON (the `default` metadata values).
pub fn spa_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' | '\t' => out.push(' '),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Description shown in GNOME's sound settings: "AirPods Pro · Spatial".
pub fn sink_description(device_description: &str) -> String {
    let d = device_description.trim();
    if d.is_empty() { "Spatial Audio".to_owned() } else { format!("{d} · Spatial") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_odd_targets() {
        assert!(validate_node_name("bluez_output.AA_BB_CC_DD_EE_FF.1").is_ok());
        assert!(validate_node_name("fake_airpods").is_ok());
        assert!(validate_node_name("").is_err());
        assert!(validate_node_name("a\" } evil = {").is_err());
        assert!(validate_node_name("a b").is_err());
    }

    #[test]
    fn escapes_and_descriptions() {
        assert_eq!(spa_json_string(r#"Jo's "Pods" \ x"#), r#""Jo's \"Pods\" \\ x""#);
        assert_eq!(sink_description("AirPods Pro"), "AirPods Pro · Spatial");
        assert_eq!(sink_description("  "), "Spatial Audio");
    }
}
