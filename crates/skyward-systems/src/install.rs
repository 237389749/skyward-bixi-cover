//! Installing the other system from its package, switching between the
//! systems on the phone, and taking the other system away again.

use crate::state::{State, Store};
use crate::{Named, Other, Overview};
use skyward_block::area::HEADER;

/// Where the areas' files are, in the data partition.
pub const FOLDER: &str = "/data/skyward/systems";
/// The devices the store and a root are laid out as while they are written.
pub const STORE_DEVICE: &str = "skyward-store";
pub const ROOT_DEVICE: &str = "skyward-root";
/// The gigabyte the pages count sizes in.
const GB: u64 = 1_000_000_000;
/// The sizes a root is offered in, in gigabytes, past what it least takes.
const OFFERED: [u64; 6] = [8, 16, 32, 64, 128, 256];

/// How far an install has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Setting room aside in the data partition: the bytes so far, of all.
    Allocating { done: u64, total: u64 },
    /// Writing the root image, then reading it back.
    Writing { done: u64, total: u64 },
    /// Putting the boot set in the store.
    Finishing,
}

/// The room the data partition has for a system's root, of `free` bytes
/// free: less the store where none is made yet, and less a super's worth,
/// which the installed system's updates keep for themselves, as Android
/// keeps it from its dynamic systems.
pub fn room(free: u64, store: u64, super_size: u64) -> u64 {
    free.saturating_sub(store + super_size + HEADER)
}

/// The sizes offered for a root that takes at least `least` bytes, with
/// `room` bytes to spare: what it least takes in whole gigabytes, and the
/// usual sizes past that, as far as there is room. The area made for one
/// is rounded up to its filesystem's blocks.
pub fn sizes(least: u64, room: u64) -> Vec<u64> {
    let first = least.div_ceil(GB).max(1);
    std::iter::once(first).chain(OFFERED.into_iter().filter(|&s| s > first)).map(|s| s * GB).filter(|&s| s <= room).collect()
}

/// What the pages show of the systems, from the state and the installed
/// system's name; `changed` says whether its vendor_boot or dtbo differ
/// from when the other system was installed.
pub fn overview(state: &State, installed: Named, changed: bool) -> Overview {
    Overview {
        installed,
        other: state.system.as_ref().map(|s| Other { id: s.id.clone(), name: s.name.clone(), version: s.version.clone(), size: s.root.size - HEADER }),
        other_starts: state.occupied.is_some(),
        changed,
    }
}

impl Store {
    /// A store's bytes for the phone's partitions, where the store is not
    /// made yet; none where it is.
    pub fn needed(state: &State, boot: u64, init_boot: u64) -> u64 {
        match state.store {
            Some(_) => 0,
            None => Store::size_for(boot, init_boot),
        }
    }
}

/// Have `control` start the slot that was written over: the installed
/// system's, which is the other one from `occupied`, where the other
/// system's boot set was. Says which slot it started.
///
/// This is the whole of what a switch back over an updated slot does to the
/// phone besides the state — and the one part of it a test cannot ask a real
/// phone for, so the control is whatever is handed in (see
/// [`skyward_vab::Starting`]).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn start_at(control: &dyn skyward_vab::Starting, occupied: skyward_vab::Slot) -> anyhow::Result<skyward_vab::Slot> {
    let installed = crate::guard::start_from(occupied);
    control.set_active(installed)?;
    Ok(installed)
}

#[cfg(target_os = "android")]
pub use sys::*;

