# JoyClicks

Merge a physical **gamepad (left hand)** and a **mouse (right hand)** into a single
**virtual gamepad** on Linux, so emulators that only accept one input device per pad
(e.g. **RPCS3**) get controller movement + mouse aim at the same time.

JoyClicks grabs both real devices (so the emulator only ever sees the virtual pad — no
double input), and converts relative mouse motion into a right-stick deflection with a
spring-back toward center: **turn rate ≈ mouse speed**, and the stick recenters when you
stop moving the mouse.

## Default mapping

| Input (physical) | → Virtual pad |
|---|---|
| DS4 left stick, D-pad, L1/L2/L3, face + Share/Options | passed through |
| DS4 **right stick** | **ignored** (mouse owns aim) |
| Mouse motion | **right stick** (aim) |
| Left click | R2 (fire) |
| Right click | L2 (ADS) |
| Middle click | R3 (melee) |
| Side / Extra mouse buttons | Cross (jump) / Square (reload) |
| Wheel up / down | D-pad up / down (weapon) |
| **PS button** | toggle grab — releases the mouse so you can alt-tab |

All of this is configurable in `~/.config/joyclicks/config.toml` (written with defaults on
first run).

## Run

```bash
~/projects/joyclicks/target/release/joyclicks
```

It prints the pad + mouse it locked onto, then creates **"JoyClicks Virtual Gamepad"**.
`Ctrl-C` cleanly ungrabs both devices and removes the virtual pad.

If it can't grab a device (permission denied): you're normally covered by the logind
`uaccess` ACL on a desktop session; otherwise run with `sudo`, or add a udev rule.

The right stick is emitted at **16-bit resolution** (vs a physical stick's ~8 bits) so slow
aim doesn't feel steppy.

## RPCS3 setup (one time)

1. Start JoyClicks **before** opening the RPCS3 Pads dialog.
2. RPCS3 → **Pads** → Handler = **SDL** (or evdev) → Device = **JoyClicks Virtual Gamepad**.
3. Bind the sticks/buttons (click each, move stick / press). Save.
4. **Set the right-stick Deadzone and Anti-Deadzone to 0** for this pad — RPCS3 auto-scales a
   large deadzone to the 16-bit axis on bind, which eats/snaps small aim. JoyClicks springs the
   stick to exact center when idle, so 0 gives no drift. (Leave the left stick's deadzone for
   controller movement.)
5. Play. Left hand on the controller, right hand on the mouse.

## Tuning (`config.toml`)

The `[aim]` section is **hot-reloaded live** — edit and save, and JoyClicks applies it within a
second without a restart or re-binding in the emulator (device names, `tick_hz`, and `[buttons]`
still need a restart).

`[aim]`
- `sensitivity` — mouse counts → stick deflection per count. Higher = faster aim.
- `y_scale` — vertical gain vs horizontal. Console shooters often turn slower vertically, so a
  hand-drawn circle comes out a wide oval; raise `y_scale` above 1.0 to round it (Turok ≈ 1.4).
- `decay` — per-tick spring-back, `0..1`. **Higher = holds deflection longer / registers slow
  movement but floatier; lower = snappier stop.** Main flicky-vs-dead balance knob.
- `low_end_boost` — anti-deadzone: while the mouse is moving, guarantees a minimum stick
  deflection so the smallest slow movements punch through the *game's* internal deadzone.
  Movement-gated (fades after you stop) so it doesn't drift. `0` = off; start ~`0.15`.
- `deadzone`, `max_deflection`, `invert_y`, `tick_hz` (default 500).

Known-good starting point (Turok on RPCS3): `sensitivity 0.03`, `y_scale 1.4`, `decay 0.87`,
`low_end_boost 0.20`.

`[buttons]` — remap any mouse button/wheel to a PS control
(`R2 L2 R1 L1 Cross Circle Square Triangle L3 R3 DpadUp DpadDown DpadLeft DpadRight None`).

`[devices]` — `pad_name` / `mouse_name` substrings if auto-detection picks the wrong device.

## Notes / limits

- Mouse→stick is still a *stick* to the game: it obeys the game's own aim deadzone /
  acceleration and the PS3's 255-step cap, so it won't feel like native PC mouse aim — but
  it's continuous and controllable.
- Reordering the `Control` enum in `config.rs` must stay in sync with `control_from_u8`
  in `main.rs` (discriminants are explicit to make this safe).
