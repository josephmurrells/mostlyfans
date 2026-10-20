# FanForge

Rust fan control for Windows. Lists every fan header the Super I/O chip exposes, sets fixed speeds, and drives fans from temperature curves. There are no per-motherboard profiles: headers show up as Fan 1, Fan 2, … and you rename them in the Fans tab once you know which is which. (Earlier builds named the PRIME B350-PLUS headers CPU_FAN, CHA_FAN1 and CHA_FAN2; those are now Fan 1–3.) Fan control is supported on the ITE IT8655E (developed on an ASUS PRIME B350-PLUS), where it works from a cold boot without HWiNFO, and on the ITE IT8689E. Other ITE and Nuvoton chips are detected and monitored only; see [Supported chips](#supported-chips).

The "only works after HWiNFO" bug comes from LibreHardwareMonitor issue #780: the firmware leaves the IT8655E on the wrong EC bank, and FanForge selects bank 0 before every access.

## Crates

| Crate | Binary | Purpose |
|---|---|---|
| `fanforge-core` | – | Curves, control engine, safety rules, config, published state. Platform independent. |
| `fanforge-win` | – | Small safe wrappers over Win32 (SDDL, named objects, pipes, process spawning, power and session events, shortcuts, System32 DLL loading, drive property queries). |
| `fanforge-hw` | – | PawnIO bindings, ISA/PCI bus mutexes, table-driven ITE and Nuvoton Super I/O drivers (bank-safe IT8655E, IT8665E and IT8689E), Ryzen SMN temperatures, GPU and drive temperature sources. |
| `fanforge-control` | – | Control loop runtime, in-process controller, watchdog, guardian protocol, old-service removal. |
| `fanforge-gui` | `fanforge.exe` | egui desktop app with tray icon. Runs the control loop in-process and doubles as its own guardian (`--guardian`). |
| `fanforge-diag` | `fanforge-diag.exe` | Diagnostics: detect, read, temps, dump, diff, guarded set-pwm, restore. |

## Supported chips

| Chip | Fan control | Notes |
|---|---|---|
| ITE IT8655E | **Yes** (verified) | 3 headers, 6 temperatures |
| ITE IT8689E | **Yes** | 6 headers, 6 temperatures. Monitoring is tested on hardware; fan control isn't yet. The register addresses match LibreHardwareMonitor and the Linux `it87` drivers. Manual mode writes `0x7F` to the control register as LibreHardwareMonitor does; the out-of-tree `it87` driver does it differently |
| ITE IT8620E, IT8625E, IT8628E, IT8665E, IT8686E, IT8688E, IT8721F, IT8728F, IT8772E | Monitor only | config port 0x2E |
| Nuvoton NCT6771F (NCT6775F), NCT6776F, NCT6779D, NCT6791D, NCT6792D, NCT6792D-A, NCT6793D, NCT6795D, NCT6796D, NCT6796D-R, NCT6797D, NCT6798D, NCT6799D | Monitor only | config port 0x2E or 0x4E |

**Why monitor only:** fan control is turned on chip by chip, once its register map has been checked against the available sources (LibreHardwareMonitor, the Linux drivers and, for Nuvoton, the datasheets) or tested on hardware. Until then, FanForge reads temperatures, fan speeds and fan modes on these chips, and never writes a fan register. That includes quit, crash and sleep, because the BIOS keeps the fans throughout. It doesn't touch `bios_snapshot.toml` either. The app shows a monitor-only notice that names the chip. A chip gets fan control once it is marked verified in `crates/hw/src/chips.rs`.

- NCT6796D-R shares its ID with NCT5585D, and NCT6799D with NCT6796D-S. FanForge can't tell them apart, so temperature labels may be off on the second chip of each pair.
- On NCT679x boards where the firmware locks the hardware-monitor I/O space, FanForge reports the lock rather than clearing it.

## Building

You need Rust 1.88+ and the MSVC toolchain (Visual Studio Build Tools with the C++ workload).

```powershell
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Setup

1. FanForge needs the signed **PawnIO** driver (<https://pawnio.eu>), and never uses WinRing0. If PawnIO is missing, the app shows an **Install PawnIO** button. It downloads the official PawnIO 2.2.0 installer from [its GitHub release](https://github.com/namazso/PawnIO.Setup/releases/tag/2.2.0) with Windows' `curl.exe`, over HTTPS only. The installer runs silently, and only if its SHA-256 matches the pinned value; fan control then starts without restarting FanForge. If Windows needs a restart to finish, the app says so. You can also install PawnIO yourself first.
2. FanForge is portable. Put `fanforge.exe` (from `target\release`) anywhere and run it. It asks for administrator rights. There's nothing to install.

Settings and logs live in `C:\ProgramData\FanForge`, which FanForge locks down to SYSTEM and Administrators. They aren't stored next to the exe, because an elevated app must not read files that a normal program could change.

FanForge controls the fans only while it runs. Closing the window hides it to the tray and control continues. **Quit FanForge** in the tray menu hands every fan back to BIOS automatic control and exits. Nothing runs in the background when FanForge isn't running.

- **Elevation:** release builds carry a `requireAdministrator` manifest, so every manual start shows a UAC prompt, including a second start that only brings the open window forward. The build is unsigned, so UAC shows "unknown publisher".
- **Start with Windows:** a toggle in Settings.
  - **On:** it copies the running exe to `C:\Program Files\FanForge` and registers a scheduled task (`FanForge`) that starts that copy with `--minimized` at sign-in, without a UAC prompt.
  - **Why a copy:** if the elevated task pointed at the portable exe in a folder you can write to, any program running as you could swap it and get it run elevated at sign-in.
  - **Updates:** whenever you run a newer portable exe, the copy is updated to match.
  - **Task limits:** the task never times out and ignores battery state.
  - **Repair:** if the task ever points somewhere else, Settings offers to repair it.
  - **Off:** removes the task and the copy.
- **One copy per PC:** a second start brings the open window forward. A start from another user session is refused.
- **Old service:** if the earlier service build is installed, FanForge offers to stop and remove it before taking control. If you decline, it only monitors.

To remove FanForge:
1. Turn off **Start with Windows**.
2. Quit FanForge from the tray.
3. Delete the exe.
4. Optionally, delete `C:\ProgramData\FanForge` to remove your settings.

Debug builds don't request elevation. Run them from an elevated terminal.

## GPU and drive temperatures

Besides the CPU and Super I/O sensors, FanForge reads GPU and drive temperatures. They show on the Overview and any curve can use them.

- **NVIDIA:** through NVML (`nvml.dll`, installed with the driver). Each GPU is identified by its NVML UUID, so a curve stays bound to the same card across reboots and slot moves.
- **AMD:** through ADL (`atiadlxx.dll`, installed with Adrenalin), edge temperature of discrete cards (the Radeon graphics built into Ryzen CPUs is skipped; its temperature tracks the CPU). This path has not been tested on real AMD hardware yet. Each GPU is identified by its PCI bus/device/function. That number can change when you add or remove a PCIe device or change BIOS PCIe settings; a curve bound to the old id then sees a missing sensor and its fan goes to 100% (or back to BIOS control if the missing-sensor fallback is off) until you pick the sensor again.
- **Drives:** NVMe drives through Windows' storage temperature query, SATA drives through their SMART table (attribute 194, else 190). Each drive is identified by its serial number. Drives without a serial, spinning hard disks (polling would keep them from spinning down) and drives that report no temperature are not listed.
- **Loading:** both GPU libraries are loaded from System32 only, with their dependencies, never from the folder FanForge runs from.
- **Polling:** GPUs are read every second and drives every 5 s, each on its own thread. They never touch the Super I/O bus, and a slow or hung driver call can't delay the control loop. A missing driver is retried every 30 s.
- **Failures:** a source that errors, hangs (no fresh value for 3 polls) or reports a value outside 0–125 °C counts as a failed sensor, with the usual missing-sensor fallback. The critical sensor stays CPU Tctl unless you change it; at startup FanForge waits up to 3 s for the first GPU and drive readings so a GPU or drive critical sensor doesn't trip the fallback.
- `fanforge-diag temps` prints these temperatures without PawnIO or administrator rights.

## Confirm the root cause on your PC

Run these from an elevated terminal, with FanForge quit:

```powershell
fanforge-diag detect
fanforge-diag dump --out cold.json
fanforge-diag dump --out after-hwinfo.json
fanforge-diag diff cold.json after-hwinfo.json
```

1. Disable Windows Fast Startup and every monitoring tool's autostart.
2. Shut down fully and boot.
3. Run `detect`, then the first `dump` (`cold.json`).
4. Start HWiNFO, then close it.
5. Run the second `dump` (`after-hwinfo.json`), then `diff`.

`dump` only reads; it puts the bank register back exactly as it found it. If the diff shows `bank_register` changing (bits 5–6 of EC register `0x06`), that confirms the bank-select cause. `detect`, `read`, `dump` and `diff` work on every supported chip. `detect` prints the chip and whether its fan control is verified.

`fanforge-diag set-pwm --channel 2 --duty 40` sets one fan to a fixed speed. It returns that fan to the BIOS after the timeout, on Ctrl+C, or when the console is closed, and refuses anything below 30%. It also refuses on monitor-only chips.

## Safety model

- **Bus access:** every hardware access holds `Global\Access_ISABUS.HTP.Method`. On the IT8655E, IT8665E and IT8689E it selects EC bank 0 first. On Nuvoton chips the hardware-monitor bank register is put back as it was found after every access. An abandoned mutex triggers a full chip re-initialisation, and that is refused if a different chip answers. Releases retry for up to 2 s when another tool holds the bus.
- **BIOS snapshot:** before FanForge first drives a channel, it saves that channel's BIOS settings and marks it "owned" in `bios_snapshot.toml`. It refuses manual control if that file can't be written. After a crash, owned channels are restored from the saved values; channels it doesn't own are re-captured on every start, so BIOS changes are picked up. On the IT8689E, manual mode overwrites the BIOS mode, so FanForge refuses to drive a header the BIOS left in manual mode, leaves such headers alone on release, and sets a header it left in manual mode with no saved settings to 100% instead of guessing an auto mode.
- **Fail-safe returns to BIOS control:**
  - **Quit, crash, sleep:** fans go back to BIOS control when you quit from the tray, the session ends (sign-out, restart, shutdown, Restart Manager), the system sleeps, or the control thread panics. A panic leaves a **Restart control** button.
  - **Stalled loop:** the in-process watchdog fires when the control loop stalls (3× the interval, at least 3 s). It releases the fans from the on-disk snapshot and exits.
  - **Guardian:** a second `fanforge.exe --guardian` process watches the app through an inherited process handle and an anonymous pipe. If the app is killed (`taskkill /F`, Task Manager), crashes, or stops sending heartbeats while it owns a fan (6 s by default), the guardian ends it if needed and returns every fan to BIOS control. If the guardian dies it is restarted, at most 3 times in 10 minutes, then the app warns that it is unprotected.
  - **Failed hardware:** if the hardware stops responding and can't be re-initialised, fans are released, through a fresh handle if needed, before it is dropped.
  - **Known limit:** "End process tree" kills both processes. Fans then stay at or above their floors until FanForge next starts and restores BIOS control. Power loss or a crash of Windows resets the chip to BIOS control.
- **Floors and fallbacks:**
  - FanForge can't tell for sure which header drives the CPU fan; header 1 is marked "usually the CPU fan" because that's the norm on ITE boards. Every header keeps a 30% floor with 0 RPM disabled until you tick **Case fan** in its settings. A case fan's floor drops to 20%, and it stops only if you also allow 0 RPM. The control loop enforces this whatever the config asks for.
  - 30% may be too low for a pump, so set a higher minimum duty for it.
  - Until a curve's sensor has read healthy, the fan stays under BIOS control.
  - **Missing-sensor fallback** (on by default): if a curve's sensor keeps failing, that fan goes to 100%, and if the safety sensor stops reporting, every fan does. After a safety-sensor dropout, fans return to normal once it reads below "Back to normal below" for the hold time; those two settings stay editable while either toggle is on. Turned off in Settings, a fan whose sensor fails holds its last speed for the same number of readings, then goes back to BIOS control instead, and a lost safety sensor only raises a warning. Config errors (a missing curve or a follow loop) still go to 100%.
  - **Critical temperature override** (on by default): a critical Tctl (85 °C by default) drives every fan to 100%. Turned off in Settings, reaching it still raises a red alert, but the fans aren't forced.
  - While either is off, a warning that can't be dismissed says so. Floors, the 0 RPM gate, release to BIOS on exit and calibration's 75 °C stop stay on regardless.
- **Unverified chips:** see [Supported chips](#supported-chips). FanForge stays monitor-only and never writes their fan registers.
- **Calibration:** **Calibrate** in a fan's detail page finds the duty at which that fan stops and restarts, and records its speed at each step.
  - **How it runs:** it steps the duty down from 100% in 5% steps. Each step waits at least 3 s and until the RPM settles.
  - **Floors still apply:** it never goes below the header floor. It only probes below the floor, down to 0%, on a case fan that is allowed to stop.
  - **When it stops early:** a temperature reaching 75 °C ends it, and so do a sensor or tach that stops reporting, the critical override or missing-sensor fallback taking over (when they're on), monitor-only mode, a config change, sleep, or quitting.
  - **Afterwards:** the fan gets 3 s at 100%, then its normal control returns.
  - **What it changes:** results are saved in the config and can only raise the floor: floor = max(header floor, minimum duty, stop duty + 10). They can also raise the spin-up duty to the start duty + 10. A fan that does not restart even at 100% stays flagged with a warning.
- **Conflicts:** when another fan controller is running (FanControl, SpeedFan, Fan Xpert, …), FanForge switches to monitor-only. It does not touch the chip at startup and only releases fans it drove itself.
- **Config limits:** profile, curve, point and name counts are capped.

## Not yet implemented

- Testing the AMD GPU (ADL) path on real hardware
- Fan control on chips other than the IT8655E and IT8689E (they are monitor-only until verified)
- Fan headers that have a tachometer but no PWM output (e.g. the IT8686E's 6th tach) are not shown
- ITE IT8705F/12F/16F/18F/20F/26F/71E (7-bit duty), the secondary IT879xE chips, Nuvoton NCT610xD/NCT668xD/NCT6701D, Winbond and Fintek chips
- A default critical sensor for systems without `cpu/tctl` (Intel, AMD Zen 5)
- Code signing
- Checking how Task Manager's "End task" treats the guardian on the target PC, and the scheduled-task guardian fallback if "End task" kills both processes

## Third-party components

`crates/hw/modules/*.bin` are signed PawnIO modules from [namazso/PawnIO.Modules](https://github.com/namazso/PawnIO.Modules), licensed LGPL-2.1. The ITE and Nuvoton register layouts follow LibreHardwareMonitor (MPL-2.0), and the AMD temperatures follow the Linux `k10temp` driver. See [NOTICE](NOTICE).
