# Drawing the pages on the cover panel

> Why this fork exists, what it changed, what has been seen on the phone, and
> what is still missing. Written 2026-11-15 against `1ec121a`.

The phone this is about has its **inner panel physically removed**. That is the
whole reason for everything below: a phone that folds is expected to have both
panels, and every part of the recovery that decides which screen to draw on
assumes the inner one is there.

---

## 1. What went wrong on the plain `bixi` build

Flashing `skyward-…-bixi-….img` on this phone gives a black screen and a working
adb shell, on a five second restart loop.

### 1.1 It draws on the connector the bootloader called primary

`crates/skyward-ui/src/surface.rs`:

```rust
/// The `which`-th built-in panel that is connected, counting up
/// from the lowest connector: 0 is the screen the phone shows the
/// pages on, 1 the cover of a phone that folds.
pub fn open_panel(path: &str, which: usize) -> Result<Self>
```

and `crates/skyward/src/ui.rs` opens it with `KmsSurface::open("/dev/dri/card0")`,
which is `open_panel(path, 0)` — the first connected panel, i.e. DSI-1.

### 1.2 The cover is only opened when the phone reports itself shut

```rust
fn shut_now() -> Option<bool> {
    let nodes = fs::read_dir("/sys/devices/platform").ok()?;
    for node in nodes.flatten() {
        if !node.file_name().to_string_lossy().starts_with("synaptics_tcm") {
            continue;
        }
        let Ok(text) = fs::read_to_string(node.path().join("synaptics_tcm/capfold_status")) else { continue };
        ...
    }
    None            // "Nothing is reported where the driver is not there."
}
```

`capfold_status` belongs to the **inner panel's** Synaptics driver. With that
panel gone the file does not exist, so `shut_now()` returns `None`, the only
call to `open_again(1)` never runs, and the cover is never driven.

### 1.3 The panel it does find is a reference panel with none of the numbers

With the inner panel absent the bootloader falls back, and the kernel is told:

```
msm_drm.dsi_display0=qcom,mdss_dsi_nt37801_wqhd_plus_vid:   1440x3200 vid
msm_drm.dsi_display1=qcom,mdss_dsi_o8_42_02_0a_sec_dsc_cmd:  1208x1392 cmd
```

`nt37801_wqhd_plus_vid` is a Qualcomm reference panel from the dtbo, not a part
this phone has. It carries `qcom,mdss-dsi-bl-max-level` and **none** of the
`mi,*` properties, so `normal_max()` failed outright:

```
skyward: recovery failed: panel qcom,mdss_dsi_nt37801_wqhd_plus_vid gives its
normal maximum as none of [("mi,normal-max-brightness-clone","mi,max-brightness-clone"),
("mi,max-brightness-normal","qcom,mdss-dsi-bl-max-level"),
("mi,mdss-dsi-fac-bl-max-level","qcom,mdss-dsi-bl-max-level")]
```

That took `Backlight::open()` down with it, and init restarted the recovery
every five seconds — black screen, adb alive.

---

## 2. What this fork changes

Three commits, each usable on its own.

### 2.1 `[display] panel` and `[display] backlight` (31c311d)

```toml
[display]
panel = "DSI-2"                    # or the size: "1208x1392"
backlight = "panel1-backlight"
```

* `crates/skyward-config/src/lib.rs` — two new optional fields, with a check
  that the name is either a connector (`DSI-2`) or a size (`1208x1392`). A typo
  is refused rather than quietly falling back to the inner panel.
* `crates/skyward-ui/src/surface.rs` — `KmsSurface::open_named` picks the
  connected panel that matches, and **lists the panels there are** when nothing
  matches, instead of drawing on another one. The naming rules live in
  `is_wanted_panel`, a plain function the host tests can reach.
* `crates/skyward/src/ui.rs` — `Backlight::open_on` derives the panel parameter
  (`dsi_displayN`) from the backlight name (`panelN-backlight`), and the folded
  state is taken out of the choice when a panel is named (`folio`).

### 2.2 `normal_max` tolerates a panel without the numbers (31c311d)

Every step became optional: no parameter, no colon, no node in the device tree,
or none of the `mi,*` properties all mean **the whole of the panel's own range
is normal**. That number only decides how far the brightness slider goes; it is
not worth a black screen.

### 2.3 `[display] orientation` (1ec121a)

```toml
orientation = "upside"     # `normal` by default
```

`crates/skyward/src/input.rs` already had the machinery — `Panel::new(…, flip)`
turns a report with `(w - x, h - y)` — but `flip` was only ever `true` for the
cover opened by the fold switch. A phone whose inner panel is gone never
triggers that switch, so the pages it put on the cover came with mirrored
touches. Which way round a panel is mounted is a fact about the phone, so it now
lives in the configuration next to the panel it describes.

Verified against the hardware: `goodix_ts` reports `properties = 2`
(`INPUT_PROP_DIRECT`) with X `0..1208` and Y `0..1392`, matching the screen
exactly, so `(w - x, h - y)` is the whole of the fix.

### 2.4 The build hands out both images (87a9217)

`.github/workflows/build.yml` builds `bixi` **and** `bixi-cover-first`. A
variant shares its phone's extra folder: the device name up to its first dash is
the phone, so `bixi-cover-first` takes `bixi_extra`. Without that the cover
image silently lost `se_omapi`, both NDK libraries and the weaver description,
and `/data` would not have opened on it.

---

## 3. Seen on the phone

| | |
|---|---|
| Cover lights up, recovery usable | ✅ user, on `1ec121a` |
| `/data` decrypts with the screen lock | ✅ user, on `1ec121a` |
| Package check | ✅ 59 passed, 0 warnings, 0 failed (`bixi_extra` 9 files included) |
| Touch direction | ⏳ fixed in `1ec121a`, **not yet confirmed on the phone** |
| Corner radius 57 | ⏳ a guess by physical size, not measured |

