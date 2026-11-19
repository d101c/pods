// libmysofa reads the SOFA HRTF files (KEMAR) for the head-tracked renderer.
// Linked through pkg-config; the headers (libmysofa-dev) are only needed for
// the .pc file, the few prototypes are declared by hand in src/hrir.rs.
fn main() {
    if let Err(e) = pkg_config::Config::new().atleast_version("1.2").probe("libmysofa") {
        panic!("libmysofa not found through pkg-config (install libmysofa-dev): {e}");
    }
}
