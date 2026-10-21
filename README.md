# FanForge

Rust fan control for Windows. Lists every fan header the Super I/O chip exposes, sets fixed speeds, and drives fans from temperature curves. There are no per-motherboard profiles: headers show up as Fan 1, Fan 2, … and you rename them in the Fans tab once you know which is which. (Earlier builds named the PRIME B350-PLUS headers CPU_FAN, CHA_FAN1 and CHA_FAN2; those are now Fan 1–3.) Fan control is supported on the ITE IT8655E (developed on an ASUS PRIME B350-PLUS), where it works from a cold boot without HWiNFO, on the ITE IT8689E, and on every other chip in the table below, from public driver sources; see [Supported chips](#supported-chips). Chips FanForge detects but can't control (the NCT6683D) are monitored only, with a notice.

The "only works after HWiNFO" bug comes from LibreHardwareMonitor issue #780: the firmware leaves the IT8655E on the wrong EC bank, and FanForge selects bank 0 before every access.

## Crates

| Crate | Binary | Purpose |
|---|---|---|
| `fanforge-core` | – | Curves, control engine, safety rules, config, published state. Platform independent. |
| `fanforge-win` | – | Small safe wrappers over Win32 (SDDL, named objects, pipes, process spawning, power and session events, shortcuts, System32 DLL loading, drive property queries). |
| `fanforge-hw` | – | PawnIO bindings, ISA/PCI bus mutexes, table-driven ITE and Nuvoton Super I/O drivers (bank-safe on the multi-bank ITE chips), AMD (SMN) and Intel (MSR) CPU temperatures, GPU and drive temperature sources. |
| `fanforge-control` | – | Control loop runtime, in-process controller, watchdog, guardian protocol, old-service removal. |
| `fanforge-gui` | `fanforge.exe` | egui desktop app with tray icon. Runs the control loop in-process and doubles as its own guardian (`--guardian`). |
| `fanforge-diag` | `fanforge-diag.exe` | Diagnostics: detect, read, temps, dump, diff, guarded set-pwm, restore. |

## Supported chips

| Chip | Fan control | Notes |
|---|---|---|
| ITE IT8655E | **Yes** (verified) | 3 headers, 6 temperatures |
| ITE IT8689E | **Yes** | 6 headers, 6 temperatures. Monitoring is tested on hardware; fan control isn't yet. The register addresses match LibreHardwareMonitor and the Linux `it87` drivers. Manual mode writes `0x7F` to the control register as LibreHardwareMonitor does; the Linux `it87` drivers do it differently |
| ITE IT8620E, IT8625E, IT8628E, IT8665E, IT8686E, IT8688E, IT8696E, IT8721F, IT8728F, IT8772E | **Yes** | Config port 0x2E. Manual mode clears bit 7 of the control register and keeps the rest, then writes the duty to `0x63`, `0x6B`, …, as LibreHardwareMonitor and the Linux `it87` drivers do; release puts the saved bytes back. The IT8665E and IT8625E use the alternate registers for headers 4–6. As the Linux `it87` drivers do, FanForge leaves a chip monitor only when its PWM configuration register (`0x14`) shows the known broken-BIOS state (`0x14 & 0x87 == 0`); the IT8655E, tested on hardware, skips that check |
| Nuvoton NCT6771F (NCT6775F), NCT6776F, NCT6779D, NCT6791D, NCT6792D, NCT6792D-A, NCT6793D, NCT6795D, NCT6796D, NCT6796D-R, NCT6797D, NCT6798D, NCT6799D | **Yes** | Config port 0x2E or 0x4E. Manual mode clears bits 7–4 of the fan mode register (`0x102`, `0x202`, …) and writes the duty to `0x109`, `0x209`, …, as the Linux `nct6775` driver and LibreHardwareMonitor do; release puts both saved bytes back |
| Nuvoton NCT6686D, NCT6687D (MSI) | **Yes** | An embedded controller behind the hardware-monitor base (page, index and data at base + 4, 5, 6). 8 headers and 7 temperatures. Each change goes through the controller's configuration handshake (request `0x80` at `0xA01`, wait for the engine status at `0xCF8`, done `0x40`), sets the header's bit in `0xA00` and writes the duty to `0xA28` + n, as the Linux `nct6687d` driver and LibreHardwareMonitor do. On MSI 800-series boards (B840, B850, B860, X870, Z890) the sources disagree on the system fan registers, so FanForge shows and controls only the CPU and Pump headers there |
| Nuvoton NCT6683D | Monitor only | Same controller, but the sources disagree on how to finish a fan change |

**When a chip is monitor only:** FanForge supports fan control on a chip when public sources (LibreHardwareMonitor, the Linux drivers and, for Nuvoton, the datasheets) agree on its whole control procedure: how to take a fan over, what to save and how to hand it back. On any chip without it, FanForge reads temperatures, fan speeds and fan modes, and never writes a fan register. That includes quit, crash and sleep, because the BIOS keeps the fans throughout. It doesn't touch `bios_snapshot.toml` either. The app shows a monitor-only notice that names the chip. Each row's `fan_control` flag in `crates/hw/src/chips.rs` decides it.

- NCT6796D-R shares its ID with NCT5585D, and NCT6799D with NCT6796D-S. FanForge can't tell them apart, so temperature labels may be off on the second chip of each pair. Both chips of each pair use the same fan-control registers.
- On NCT679x boards where the firmware locks the hardware-monitor I/O space, FanForge clears the lock whenever it sets up the chip (startup, resume and recovery after a failed read or an abandoned bus mutex), as the Linux `nct6775` driver does. It's a single config-space write. If the lock doesn't clear, FanForge reports it. `fanforge-diag read` and `set-pwm` clear it the same way; `detect` and `dump` never do, and the dump's LDN section shows register `0x28` as it was at that moment (bit 4 set means locked; anything that cleared it since boot, including FanForge itself, shows as unlocked).

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
- **Failures:** a source that errors, hangs (no fresh value for 3 polls) or reports a value outside 0–125 °C counts as a failed sensor, with the usual missing-sensor fallback. The critical sensor stays the CPU temperature (`cpu/package`) unless you change it; at startup FanForge waits up to 3 s for the first GPU and drive readings so a GPU or drive critical sensor doesn't trip the fallback.
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

`dump` only reads; it puts the bank register back exactly as it found it. If the diff shows `bank_register` changing (bits 5–6 of EC register `0x06` on ITE chips), that confirms the bank-select cause. On a Nuvoton NCT679x that the firmware has locked, `detect` says so and `dump` records the lock (register `0x28`) but skips the hardware-monitor registers, because they all read `0xFF` until the lock is cleared. If the diff shows them as "not read" on one side, the lock itself (register `0x28` changing in the diff) is the finding, not the bank register. `detect`, `read`, `dump` and `diff` work on every supported chip. `detect` prints the chip and whether FanForge supports fan control on it.

`fanforge-diag set-pwm --channel 2 --duty 40` sets one fan to a fixed speed. It returns that fan to the BIOS after the timeout, on Ctrl+C, or when the console is closed, and refuses anything below 30%. It also refuses on monitor-only chips.

## Safety model

- **Bus access:** every hardware access holds `Global\Access_ISABUS.HTP.Method`. On the IT8625E, IT8655E, IT8665E, IT8686E, IT8688E, IT8689E and IT8696E it selects EC bank 0 first, as the out-of-tree Linux `it87` driver does. On Nuvoton chips the hardware-monitor bank register is put back as it was found after every access. An abandoned mutex triggers a full chip re-initialisation, and that is refused if a different chip answers. Releases retry for up to 2 s when another tool holds the bus.
- **BIOS snapshot:** before FanForge first drives a channel, it saves that channel's BIOS settings and marks it "owned" in `bios_snapshot.toml`. It refuses manual control if that file can't be written. After a crash, owned channels are restored from the saved values; channels it doesn't own are re-captured on every start, so BIOS changes are picked up. On the classic ITE chips FanForge can rebuild the BIOS auto mode from the register itself, so every release, including the guardian's, puts every header back to auto. On Nuvoton chips, the NCT6686D/NCT6687D and the IT8689E it can't, so every release only touches headers FanForge drove itself, it refuses to drive a header the BIOS left in manual mode, and a header it drove with no saved settings goes to 100% instead of a guessed auto mode. If `bios_snapshot.toml` exists but can't be read, FanForge can't tell which headers it drove, so that start (or that fail-safe release) treats every manual header as its own and sends it to 100%. If the PWM check starts failing while FanForge owns headers, it stops driving them but still hands them back. A header another tool left at the IT8689E's manual value `0x7F` is left alone until FanForge drives it.
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
  - **Critical temperature override** (on by default): a critical CPU temperature (85 °C by default) drives every fan to 100%. The CPU temperature (`cpu/package`) is Tctl on AMD Zen 1 to Zen 5 and the package temperature on Intel (the hottest core on CPUs without a package sensor). Configs that named the old `cpu/tctl` sensor are moved to `cpu/package` when they load. Turned off in Settings, reaching it still raises a red alert, but the fans aren't forced.
  - While either is off, a warning that can't be dismissed says so. Floors, the 0 RPM gate, release to BIOS on exit and calibration's 75 °C stop stay on regardless.
- **Chips without fan control:** see [Supported chips](#supported-chips). FanForge stays monitor-only and never writes their fan registers.
- **Calibration:** **Calibrate** in a fan's detail page finds the duty at which that fan stops and restarts, and records its speed at each step.
  - **How it runs:** it steps the duty down from 100% in 5% steps. Each step waits at least 3 s and until the RPM settles.
  - **Floors still apply:** it never goes below the header floor. It only probes below the floor, down to 0%, on a case fan that is allowed to stop.
  - **When it stops early:** a temperature reaching 75 °C ends it, and so do a sensor or tach that stops reporting, the critical override or missing-sensor fallback taking over (when they're on), monitor-only mode, a config change, sleep, or quitting.
  - **Afterwards:** the fan gets 3 s at 100%, then its normal control returns.
  - **What it changes:** results are saved in the config and can only raise the floor: floor = max(header floor, minimum duty, stop duty + 10). They can also raise the spin-up duty to the start duty + 10. A fan that does not restart even at 100% stays flagged with a warning.
- **Conflicts:** when another fan controller is running (FanControl, SpeedFan, Fan Xpert, …), FanForge switches to monitor-only. It does not touch the chip at startup, and on sleep, resume, recovery and quit it only releases fans it drove itself, so it never overwrites the other tool's settings.
- **Config limits:** profile, curve, point and name counts are capped.

## Not yet implemented

- Testing the AMD GPU (ADL) path on real hardware
- Fan headers that have a tachometer but no PWM output (e.g. the IT8686E's 6th tach) are not shown
- ITE IT8705F/12F/16F/18F/20F/26F/71E (7-bit duty), the secondary IT879xE chips, Nuvoton NCT610xD/NCT6701D, Winbond and Fintek chips
- System fan headers on MSI 800-series boards with the NCT6687D
- Code signing
- Checking how Task Manager's "End task" treats the guardian on the target PC, and the scheduled-task guardian fallback if "End task" kills both processes

## Third-party components

`crates/hw/modules/*.bin` are signed PawnIO modules from [namazso/PawnIO.Modules](https://github.com/namazso/PawnIO.Modules), licensed LGPL-2.1. The ITE and Nuvoton register layouts follow LibreHardwareMonitor (MPL-2.0), the AMD temperatures follow the Linux `k10temp` driver, and the Intel temperatures follow the Linux `coretemp` driver and LibreHardwareMonitor. See [NOTICE](NOTICE).