The image the user had been running before that was `87a9217`, which still had
the mirrored touch: `/skyward/device.toml` on the phone had `panel`,
`backlight` and `covers` but no `orientation`. If touch still feels wrong after
flashing, check that line is there before looking at the code.

---

## 4. Still missing

### 4.1 The swipe-to-confirm slider falls off the bottom — **fixed 2026-11-16**

The same bug this fork's older sibling fixed for the cover. These pages built a
**fixed canvas** (`page(cx)`) and then drew the confirm track at the current
`y`:

| page | line | canvas |
|---|---|---|
| `reboot` | `pages.rs:957` | `page(cx)` |
| `relabel` | `pages.rs:2614` | `page(cx)` |
| `slot` | `pages.rs:4725` | `page(cx)` |

On a 1392 px tall screen the target list plus the slot section pushed the track
past the bottom, where it was neither drawn nor reachable.

They now build on `long_page(cx)` and trim the canvas to what they drew, as
every other page does:

```rust
let mut c = long_page(cx);
...
Frame::new(t, fit(c, t, track.y + track.h), out, l.pick(REBOOT), top)
```

`long_page` makes the canvas as tall as the content needs; `fit` trims it to the
content plus the bottom margin, never shorter than one screen. The regression
test is `the_reboot_confirm_is_in_the_page_the_cover_panel_scrolls` in `app.rs`:
it lays the page out at 1208×1392 and holds the confirm track's bottom against
the canvas.

### 4.2 The lock pages are not laid out for a short screen — **fixed 2026-11-16**

The numeric and pattern lock pages had no short-screen branch — nothing in
`pages.rs` tested a height like `< 700dp`. On the 1208×1392 cover the keypad
covered the title and the pattern's third row ran off the screen.

Both now have one: below 700 dp the words sit at the top, the keypad is scaled
by the room left (`keypad_height(t, scale)` and `keypad(…, scale)`), and the
pattern grid is shrunk to fit between the words and the bottom edge and centred
in that space. The grid's touch area was also twice the grid itself and is now
the grid. The regression test is `the_lock_pages_fit_the_cover_panel` in
`app.rs`.


### 4.3 The power key after the screen has gone dark

Measured on the phone: the screen rests at 120 s, the panel goes to
`FB_BLANK LP` at ~136 s, and then **no key event reaches the kernel log at all**.
Pressing the power key once leaves the screen dark, and the phone hard-resets
about ten seconds later — the PMIC's long-press protection, because nothing
answered the press.

`app.key()` does handle a dark panel:

```rust
if !self.display_on {
    if key == Key::Power { self.wake(now); }
    ...
}
```

so the gap is upstream of that: the key never arrives. Candidates, none checked
yet: the power key's evdev node stops reporting after `FB_BLANK LP`, the
`wakeful` epoll is not woken, or the node was never taken in. `input.rs` only
takes a device whose keys intersect `KEYS` (`POWER`, `VOLUMEUP`,
`VOLUMEDOWN`, `BACK`).

### 4.4 A USB mouse does nothing

Not a setting: there is no pointer support anywhere in `input.rs`. A mouse is
relative (`REL_X`/`REL_Y`) with `BTN_LEFT`, matches neither the `DIRECT` panel
branch nor the multitouch branch, and has none of `KEYS`, so `scan()` walks past
it. It enumerates and gets power, and the recovery never opens it.

### 4.5 adb drops while the recovery is still up

Seen twice after a power-key reset: the phone is in the recovery, the screen is
on, and `adb devices` is empty until `adb kill-server`. Worth a look at
`sys.usb.config` / the FunctionFS state at the time.

---

## 5. Building and flashing

CI builds both images on every dispatch; run
`.github/workflows/build.yml` with `device` empty.

```powershell
fastboot flash recovery_a skyward-<sha>-bixi-cover-first-OS4.0.0.11.XOHCNXM.img
fastboot reboot recovery
```

This phone's fastboot does **not** take `--slot=all`; flash `recovery_a`.

Images seen good on the phone:

| image | sha256 |
|---|---|
| `skyward-1ec121a-bixi-cover-first-OS4.0.0.11.XOHCNXM.img` | `908c054464fb6552f08fb0bb9878a9b5dda4b703831916b2628dd70b5d8f6c21` |
| `skyward-87a9217-bixi-cover-first-OS4.0.0.11.XOHCNXM.img` | `0353129c49e5e1b43c3a7ec115e30bc629e2bc5aad30220e4f2c8cb99b8360bb` (no `orientation`) |

---

## 6. What is known about the screen-off settings

For whoever picks up 4.3, so they do not have to re-derive it:

```rust
pub const SCREEN_OFF: [Option<u64>; 7] = [15s, 30s, 60s, 120s, 300s, 600s, None];
```

`State::default()` has `screen_off: 3` — **two minutes**. The last entry is
`None`, which the settings page shows as never turning off.

Two taps to wake (`wake_by_tap`) defaults to off. Turning it on has a cost
written into `ui.rs`: a panel two taps are to bring back cannot `rest()`, so it
stays lit at no brightness and **the phone no longer sleeps**:

```rust
if !app.state.wake_by_tap {
    ... inner.rest()?;     // else the controller stops following it
}
```

Measured timings on the phone, for reference:

```
123 s  backlight 790 → 785          dimming
131 s  power mode (Doze_Suspend)
136 s  backlight 0, DSI_CMD_SET_OFF
136 s  FB_BLANK LP                  after this, no key events in dmesg
```
