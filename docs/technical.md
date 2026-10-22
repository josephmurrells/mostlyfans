# FanForge technical notes

How FanForge works under the hood: the code layout, how each chip is driven, the safety internals and the diagnostics tool. For using the app, see the [README](../README.md).

## Crates

| Crate | Binary | Purpose |
|---|---|---|
| `fanforge-core` | – | Curves, control engine, safety rules, config, published state. Platform independent. |
| `fanforge-win` | – | Small safe wrappers over Win32 (SDDL, named objects, pipes, process spawning, power and session events, shortcuts, System32 DLL loading, drive property queries). |
| `fanforge-hw` | – | PawnIO bindings, ISA/PCI bus mutexes, table-driven ITE and Nuvoton Super I/O drivers (bank-safe on the multi-bank ITE chips), AMD (SMN) and Intel (MSR) CPU temperatures, GPU and drive temperature sources, NVIDIA GPU fan control. |
| `fanforge-control` | – | Control loop runtime, in-process controller, watchdog, guardian protocol, old-service removal. |
| `fanforge-gui` | `fanforge.exe` | egui desktop app with tray icon. Runs the control loop in-process and doubles as its own guardian (`--guardian`). |
| `fanforge-diag` | `fanforge-diag.exe` | Diagnostics: detect, read, temps, pm-table, dump, diff, guarded set-pwm, restore, gpu, guarded set-gpu-fan, restore-gpu. |

## Chip support

FanForge supports fan control on a chip when public sources (LibreHardwareMonitor, the Linux drivers and, for Nuvoton, the datasheets) agree on its whole control procedure: how to take a fan over, what to save and how to hand it back. Each row's `fan_control` flag in `crates/hw/src/chips.rs` decides it. There are no per-motherboard profiles: every header the chip exposes is listed as Fan 1, Fan 2, … and the user names them.