#[cfg(target_os = "android")]
mod sys {
    use super::*;
    use crate::boot::{self, Start};
    use crate::guard;
    use crate::package::{Contents, BOOT, INIT_BOOT, ROOT};
    use anyhow::Result;
    use crate::state::{System, Who, PATH};
    use crate::store::{self, OTHER};
    use crate::switch::{self, Places};
    use anyhow::{ensure, Context};
    use sha2::{Digest, Sha256};
    use skyward_block::area::{self, Area};
    use skyward_block::{dm, BY_NAME};
    use skyward_vab::Slot;
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};
    use zip::ZipArchive;

    const CHUNK: usize = 1 << 20;

    fn by_name() -> &'static Path {
        Path::new(BY_NAME)
    }

    /// Where the data partition is mounted.
    const DATA: &std::ffi::CStr = c"/data";

    fn data() -> &'static Path {
        Path::new(DATA.to_str().expect("the data partition's mount point is written in ASCII"))
    }

    /// The bytes free in the data partition's filesystem.
    pub fn free() -> Result<u64> {
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(DATA.as_ptr(), &mut s) } != 0 {
            return Err(std::io::Error::last_os_error()).context("look at the room in the data partition");
        }
        Ok(s.f_bavail as u64 * s.f_frsize as u64)
    }

    /// The room for a root, in the data partition as it is now.
    pub fn room_now(slot: Slot) -> Result<u64> {
        let state = state()?;
        let size = |name: &str| skyward_block::size_bytes(store::partition(by_name(), name, slot));
        let needed = Store::needed(&state, size(store::BOOT)?, size(store::INIT_BOOT)?);
        Ok(room(free()?, needed, skyward_block::size_bytes(by_name().join("super"))?))
    }

    /// Lay the store out as a device, once it is found where it was, run
    /// `work` on it, and take the device away again.
    fn with_store<T>(state: &State, work: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
        let store = state.store.as_ref().context("no store holds the boot sets")?;
        store.area.check_header()?;
        if dm::status(STORE_DEVICE)?.is_some() {
            dm::remove(STORE_DEVICE)?;
        }
        let node = store.area.map(STORE_DEVICE)?;
        let done = work(&node);
        let gone = dm::remove(STORE_DEVICE);
        let done = done?;
        gone?;
        Ok(done)
    }

    const METADATA: &str = "/metadata";

    /// Mount /metadata, where the state is kept, read-only where it is not
    /// mounted yet, as the recovery keeps it.
    pub fn metadata() -> Result<()> {
        let at = Path::new(METADATA);
        if !skyward_block::mount::is_mount_point(at)? {
            skyward_block::mount::mount_ro(&by_name().join("metadata"), at)?;
        }
        Ok(())
    }

    /// Run `work` with /metadata writable, and leave it read-only after.
    fn writable<T>(work: impl FnOnce() -> Result<T>) -> Result<T> {
        metadata()?;
        let at = Path::new(METADATA);
        skyward_block::mount::remount_writable(at, true)?;
        let done = work();
        let back = skyward_block::mount::remount_writable(at, false);
        let done = done?;
        back?;
        Ok(done)
    }

    /// The state as kept now.
    pub fn state() -> Result<State> {
        metadata()?;
        State::load(Path::new(PATH))
    }

    /// The program the other system starts with: this recovery's own.
    fn program() -> PathBuf {
        Path::new("/").join(skyward_config::ramdisk::SYSTEM_INIT)
    }

    /// Have the bootloader start `to`, writing its boot set into the boot
    /// partitions; see [`switch::switch`]. Switching to the other system
    /// waits for any update to be merged, and first checks that the other
    /// system's root is still where it was put.
    pub fn switch_to(next: Slot, to: Who) -> Result<()> {
        let state = state()?;
        if to == Who::Other {
            let update = skyward_vab::snapshot_state()?;
            ensure!(update == skyward_vab::SnapshotState::None, "an update is in flight ({update:?}), and the installed system has to start once more to merge it");
            state.system.as_ref().context("no other system is installed")?.root.check_header()?;
        }
        if to == Who::Installed {
            let slot = match state.occupied.as_deref() {
                Some(suffix) => Slot::from_suffix(suffix).with_context(|| format!("the other system is in slot {suffix:?}"))?,
                None => next,
            };
            if let Err(why) = guard::written_over(by_name(), &state, slot) {
                // The three things the guard compares are all about the
                // installed system's slot, `slot.other()`: its boot set
                // against what was saved, its vendor_boot and dtbo against
                // what was installed against. So the slot that was written
                // over, and the one to start, is the one the other system is
                // not in; `start_as_it_is` picks it.
                return start_as_it_is(slot, &why);
            }
        }
        let (path, program) = (Path::new(PATH), program());
        writable(|| with_store(&state, |device| switch::switch(&Places { state: path, by_name: by_name(), store: device, program: &program }, next, to)))
    }

    /// Put the installed system's boot set back for the sake of erasing the
    /// data partition or wiping the other system away, where it is still the
    /// boot set the partitions hold.
    ///
    /// Where it is not — an update wrote the installed system's slot while
    /// the other system ran — the saved copy is let go and the bootloader is
    /// pointed at the slot as it stands: what is there is the updated
    /// system's own boot image beside its own system and vendor, which
    /// starts itself. Writing the copy back would start the phone with a
    /// boot image from before the update over a system from after it. The
    /// erase goes on either way, because the data partition being erased is
    /// not something the other system has to start for.
    fn switch_back_for_erase(state: &State, slot: Slot) -> Result<()> {
        if let Err(why) = guard::written_over(by_name(), state, slot) {
            return start_as_it_is(slot, &why);
        }
        let (path, program) = (Path::new(PATH), program());
        writable(|| with_store(state, |device| switch::switch(&Places { state: path, by_name: by_name(), store: device, program: &program }, slot, Who::Installed)))
    }

    /// Have the bootloader start `slot` as it stands, writing no boot set:
    /// what is in its partitions is the system that was updated there, its
    /// own boot image beside its own system and vendor, and that set starts
    /// itself and merges what an update left to merge as it does.
    ///
    /// `slot` is the slot the other system's boot set is in — what
    /// [`guard::written_over`] said was written over — and the slot that
    /// starts is the other one, where the installed system is: starting
    /// `slot` itself would start the other system after its boot set was
    /// let go of, which the state no longer says is there, and in
    /// [`Self::remove`] with the root it is about to delete.
    ///
    /// This is what a switch back does when the saved copy is not the boot
    /// set the partitions hold any more. Writing the copy back — which is
    /// what a switch back does when nothing has been written over — would
    /// start the phone with a boot image from before an update over the
    /// system and vendor from after it, and it would not come up, or it
    /// would come up part way and crash. What was saved is let go instead,
    /// with the other system's record, because nothing holds its boot set
    /// any more; its root stays named in the state as forgotten, so the room
    /// it takes can be taken back and a reinstall of the same package is not
    /// blocked by it. kmsg says so, so a phone that then starts from an
    /// updated slot has an account of why it started as it was, why the
    /// other system has to be installed again, and what is still taking
    /// room.
    fn start_as_it_is(slot: Slot, why: &anyhow::Error) -> Result<()> {
        let (binder, hardware) = (skyward_binder::driver::Binder::open()?, skyward_binder::driver::Binder::hardware()?);
        let control = skyward_vab::boot::BootControl::open(&binder, &hardware)?;
        let installed = start_at(&control, slot)?;
        let left = writable(|| guard::let_go_at(Path::new(PATH), &format!("{why:#}")))?;
        let _ = skyward_sys::kmsg(&format!(
            "the installed system's partitions were written over ({why:#}); slot {} starts as it is, its boot set let go of rather than written back, and the other system's record goes with it{} - it is to be installed again",
            installed.letter(),
            match &left {
                Some(id) => format!(", its root left in the data partition as {id} until it is installed again or taken away"),
                None => String::new(),
            }
        ));
        Ok(())
    }

    /// Finish a switch cut off before, where one was; says whether there
    /// was one.
    pub fn resume() -> Result<bool> {
        let state = state()?;
        if state.switching.is_none() {
            return Ok(false);
        }
        let (path, program) = (Path::new(PATH), program());
        writable(|| with_store(&state, |device| switch::resume(&Places { state: path, by_name: by_name(), store: device, program: &program })))
    }

    /// Put the installed system's boot set back where the other system's
    /// is, as everything that writes or erases the installed system first
    /// does; says whether it had to.
    ///
    /// Where the saved copy is not what the partitions hold any more — an
    /// update wrote the installed system's slot while the other system ran
    /// — the copy is let go and the bootloader is pointed at the slot as it
    /// stands instead: a boot image from before the update over a system
    /// from after it does not start, or starts part way and crashes, while
    /// the updated slot starts itself. Either way the phone has somewhere
    /// to start from, which refusing to switch would not give it.
    pub fn ensure_installed() -> Result<bool> {
        resume()?;
        let state = state()?;
        let Some(slot) = state.occupied.as_deref().map(|s| Slot::from_suffix(s).with_context(|| format!("the other system is in slot {s:?}"))).transpose()? else {
            return Ok(false);
        };
        switch_to(slot, Who::Installed)?;
        Ok(true)
    }

    /// Whether anything has been written over what the other system was
    /// installed against; see [`guard::changed`].
    pub fn changed(state: &State, current: Slot) -> Result<bool> {
        guard::changed(by_name(), state, current)
    }

    /// Install the system in the package `zip`, whose contents are
    /// `contents`, with a root of `size` bytes: its root written into a
    /// new area of the data partition, which has to be open, and its boot
    /// set, with `program` to start it, put in the store, made where there
    /// is none yet. `slot` is the slot the phone runs from, whose
    /// init_boot image lends its header.
    pub fn install<R: Read + Seek>(zip: &mut ZipArchive<R>, contents: &Contents, size: u64, program: &[u8], slot: Slot, progress: &mut dyn FnMut(Step)) -> Result<()> {
        writable(|| install_now(zip, contents, size, program, slot, progress))
    }

    fn install_now<R: Read + Seek>(zip: &mut ZipArchive<R>, contents: &Contents, size: u64, program: &[u8], slot: Slot, progress: &mut dyn FnMut(Step)) -> Result<()> {
        let path = Path::new(PATH);
        let mut state = State::load(path)?;
        ensure!(state.switching.is_none(), "a switch begun before is not done yet");
        ensure!(state.system.is_none(), "another system is installed already");
        ensure!(size >= contents.least(), "a root of {size} bytes is less than the {} the system takes", contents.least());
        let folder = Path::new(FOLDER);
        let id = contents.manifest.id.clone();
        let file = folder.join(format!("{id}.img"));
        // A system that was written over leaves its root behind, named in the
        // state as forgotten; installing the same package again looks for that
        // very file, and `area::create` will not make one where one is. It is
        // taken away before the room is counted, because it is the room this
        // install is about to take: the bytes it holds are still in the
        // partition, so counting first would answer "there is room for less
        // than this" to a phone where taking those bytes back is all that is
        // needed — and would not even let it try.
        if let Some(at) = state.forgotten.iter().position(|forgotten| forgotten.id == id) {
            let forgotten = state.forgotten.remove(at);
            forgotten.root.remove().context("take away the room the system it replaces was left in")?;
            state.save(path)?;
        }
        let room = room_now(slot)?;
        ensure!(size <= room, "the data partition has room for {room} bytes, less than {size}");
        fs::create_dir_all(folder).with_context(|| format!("make {}", folder.display()))?;
        if state.store.is_none() {
            let (boot, init_boot) = (skyward_block::size_bytes(store::partition(by_name(), store::BOOT, slot))?, skyward_block::size_bytes(store::partition(by_name(), store::INIT_BOOT, slot))?);
            let area = area::create(&folder.join("store"), Store::size_for(boot, init_boot), &mut |done, total| progress(Step::Allocating { done, total }))?;
            state.store = Some(Store { area, boot, init_boot });
            state.save(path)?;
        }
        // A file left at the path by anything else is taken away too, rather
        // than failing an install of several gigabytes over it.
        if file.exists() {
            fs::remove_file(&file).with_context(|| format!("take away what is left at {}", file.display()))?;
        }
        let root = area::create(&file, size + HEADER, &mut |done, total| progress(Step::Allocating { done, total }))?;
        let made = (|| -> Result<System> {
            write_root(zip, &root, contents.root, progress)?;
            progress(Step::Finishing);
            let start = Start { id: id.clone(), filesystem: contents.manifest.filesystem.clone(), init: contents.manifest.init.clone(), root: root.clone() };
            let (init_boot, own_ramdisk) = match contents.init_boot {
                true => {
                    let mut own = Vec::new();
                    zip.by_name(INIT_BOOT).context("open init_boot.img")?.read_to_end(&mut own).context("read init_boot.img")?;
                    let (image, own_ramdisk) = boot::with_start(&own, program, &start)?;
                    (image, Some(own_ramdisk))
                }
                false => (boot::init_boot(&fs::read(store::partition(by_name(), store::INIT_BOOT, slot)).context("read the phone's init_boot")?, program, &start)?, None),
            };
            let set = with_store(&state, |device| {
                let mut image = zip.by_name(BOOT).context("open boot.img")?;
                let store = state.store.as_ref().expect("made above");
                store::put(store, device, OTHER, &mut image, contents.boot, &init_boot)
            })?;
            Ok(System {
                id: id.clone(),
                name: contents.manifest.name.clone(),
                version: contents.manifest.version.clone(),
                filesystem: contents.manifest.filesystem.clone(),
                init: contents.manifest.init.clone(),
                root: root.clone(),
                set,
                own_ramdisk,
                vendor_boot: guard::sum_of(&store::partition(by_name(), "vendor_boot", slot))?,
                dtbo: guard::sum_of(&store::partition(by_name(), "dtbo", slot))?,
            })
        })();
        match made {
            Ok(system) => {
                state.system = Some(system);
                state.save(path)
            }
            Err(e) => match root.remove() {
                Ok(()) => Err(e),
                Err(r) => Err(e.context(format!("taking the root away afterwards failed too: {r:#}"))),
            },
        }
    }

    /// Write the root image from the package into the root area, after its
    /// header, then read it back.
    fn write_root<R: Read + Seek>(zip: &mut ZipArchive<R>, root: &Area, len: u64, progress: &mut dyn FnMut(Step)) -> Result<()> {
        ensure!(len <= root.size - HEADER, "the root image takes {len} bytes, more than its area's {}", root.size - HEADER);
        if dm::status(ROOT_DEVICE)?.is_some() {
            dm::remove(ROOT_DEVICE)?;
        }
        let node = root.map(ROOT_DEVICE)?;
        let written = (|| -> Result<()> {
            let mut out = OpenOptions::new().read(true).write(true).open(&node).with_context(|| format!("open {}", node.display()))?;
            let mut image = zip.by_name(ROOT).context("open rootfs.img")?;
            out.seek(SeekFrom::Start(HEADER))?;
            let mut buf = vec![0u8; CHUNK];
            let (mut sum, mut done) = (Sha256::new(), 0);
            while done < len {
                let n = (len - done).min(CHUNK as u64) as usize;
                image.read_exact(&mut buf[..n]).context("read rootfs.img")?;
                out.write_all(&buf[..n]).context("write the root")?;
                sum.update(&buf[..n]);
                done += n as u64;
                progress(Step::Writing { done, total: len * 2 });
            }
            out.sync_all().context("sync the root")?;
            skyward_block::drop_cache(&out)?;
            out.seek(SeekFrom::Start(HEADER))?;
            let (mut back, mut read) = (Sha256::new(), 0);
            while read < len {
                let n = (len - read).min(CHUNK as u64) as usize;
                out.read_exact(&mut buf[..n]).context("read the root back")?;
                back.update(&buf[..n]);
                read += n as u64;
                progress(Step::Writing { done: len + read, total: len * 2 });
            }
            ensure!(back.finalize() == sum.finalize(), "the root reads back different from what was written");
            Ok(())
        })();
        let gone = dm::remove(ROOT_DEVICE);
        written?;
        gone
    }

    /// Take the other system away: its boot set out of the partitions
    /// where it is there, its root and the store out of the data partition,
    /// which has to be open, and the state with them.
    ///
    /// The boot set goes back whether or not the installed system's
    /// partitions are still what they were: the other system is being taken
    /// away, so nothing is left to protect, and refusing here would leave a
    /// phone holding a boot set for a system that is about to be deleted.
    /// What was written over is said in kmsg, so a phone that starts part
    /// way afterwards has an account of why.
    pub fn remove() -> Result<()> {
        ensure!(skyward_block::mount::is_mount_point(data())?, "the data partition is not open");
        resume()?;
        let state = state()?;
        // Where the switch back finds the partitions written over, it lets go
        // of the other system's record along with the saved copy; the root it
        // leaves behind is named in the state as forgotten then, and this is
        // the call that was asked to take everything away, so the root is kept
        // here rather than read out of the state afterwards.
        let root = state.system.as_ref().map(|system| system.root.clone());
        let forgotten: Vec<_> = state.forgotten.iter().map(|forgotten| forgotten.root.clone()).collect();
        if let Some(suffix) = state.occupied.as_deref() {
            let slot = Slot::from_suffix(suffix).with_context(|| format!("the other system is in slot {suffix:?}"))?;
            switch_back_for_erase(&state, slot)?;
        }
        writable(|| {
            let path = Path::new(PATH);
            let mut state = State::load(path)?;
            if let Some(root) = &root {
                root.remove()?;
            }
            for forgotten in &forgotten {
                forgotten.remove()?;
            }
            if state.system.is_some() || state.occupied.is_some() || state.saved.is_some() || !state.forgotten.is_empty() {
                state.system = None;
                state.occupied = None;
                state.saved = None;
                state.forgotten.clear();
                state.save(path)?;
            }
            if let Some(store) = &state.store {
                store.area.remove()?;
                state = State::default();
                state.save(path)?;
            }
            let folder = PathBuf::from(FOLDER);
            if folder.exists() && fs::read_dir(&folder)?.next().is_none() {
                fs::remove_dir(&folder).with_context(|| format!("delete {}", folder.display()))?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_room_for_a_root_keeps_a_supers_worth_for_updates() {
        assert_eq!(room(59 * GB, 600 << 20, 9 * GB), 59 * GB - (600 << 20) - 9 * GB - HEADER);
        assert_eq!(room(GB, 0, 9 * GB), 0, "none to spare");
    }

    #[test]
    fn the_sizes_offered_start_at_what_the_root_takes_and_stay_within_room() {
        assert_eq!(sizes(3 << 30, 50 * GB), [4 * GB, 8 * GB, 16 * GB, 32 * GB], "3 GiB is past 3 GB");
        assert_eq!(sizes(100 << 20, 9 * GB), [GB, 8 * GB]);
        assert_eq!(sizes(20 * GB, 300 * GB), [20 * GB, 32 * GB, 64 * GB, 128 * GB, 256 * GB]);
        assert!(sizes(20 * GB, 10 * GB).is_empty(), "no room for it at all");
    }

    /// A boot control that remembers what it was told to start.
    struct Recorder(std::sync::Mutex<Vec<skyward_vab::Slot>>);

    impl skyward_vab::Starting for Recorder {
        fn active(&self) -> anyhow::Result<skyward_vab::Slot> {
            Ok(skyward_vab::Slot::A)
        }

        fn slots(&self) -> anyhow::Result<skyward_vab::BootSlots> {
            Ok(skyward_vab::BootSlots { active: skyward_vab::Slot::A, bootable: [true, true], successful: [true, true] })
        }

        fn set_active(&self, slot: skyward_vab::Slot) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(slot);
            Ok(())
        }
    }

    #[test]
    fn a_slot_started_over_an_updated_one_is_the_installed_systems_not_the_other_ones() {
        let control = Recorder(Default::default());
        assert_eq!(start_at(&control, skyward_vab::Slot::B).unwrap(), skyward_vab::Slot::A, "the other system is in B, so A starts");
        assert_eq!(*control.0.lock().unwrap(), [skyward_vab::Slot::A], "and the boot control is told so, not B");
    }

    #[test]
    fn once_the_other_systems_slot_is_let_go_of_there_is_nothing_left_to_switch_to() {
        let (_dir, _at, state) = crate::guard::tests::phone(b"old boot", b"other system boot");
        let mut gone = state;
        crate::guard::let_go(&mut gone, "an update wrote over it");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("systems.toml");
        gone.save(&path).unwrap();
        let places = crate::switch::Places { state: &path, by_name: dir.path(), store: dir.path(), program: dir.path() };
        let why = crate::switch::switch(&places, skyward_vab::Slot::A, crate::state::Who::Other).unwrap_err();
        assert!(format!("{why:#}").contains("no other system is installed"), "the trap that ends in the other system starting every time is shut: {why:#}");
    }
}
