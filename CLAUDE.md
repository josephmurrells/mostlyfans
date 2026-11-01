# MostlyFans

Windows fan control (tray app, egui). It writes to Super I/O fan controllers and NVIDIA GPUs through the PawnIO kernel driver, so bugs can stop fans or overheat a PC. Repo and crate names are `mostlyfans*`; the folder and old docs still say FanForge.

- User docs: `README.md`. Internals (crates, chip support, safety, diag tool): `docs/technical.md`. Contributor rules: `CONTRIBUTING.md`.
- `PLAN.md` is gitignored and holds only the next work items.

## Workspace

`crates/core` (platform-independent domain: curves, engine, safety, config), `win` (safe Win32 wrappers), `hw` (PawnIO, Super I/O, CPU/GPU/drive sensors, NVIDIA fans), `control` (control loop, watchdog, guardian), `gui` (`mostlyfans.exe`), `diag` (`mostlyfans-diag.exe`). Edition 2024, MSRV 1.95.

## Commands

Run these before calling a change done, exactly as CI does:

```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- Check UI changes without hardware: `MOSTLYFANS_SCREENSHOTS=<dir> cargo test -p mostlyfans-gui render_screenshots -- --ignored`, then look at the PNGs.
- Dev builds use asInvoker, so tests run unelevated. Release builds require admin.
- **Never run** the app, `cargo run`, or `mostlyfans-diag` write commands (`set-pwm`, `set-gpu-fan`, `restore*`). They need elevation and real hardware, and the user does all hardware testing.

## Conventions (override the global `rust-style` skill where they differ)

- **No code comments**, `///` doc comments included. Names and small functions carry the meaning. There are two exceptions:
  - the MPL-2.0 header in files derived from LibreHardwareMonitor;
  - `// SAFETY:` comments on `unsafe` blocks and impls, explaining why the code is sound. Every new or changed `unsafe` block gets one.
- Workspace lints apply: `unwrap_used`, `todo` and `dbg_macro` warn, and `unsafe_op_in_unsafe_fn` denies. `unwrap` is allowed in tests.
- FFI and raw `unsafe` code belong in `crates/win` or `crates/hw` wrappers. Other crates only call those wrappers (e.g. the guardian's `inherited_process`/`inherited_pipe`), and business logic stays safe.
- Never relax safety: duty floors, the 0 RPM gate, ITE bank 0 selection, and handing every fan back to BIOS/driver on every exit path. Any change touching these needs a clear explanation.
- Chip support comes only from public sources (datasheets, Linux hwmon, LibreHardwareMonitor). If they disagree or leave a gap, the chip stays monitor-only. Never copy Linux driver code (GPL-2.0-only); reading it for register details is fine. Credit sources in `NOTICE`.
- Tests that create named objects must label them at the process's own integrity level, because CI runs elevated and local runs don't.
- `wgpu` is a direct dependency matching eframe's version: bump both together.
- `crates/control/src/legacy.rs` deliberately keeps the old FanForge service names.

## Workflow

- Claude commits on a feature branch. The user pushes and opens PRs.
- Every PR ends with a `Release x.y.z` commit bumping `version` in the root `Cargo.toml` (run `cargo check` so `Cargo.lock` follows). Use patch for fixes, minor for features.
- Plans and code are reviewed by a subagent using Crit before handing over.
