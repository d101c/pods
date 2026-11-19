// SPDX-License-Identifier: GPL-3.0-or-later
//! Other AAP clients. The buds only talk to the last host that opened
//! the channel, so a second client steals the notifications. The engine
//! cannot stop one; it tells the user through `Manager.Conflict`.

use std::path::Path;

/// Process names (as in `/proc/<pid>/comm`, 15 characters at most) and
/// the name shown to the user.
const KNOWN: &[(&str, &str)] = &[
    ("podctld", "podctl"),
    ("podctl", "podctl"),
    ("earport-daemon", "EarPort"),
    ("earport", "EarPort"),
    ("librepods", "LibrePods"),
    ("mousepods", "MousePods"),
    ("podtrack", "podtrack"),
];

/// The first known AAP client among `comms`.
pub fn find<I, S>(comms: I) -> Option<&'static str>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    comms.into_iter().find_map(|c| {
        let c = c.as_ref().trim().to_ascii_lowercase();
        KNOWN
            .iter()
            .find(|(comm, _)| c == *comm)
            .map(|(_, shown)| *shown)
    })
}

/// Looks through `proc_root` (normally `/proc`) for another AAP client.
/// Reads files only; starts nothing.
pub fn scan(proc_root: &Path) -> Option<&'static str> {
    let me = std::process::id().to_string();
    let entries = std::fs::read_dir(proc_root).ok()?;
    let comms = entries.filter_map(|e| {
        let e = e.ok()?;
        let name = e.file_name().into_string().ok()?;
        if !name.bytes().all(|b| b.is_ascii_digit()) || name == me {
            return None;
        }
        std::fs::read_to_string(e.path().join("comm")).ok()
    });
    find(comms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_clients_are_named() {
        assert_eq!(find(["bash", "podctld\n"]), Some("podctl"));
        assert_eq!(find(["earport-daemon"]), Some("EarPort"));
        assert_eq!(find(["LibrePods"]), Some("LibrePods"));
        assert_eq!(find(["podsd", "gnome-shell"]), None);
        assert_eq!(find(Vec::<String>::new()), None);
    }

    #[test]
    fn scans_a_proc_tree() {
        let dir = tempfile::tempdir().unwrap();
        for (pid, comm) in [("12", "bash\n"), ("40", "earport-daemon\n"), ("self", "x")] {
            std::fs::create_dir(dir.path().join(pid)).unwrap();
            std::fs::write(dir.path().join(pid).join("comm"), comm).unwrap();
        }
        assert_eq!(scan(dir.path()), Some("EarPort"));
        assert_eq!(scan(&dir.path().join("missing")), None);
    }
}
