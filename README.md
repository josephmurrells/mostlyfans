# FanForge

Rust fan control for Windows. Lists every motherboard fan, sets fixed speeds, and drives fans from temperature curves. It is built for the **ASUS PRIME B350-PLUS** (ITE IT8655E) and works from a cold boot without HWiNFO.

The "only works after HWiNFO" bug comes from LibreHardwareMonitor issue #780: the firmware leaves the IT8655E on the wrong EC bank, and FanForge selects bank 0 before every access.

## Crates

| Crate | Binary | Purpose |
|---|---|---|
| `fanforge-core` | – | Curves, control engine, safety rules, config, IPC protocol. Platform independent. |
| `fanforge-win` | – | Small safe wrappers over Win32 (SDDL, registry, process list). |
| `fanforge-hw` | – | PawnIO bindings, ISA/PCI bus mutexes, bank-safe IT8655E driver, Ryzen SMN temperatures. |
| `fanforge-ipc` | – | Hardened named pipe transport. |
| `fanforge-service` | `fanforge-service.exe` | Windows service (LocalSystem): control loop, watchdog, IPC server. |
| `fanforge-gui` | `fanforge.exe` | egui desktop app with tray icon. Unprivileged. |
| `fanforge-diag` | `fanforge-diag.exe` | Diagnostics: detect, read, dump, diff, guarded set-pwm, restore. |

## Building

You need Rust 1.88+ and the MSVC toolchain (Visual Studio Build Tools with the C++ workload).

```powershell
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Setup

1. Install the signed **PawnIO** driver from <https://pawnio.eu>. FanForge never uses WinRing0.
2. From an elevated terminal, run `target\release\fanforge-service.exe install`. This:
   - copies the service binary to `C:\Program Files\FanForge` (so the SYSTEM service never runs a user-writable file);
   - locks down `C:\ProgramData\FanForge` to SYSTEM and Administrators;
   - registers the service with automatic start and restart-on-failure, then starts it.
3. Run `target\release\fanforge.exe` as your normal user.

To remove the service, run `fanforge-service.exe uninstall`. It stops the service first, which hands every fan back to the BIOS.

`fanforge-service run` starts the same control loop in a console for debugging; it refuses to start unless the terminal is elevated. Debug builds of the GUI accept `FANFORGE_DEV_ANY_SERVER=1` to talk to a console instance; release builds always verify that the pipe belongs to the installed service.

## Confirm the root cause on your PC

Run these from an elevated terminal, with the FanForge service stopped:

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

`dump` only reads; it puts the bank register back exactly as it found it. If the diff shows `bank_register` changing (bits 5–6 of EC register `0x06`), that confirms the bank-select cause.

`fanforge-diag set-pwm --channel 2 --duty 40` sets one fan to a fixed speed. It returns that fan to the BIOS after the timeout, on Ctrl+C, or when the console is closed, and refuses anything below 30%.

## Safety model

- **Bus access:** every hardware access holds `Global\Access_ISABUS.HTP.Method` and selects EC bank 0 first. An abandoned mutex triggers a full chip re-initialisation. Releases retry for up to 2 s when another tool holds the bus.
- **BIOS snapshot:** before FanForge first drives a channel, it saves that channel's BIOS settings and marks it "owned" in `bios_snapshot.toml`. It refuses manual control if that file can't be written. After a crash, owned channels are restored from the saved values; channels it doesn't own are re-captured on every start, so BIOS changes are picked up.
- **Fail-safe returns to BIOS control:**
  - The service hands the fans back to BIOS automatic control when it stops, the system shuts down, the system suspends, or the control loop panics (the runtime releases fans while unwinding).
  - The watchdog does the same if the control loop stalls, using the on-disk snapshot; it then exits so the service manager restarts it.
  - If the hardware stops responding and can't be re-initialised, fans are released, through a fresh handle if needed, before it is dropped.
- **Floors and fallbacks:**
  - Speed floors are enforced inside the service, whatever a client asks for.
  - The PRIME B350-PLUS profile is still unverified, so every fan keeps a 30% floor and 0 RPM is disabled.
  - Until a curve's sensor has read healthy, the fan stays under BIOS control; if the sensor keeps failing, that fan goes to 100%.
  - A critical Tctl (85 °C by default), or a critical sensor that stops reporting, drives every fan to 100%.
- **Conflicts:** when another fan controller is running (FanControl, SpeedFan, Fan Xpert, …), FanForge switches to monitor-only. It does not touch the chip at startup and only releases fans it drove itself.
- **Config limits:** profile, curve, point and name counts are capped, so a client can't push a config too large to serve back.

## Not yet implemented

- Calibration
- GPU temperature sensors (NVML)
- MSI installer
- Nuvoton chips and other boards
- Code signing

Also open from the code review:

- IPC request timeouts and rate limiting
- Optimistic-concurrency revisions on config writes

## Third-party components

`crates/hw/modules/*.bin` are signed PawnIO modules from [namazso/PawnIO.Modules](https://github.com/namazso/PawnIO.Modules), licensed LGPL-2.1. The register layout follows LibreHardwareMonitor (MPL-2.0) and the Linux `k10temp` driver. See [NOTICE](NOTICE).
