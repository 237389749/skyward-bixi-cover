//! Whether the boot partitions still hold the boot set the state names.
//!
//! The state is worth only what the partitions it describes are worth: an
//! update of the installed system, run while the other system is up, writes
//! the slot that does not start next — the installed system's own boot,
//! vendor_boot and dtbo — so the copy of that boot set in the store is the
//! boot set of a version that is no longer there. A switch back that wrote
//! it would start the phone with the old boot image and the new system and
//! vendor partitions: it does not come up, or it comes up part way and
//! crashes.
//!
//! This is kept apart from the parts of the store that only a phone can
//! reach, so that what decides it can be tried without a phone: the
//! partitions are read through `by_name`, which is a directory of files
//! standing in for them in the tests.

use crate::state::{Saved, State};
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use skyward_vab::Slot;
use std::fs::File;
use std::io::Read;
use std::path::Path;

const CHUNK: usize = 1 << 20;

/// The SHA-256 of all of a partition.
pub(crate) fn sum_of(path: &Path) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    skyward_block::drop_cache(&f)?;
    let mut sum = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf).with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        sum.update(&buf[..n]);
    }
    Ok(sum.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether the boot partitions of `slot` still hold the boot set `saved`
/// names, as many images of it as the phone keeps.
pub(crate) fn holds(by_name: &Path, slot: Slot, saved: &Saved) -> Result<bool> {
    if sum_of(&crate::store::partition(by_name, crate::store::BOOT, slot))? != saved.boot {
        return Ok(false);
    }
    if saved.init_boot.is_empty() {
        return Ok(true);
    }
    Ok(sum_of(&crate::store::partition(by_name, crate::store::INIT_BOOT, slot))? == saved.init_boot)
}

/// Whether anything has been written over what the other system was
/// installed against: the installed system's boot set and the vendor_boot
/// and dtbo it starts with, in the slot the other system is in the other
/// one of; or the other system's own boot set, in the slot the state says
/// it is in.
///
/// An update of the installed system, run from the other system — which is
/// where the stock updater writes, the slot that does not start next —
/// changes exactly those partitions, and the state has to say so rather
/// than read as unchanged.
pub(crate) fn changed(by_name: &Path, state: &State, current: Slot) -> Result<bool> {
    let Some(system) = &state.system else { return Ok(false) };
    let occupied = match &state.occupied {
        Some(suffix) => Some(Slot::from_suffix(suffix).with_context(|| format!("the slot {suffix:?} the other system is in"))?),
        None => None,
    };
    // The installed system's slot: the other system runs from the one the
    // state says its boot set is in, so the installed system is in the
    // other one. With no switch made, it is the one the phone runs.
    let installed = match occupied {
        Some(occupied) => occupied.other(),
        None => current,
    };
    let mut changed = false;
    if let Some(saved) = &state.saved {
        changed |= !holds(by_name, installed, saved)?;
    }
    for (name, want) in [("vendor_boot", &system.vendor_boot), ("dtbo", &system.dtbo)] {
        changed |= sum_of(&crate::store::partition(by_name, name, installed))? != *want;
    }
    if let Some(occupied) = occupied {
        changed |= !holds(by_name, occupied, &system.set)?;
    }
    Ok(changed)
}

/// Refuse a switch back that would put a boot set saved before onto a
/// system that is no longer the one it was saved from; `slot` is the one
/// the other system's boot set is in, so the installed system is in the
/// other one.
///
/// So say so, with what can be done about it, instead of writing and
/// leaving a phone that has to be got out of with fastboot and no
/// explanation.
pub(crate) fn written_over(by_name: &Path, state: &State, slot: Slot) -> Result<()> {
    let (Some(saved), Some(system)) = (&state.saved, &state.system) else { return Ok(()) };
    ensure!(
        holds(by_name, slot, &system.set)?,
        "the other system's boot set is no longer in the partitions of slot {}, so something wrote over it; installing it again is the safe way back",
        slot.suffix()
    );
    let installed = slot.other();
    ensure!(
        holds(by_name, installed, saved)?,
        "the installed system's boot set in slot {} is not the one that was saved, so an update has replaced it; writing the saved one back would start the phone with an old boot image and a new system - delete the other system and install it again",
        installed.suffix()
    );
    for (name, want) in [("vendor_boot", &system.vendor_boot), ("dtbo", &system.dtbo)] {
        ensure!(
            sum_of(&crate::store::partition(by_name, name, installed))? == *want,
            "the installed system's {name} in slot {} was replaced after the other system was installed, so the other system is not what it was installed as - delete it and install it again",
            installed.suffix()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::area;
    use crate::state::{Saved, System};
    use crate::store;

    /// A phone's boot partitions as a directory of files: the installed
    /// system's boot set in slot `_a`, the other system's in slot `_b`, the
    /// installed system's set saved before the other one took the slot, and
    /// the other system installed against the installed one's vendor_boot
    /// and dtbo.
    fn phone(old: &[u8], new: &[u8]) -> (tempfile::TempDir, std::path::PathBuf, State) {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().to_path_buf();
        let saved = Saved { entry: 0, boot: hex(old), init_boot: hex(old) };
        let set = Saved { entry: 1, boot: hex(new), init_boot: hex(new) };
        for (name, bytes) in [(store::BOOT, old), (store::INIT_BOOT, old)] {
            std::fs::write(store::partition(&at, name, Slot::A), bytes).unwrap();
        }
        for (name, bytes) in [(store::BOOT, new), (store::INIT_BOOT, new)] {
            std::fs::write(store::partition(&at, name, Slot::B), bytes).unwrap();
        }
        for name in ["vendor_boot", "dtbo"] {
            std::fs::write(store::partition(&at, name, Slot::A), b"stock").unwrap();
        }
        let state = State {
            saved: Some(saved),
            system: Some(System {
                id: "test".into(),
                name: "Test".into(),
                version: "1".into(),
                filesystem: "ext4".into(),
                init: "/sbin/init".into(),
                root: area("/data/skyward/systems/test", 1 << 20),
                set,
                own_ramdisk: None,
                vendor_boot: hex(b"stock"),
                dtbo: hex(b"stock"),
            }),
            occupied: Some(Slot::B.suffix().to_string()),
            ..State::default()
        };
        (dir, at, state)
    }

    fn hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn a_switch_back_over_an_updated_system_is_refused() {
        let (_dir, at, state) = phone(b"old boot", b"other system boot");
        assert!(written_over(&at, &state, Slot::B).is_ok(), "nothing was written over yet");
        // An update of the installed system, run from the other one, writes
        // the slot the installed system is in: its boot no longer holds what
        // was saved before the other system took the other slot.
        std::fs::write(store::partition(&at, store::BOOT, Slot::A), b"new boot").unwrap();
        let why = written_over(&at, &state, Slot::B).unwrap_err();
        assert!(format!("{why:#}").contains("would start the phone with an old boot image"), "says what the write would do: {why:#}");
        assert!(changed(&at, &state, Slot::A).unwrap(), "a written over boot set is a change the page sees");
        let mut unafraid = state.clone();
        unafraid.saved = None;
        assert!(written_over(&at, &unafraid, Slot::B).is_ok(), "with nothing saved there is nothing that could be put back");
    }

    #[test]
    fn an_updated_vendor_boot_or_dtbo_is_a_change_even_with_the_boot_set_untouched() {
        let (_dir, at, state) = phone(b"old boot", b"other system boot");
        assert!(!changed(&at, &state, Slot::A).unwrap(), "nothing was written over");
        std::fs::write(store::partition(&at, "dtbo", Slot::A), b"updated stock").unwrap();
        let why = written_over(&at, &state, Slot::B).unwrap_err();
        assert!(format!("{why:#}").contains("delete it and install it again"), "says what to do: {why:#}");
        assert!(changed(&at, &state, Slot::A).unwrap(), "an update of dtbo is seen");
    }

    #[test]
    fn the_other_systems_own_boot_set_going_away_is_a_change() {
        let (_dir, at, state) = phone(b"old boot", b"other system boot");
        std::fs::write(store::partition(&at, store::BOOT, Slot::B), b"something else").unwrap();
        let why = written_over(&at, &state, Slot::B).unwrap_err();
        assert!(format!("{why:#}").contains("installing it again is the safe way back"), "says what to do: {why:#}");
        assert!(changed(&at, &state, Slot::A).unwrap(), "the other system's boot set leaving slot B is a change");
    }
}