| Chip | Fan control | How it's driven |
|---|---|---|
| ITE IT8655E | Yes (tested on hardware) | 3 headers, 6 temperatures. The firmware can leave the chip on a non-zero EC bank (LibreHardwareMonitor issue #780, the "only works after HWiNFO" bug), so FanForge selects bank 0 before every access. Skips the PWM configuration check (see below). |
| ITE IT8689E | Yes (tested on hardware) | 6 headers, 6 temperatures. The register addresses match LibreHardwareMonitor and the Linux `it87` drivers. Manual mode writes `0x7F` to the control register as LibreHardwareMonitor does; the out-of-tree `frankcrawford/it87` driver instead disables the extra fan-curve vectors Gigabyte's BIOS sets up in banks 2 and 3. |
| ITE IT8620E, IT8625E, IT8628E, IT8665E, IT8686E, IT8688E, IT8696E, IT8721F, IT8728F, IT8772E | Yes | Config port 0x2E. Manual mode clears bit 7 of the control register and keeps the rest, then writes the duty to `0x63`, `0x6B`, …, as LibreHardwareMonitor and the Linux `it87` drivers do; release puts the saved bytes back. The IT8665E and IT8625E use the alternate registers for headers 4–6. As the Linux `it87` drivers do, FanForge leaves a chip monitor only when its PWM configuration register (`0x14`) shows the known broken-BIOS state (`0x14 & 0x87 == 0`). |
| Nuvoton NCT6771F (NCT6775F), NCT6776F, NCT6779D, NCT6791D, NCT6792D, NCT6792D-A, NCT6793D, NCT6795D, NCT6796D, NCT6796D-R, NCT6797D, NCT6798D, NCT6799D | Yes | Config port 0x2E or 0x4E. Manual mode clears bits 7–4 of the fan mode register (`0x102`, `0x202`, …) and writes the duty to `0x109`, `0x209`, …, as the Linux `nct6775` driver and LibreHardwareMonitor do; release puts both saved bytes back. |
| Nuvoton NCT6686D, NCT6687D (MSI) | Yes | An embedded controller behind the hardware-monitor base (page, index and data at base + 4, 5, 6). 8 headers and 7 temperatures. Each change goes through the controller's configuration handshake (request `0x80` at `0xA01`, wait for the engine status at `0xCF8`, done `0x40`), sets the header's bit in `0xA00` and writes the duty to `0xA28` + n, as the Linux `nct6687d` driver and LibreHardwareMonitor do. On MSI 800-series boards (B840, B850, B860, X870, Z890) the sources disagree on the system fan registers, so only the CPU and Pump headers are shown and controlled there. |
| Nuvoton NCT6683D | Monitor only | Same controller, but the sources disagree on how to finish a fan change. |

- **Monitor-only chips:** FanForge reads temperatures, fan speeds and fan modes and never writes a fan register, including on quit, crash and sleep, because the BIOS keeps the fans throughout. It doesn't touch `bios_snapshot.toml` either.
- **Shared IDs:** NCT6796D-R shares its ID with NCT5585D, and NCT6799D with NCT6796D-S. FanForge can't tell them apart, so temperature labels may be off on the second chip of each pair. Both chips of each pair use the same fan-control registers.
- **Nuvoton I/O lock:** on NCT679x boards where the firmware locks the hardware-monitor I/O space, FanForge clears the lock whenever it sets up the chip (startup, resume and recovery after a failed read or an abandoned bus mutex), as the Linux `nct6775` driver does. It's a single config-space write. If the lock doesn't clear, FanForge reports it. `fanforge-diag read` and `set-pwm` clear it the same way; `detect` and `dump` never do, and the dump's LDN section shows register `0x28` as it was at that moment (bit 4 set means locked; anything that cleared it since boot, including FanForge itself, shows as unlocked).
- **Not supported yet:** ITE IT8705F/12F/16F/18F/20F/26F/71E (7-bit duty), the secondary IT879xE chips on larger Gigabyte boards, Nuvoton NCT610xD/NCT6701D, Winbond and Fintek chips, and tach-only headers (e.g. the IT8686E's 6th tach).

## Safety internals

- **Bus access:** every hardware access holds `Global\Access_ISABUS.HTP.Method`, the mutex other monitoring tools use. On the IT8625E, IT8655E, IT8665E, IT8686E, IT8688E, IT8689E and IT8696E it selects EC bank 0 first, as the out-of-tree Linux `it87` driver does. On Nuvoton chips the hardware-monitor bank register is put back as it was found after every access. An abandoned mutex triggers a full chip re-initialisation, and that is refused if a different chip answers. Releases retry for up to 2 s when another tool holds the bus. SMU calls hold `Global\Access_PCI`.
- **BIOS snapshot:** before FanForge first drives a channel, it saves that channel's BIOS settings and marks it "owned" in `bios_snapshot.toml`. It refuses manual control if that file can't be written. After a crash, owned channels are restored from the saved values; channels it doesn't own are re-captured on every start, so BIOS changes are picked up.
  - On the classic ITE chips FanForge can rebuild the BIOS auto mode from the register itself, so every release, including the guardian's, puts every header back to auto.
  - On Nuvoton chips, the NCT6686D/NCT6687D and the IT8689E it can't, so every release only touches headers FanForge drove itself, it refuses to drive a header the BIOS left in manual mode, and a header it drove with no saved settings goes to 100% instead of a guessed auto mode.
  - If `bios_snapshot.toml` exists but can't be read, FanForge can't tell which headers it drove, so that start (or that fail-safe release) treats every manual header as its own and sends it to 100%.
  - If the PWM check starts failing while FanForge owns headers, it stops driving them but still hands them back. A header another tool left at the IT8689E's manual value `0x7F` is left alone until FanForge drives it.
- **Release paths:**
  - **Quit, session end, sleep, panic:** fans go back to BIOS control when the user quits from the tray, the session ends (sign-out, restart, shutdown, Restart Manager), the system sleeps, or the control thread panics. A panic leaves a **Restart control** button.
  - **Watchdog:** fires when the control loop stalls (3× the interval, at least 3 s). It releases the fans from the on-disk snapshot and exits.
  - **Guardian:** a second `fanforge.exe --guardian` process watches the app through an inherited process handle and an anonymous pipe. If the app is killed (`taskkill /F`, Task Manager "End task"), crashes, or stops sending heartbeats while it owns a fan (6 s by default), the guardian ends it if needed and returns every fan to BIOS control. If the guardian dies it is restarted, at most 3 times in 10 minutes, then the app warns that it is unprotected.
  - **Failed hardware:** if the hardware stops responding and can't be re-initialised, fans are released, through a fresh handle if needed, before it is dropped.
  - **Known limit:** "End process tree" kills both processes. Fans then stay at or above their floors until FanForge next starts and restores BIOS control. Power loss or a Windows crash resets the chip to BIOS control.
- **Floors:** the control loop enforces them whatever the config asks for. Every header has a 30% floor with 0 RPM disabled until it's marked **Case fan** (20% floor, and 0 RPM only with **Allow 0 RPM**). Header 1 is labelled "usually the CPU fan" because that's the ITE convention; the BIOS temperature-source bits can't tell headers apart reliably.
- **Fallbacks:** until a curve's sensor has read healthy, the fan stays under BIOS control. After the critical override or a safety-sensor dropout, fans return to normal once the safety sensor reads below "Back to normal below" (80 °C by default) for the hold time (10 s by default). With the critical override off, reaching the critical temperature still raises a red alert. With the missing-sensor fallback off, a fan whose sensor fails holds its last speed for the same number of readings, then goes back to BIOS control. Config errors (a missing curve or a follow loop) always go to 100%.
- **CPU temperature:** `cpu/package` is Tctl on AMD Zen 1 to Zen 5 and the package temperature on Intel (the hottest core on CPUs without a package sensor). Configs that named the old `cpu/tctl` sensor, or a per-core sensor, are moved to `cpu/package` when they load.
- **Calibration:** steps the duty down from 100% in 5% steps, each at least 3 s and until the RPM settles, never below the header floor (only a case fan allowed to stop is probed down to 0%). It stops early at 75 °C, on a lost sensor or tach, a safety override, monitor-only mode, a config change, sleep or quit, then gives the fan 3 s at 100%. Results can only raise the floor: floor = max(header floor, minimum duty, stop duty + 10), and the spin-up duty to start duty + 10.
- **Conflicts:** the process list is checked every 5 s for FanControl, SpeedFan, Argus Monitor, ASUS Fan Xpert, AI Suite 3 and Armoury Crate. While one runs, FanForge is monitor-only: it doesn't touch the chip at startup, and on sleep, resume, recovery and quit it only releases fans it drove itself.
- **Config limits:** profile, curve, point and name counts are capped.
- **Files:** settings and logs live in `C:\ProgramData\FanForge`, locked down to SYSTEM and Administrators, because an elevated app must not read files a normal program could change. "Start with Windows" copies the exe to `C:\Program Files\FanForge` for the same reason, and registers a scheduled task (`FanForge`, `--minimized`, no time limit, ignores battery state). Release builds embed `requireAdministrator`; debug builds use `asInvoker` so tests can spawn them.
- **Process rules:** one copy per PC; a second start brings the open window forward (it still shows a UAC prompt, since the manifest requires elevation), and a start from another user session is refused. If the autostart task points somewhere else, Settings offers **Repair**. The Program Files copy is refreshed whenever a different portable exe runs. If the old service build is installed, FanForge offers to stop and remove it; declining leaves it monitor-only.

## GPU fan control

NVIDIA only so far, through the documented NVML calls (`nvmlDeviceGetNumFans`, `nvmlDeviceGetMinMaxFanSpeed`, `nvmlDeviceGetFanControlPolicy_v2`, `nvmlDeviceGetTargetFanSpeed`, `nvmlDeviceSetFanSpeed_v2`, `nvmlDeviceSetDefaultFanSpeed_v2`). Setting a speed needs administrator rights.

- **Ids and limits:** each fan is `gpu/nvidia/<uuid>/fan<N>`, so ids survive reboots and PCIe changes. A fan's kind comes from its id, so a GPU fan can never get header limits: its floor is the driver minimum (never below 30%), it never stops, and the config rejects **Case fan**, **Allow 0 RPM**, calibration and following a fan of the other kind. A GPU fan whose minimum isn't known yet gets no commands. Outputs are whole percent, because that's all the driver holds.
- **Worker:** a `gpu-fans` thread owns the NVML session so a slow or hung driver (TDR, driver update) can never stall the control loop and trip the watchdog. The loop leaves the newest command per fan in a slot (never a queue) and reads the latest sample. The worker checks the halt flag right before every write. Readings older than max(3 intervals, 3 s) mark every GPU fan "GPU driver not responding"; that clears by itself when the driver answers again. A failed or panicking driver is dropped and reopened every 10 s.
- **Reading back:** the target speed, not the measured speed, which ramps for several seconds. While the driver still holds the last value written, the newest pending command is reported, so the engine's mismatch check isn't tripped by the worker's lag.
- **Claiming:** a fan already in manual mode that FanForge doesn't own is never claimed and shows "controlled by another program". Before the first write to a fan, it's recorded in `gpu_owned.toml`; if that file can't be written, the fan isn't touched.
- **Release:** `nvmlDeviceSetDefaultFanSpeed_v2` hands the fan back to the driver, whose own curve (with 0 RPM at idle) resumes at once, and the fan leaves `gpu_owned.toml`. A manual speed survives the process being killed, so every release path covers GPU fans: quit, session end, sleep, the watchdog, the controller's direct release and the guardian. Those fail-safe paths use a spare NVML session opened at startup, run alongside the chip release within the same budget, and never rewrite `gpu_owned.toml`. At the next start, fans left in the file are handed back; if the file is unreadable, every GPU fan in manual mode is. The guardian keeps running without PawnIO as long as NVML opens.
- **Board failures stay apart:** the board keeps its own lock-timeout, re-initialisation and retry handling. While the board read is skipped, GPU fans still get commands using the last good board sensors, until the busy-bus warning limit; after that those sensors count as missing.
- **Conflicts:** MSI Afterburner, EVGA Precision X1 and ASUS GPU Tweak III make only GPU fans monitor-only; the fan tools listed under Safety make every fan monitor-only.
- **Known limit:** "End process tree" leaves GPU fans at their last speed until the next start, a driver reset or a restart.

## GPU and drive temperatures

- **NVIDIA:** NVML (`nvml.dll`). Each GPU is identified by its NVML UUID.
- **AMD:** ADL (`atiadlxx.dll`): edge, plus hotspot (junction) and memory from the PMLOG buffer where the card reports them. Integrated Radeon graphics are skipped. Each GPU is identified by PCI bus/device/function, which can change when PCIe devices or BIOS PCIe settings change.
- **Drives:** NVMe through the Windows storage temperature query, SATA through SMART (attribute 194, else 190). Extra NVMe sensors come from the temperature query's extra entries or the NVMe health log (sensors 1–8, Kelvin). Drives are identified by serial; drives without one, spinning hard disks (polling would stop spin-down) and drives with no temperature are skipped.
- **Loading and polling:** both GPU libraries load from System32 only. GPUs are read every second and drives every 5 s, each on its own thread, never touching the Super I/O bus. A missing driver is retried every 30 s. A source that errors, hangs (no fresh value for 3 polls) or reads outside 0–125 °C counts as failed. At startup FanForge waits up to 3 s for the first readings.

## Per-core temperatures

Read every 2 s on their own thread. Display only: `Readings::temperature` returns `None` for `cpu/core<N>` ids, and the config rejects them as a curve or safety sensor.

- **Intel:** `IA32_THERM_STATUS` per physical core (one affinity hop per core, processor groups handled), with P/E labels from the core's efficiency class on hybrid CPUs.
- **AMD:** the SMU PM table through PawnIO's `RyzenSMU` module. The layout depends on the PM table version, so only allow-listed versions (from `ryzen_monitor`, Ryzen 3000 and 5000, 8 and 16 cores) are used, and only when the table's core count matches the CPU's physical core count, which excludes 6- and 12-core parts until the core-disable map is ported. Only `0x380905` (Ryzen 7 5700X3D, index 188) has been confirmed against HWiNFO on hardware. Every round is cross-checked: all cores plausible, and the hottest core on each CCD within range of that CCD's temperature (or Tctl). 3 failed rounds in a row turn per-core readings off until restart.

## Diagnostics (`fanforge-diag`)

Run from an elevated terminal, with FanForge quit for `dump` and `set-pwm`. `detect`, `read`, `dump` and `diff` work on every supported chip.

- `detect`: the board, the chip(s) found on ports 0x2E and 0x4E, header and temperature counts, and whether fan control is supported.
- `read`: one reading of every sensor and fan.
- `temps`: GPU and drive temperatures, without PawnIO or administrator rights.
- `pm-table [--raw]`: the CPU code name, SMU firmware and PM table version, Tctl, CCD and per-core temperatures if the layout is known; compare them with HWiNFO's "Core0 (CCD1)" values. The list shows every core slot in the table, so on parts with cores fused off it includes slots HWiNFO skips and the numbering can differ. `--raw` prints every value so an unknown version can be mapped (`fanforge-diag pm-table --raw | Out-File -Encoding utf8 pm-table.txt`). It asks the SMU to refresh the table, the same request HWiNFO and ZenTimings make, holding `Global\Access_PCI`; it changes no settings and works while FanForge runs.
- `dump --out file.json` and `diff a.json b.json`: snapshot every register (read only; the bank register is put back as found) and compare two snapshots.
- `set-pwm --channel 2 --duty 40`: sets one fan to a fixed speed, returning it to the BIOS after the timeout, on Ctrl+C or when the console closes. Refuses below 30% and on monitor-only chips.
- `gpu`: every NVIDIA GPU fan with its range, speed, target, RPM and mode (no administrator rights needed).
- `set-gpu-fan --fan 1 --duty 60`: sets one GPU fan to a fixed speed, returning it to the driver after the timeout or on Ctrl+C. Refuses below the driver minimum or 30%, and fans another program already set to manual. A killed `set-gpu-fan` leaves the fan in manual mode; `restore-gpu` hands every NVIDIA fan back to the driver.

**Investigating an "only works after another tool" bug:** disable Fast Startup and every monitoring tool's autostart, shut down fully and boot, run `detect` and `dump --out cold.json`, start and close the other tool, then `dump --out after.json` and `diff cold.json after.json`. A changing `bank_register` (bits 5–6 of EC register `0x06` on ITE chips) points at the bank-select cause. On a locked Nuvoton NCT679x, the lock itself (register `0x28`) is the finding.

## Not yet implemented or untested

- AMD Radeon fan control.
- The AMD GPU (ADL) path hasn't been tested on a Radeon. A curve bound to a Radeon whose PCI id changed sees a missing sensor and its fan goes to 100% (or BIOS control with the fallback off) until the sensor is picked again.
- AMD per-core temperatures on 6- and 12-core parts, and PM table layouts beyond the allow-list.
- The secondary IT879xE chip on larger Gigabyte boards, and the other chips listed under "Not supported yet".
- System fan headers on MSI 800-series boards with the NCT6687D.
- Code signing.

## Development

```powershell
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Rust 1.95+ with the MSVC toolchain. Debug builds don't request elevation; run them from an elevated terminal to reach the hardware. To render every page without hardware: `$env:FANFORGE_SCREENSHOTS="<dir>"; cargo test -p fanforge-gui render_screenshots -- --ignored`.
