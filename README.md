# FanForge

Fan control for Windows. FanForge reads your motherboard's fan sockets and temperature sensors and drives your case, CPU and NVIDIA graphics card fans from temperature curves you set. It's a small portable app that lives in the system tray.

- **Curves for every fan:** follow the CPU, GPU, a drive or a motherboard sensor, or run at a fixed speed. Three profiles to start with (Silent, Balanced, Performance), each with its own fan settings, switchable from the window or the tray.
- **Every temperature in one place:** CPU (including each core on supported CPUs), motherboard, GPU and drive temperatures, with the lowest and highest since start and a short history. Pin the ones you care about to the Overview.
- **Safe by default:** fans never drop below a minimum speed, go to full speed if the CPU gets too hot or a sensor fails, and go back to your BIOS fan settings (or the graphics driver's) when FanForge stops.
- **Works on its own:** no other monitoring tools needed. FanForge talks to the hardware through the signed PawnIO driver.

A few terms used below: a **fan header** is a fan socket on the motherboard; **duty** is how much power a fan gets, in %; **BIOS control** means the motherboard's own fan settings are in charge.

## Requirements

- Windows 11, 64-bit, and administrator rights. Windows 10 should work but hasn't been tested.
- A motherboard with a supported ITE or Nuvoton fan controller chip, found on many ASUS, Gigabyte, MSI and ASRock boards, or an NVIDIA graphics card. See [Supported hardware](#supported-hardware).
- The [PawnIO](https://pawnio.eu) driver for the motherboard fans. FanForge offers to install it for you. Graphics card fans work without it.

## Getting started

1. Download `fanforge.exe` from the [Releases](https://github.com/josephmurrells/fanforge/releases) page (until the first release is published, [build it from source](#building-from-source)) and put it anywhere you like. There's nothing to install.
2. Run it and accept the administrator prompt.
3. If PawnIO isn't installed, click **Install PawnIO**. FanForge downloads the official installer, checks it hasn't been tampered with and installs it. Fan control then starts on its own, unless Windows asks for a restart first.
4. On the Overview, click **Start setup** under **Set up your fans**. It walks you through each header: **Identify** spins the fan at full speed for 3 seconds so you can see which one it is, then you name it and set its **Header type** (**Case fan** allows lower speeds than **CPU fan or pump**). You can do the same later on the **Fans** page.
5. On the **Curves** page, click **New curve** and pick a type (custom points, straight line, fixed speed or a combination of curves).
6. On the **Fans** page, set each fan's **Control** to **Curve** and pick your curve. Fans you don't change stay under BIOS control.
7. Click **Apply** (or press Ctrl+S). No change takes effect until you apply it; **Revert** throws your edits away.

Optionally, click **Calibrate** on a fan to find the speed where it stops and starts, so its minimum speed is set from real measurements.

Closing the window keeps FanForge running in the tray. To stop it, choose **Quit FanForge** from the tray menu, and your fans go straight back to BIOS control. To start FanForge automatically, turn on **Start with Windows** in Settings: it starts hidden in the tray at sign-in, with no administrator prompt.

## Pages

- **Overview:** the temperatures and fans that matter most at a glance. Right-click a tile to unpin it.
- **Temperatures:** every sensor FanForge can read, grouped by CPU, motherboard, GPU, drives and other. Pin any of them to the Overview, or use **Reset Overview to automatic** to go back to the default selection. Sensors that aren't connected are folded away under "Not reporting".
- **Fans:** each fan's speed and settings: **Control** (BIOS, fixed, curve, or follow another fan), name, header type, **Minimum duty**, **Allow 0 RPM**, Identify, Calibrate and Pin to Overview. Graphics card fans are listed in their own group, with **Driver** instead of BIOS.
- **Curves:** create and edit temperature curves, and choose how quickly each one responds (Fast, Smooth or Gentle).
- **Settings:** **Monitor only**, profiles, the safety sensor and critical temperature, the safety switches, Start with Windows, and **About** (which fan controller chip you have, and **Open log folder**).

## Safety

FanForge is built so a mistake, a crash or a bad setting can't leave your PC without cooling.

- **Minimum speeds:** every fan keeps at least 30%, or 20% once you mark it as a case fan. A fan only stops completely if it's a case fan and you turn on **Allow 0 RPM**. A pump may need more than 30%, so give it a higher **Minimum duty**. Graphics card fans keep at least the driver's minimum (and never less than 30%) and never stop while FanForge drives them; set their **Control** to **Driver** to get the card's own quiet mode with 0 RPM at idle back.
- **Critical temperature:** if the CPU reaches 85 °C (you can change this), every fan goes to full speed until it cools below the "Back to normal below" temperature. That includes graphics card fans left on **Driver**.
- **Failed sensors:** if a sensor a curve uses stops reporting, that fan goes to full speed. If the safety sensor stops, every fan does.
- **Fans that don't follow:** if a motherboard fan stops while FanForge asks it to spin, FanForge runs it at full speed for 2 seconds, then hands it back to the BIOS and shows why. It does the same, without the full-speed burst, when a fan's speed doesn't change with its duty (for example a 3-pin fan on a PWM header, or a pump). Fix the cause, then press **Try again**. For a pump or a fixed-speed fan, turn off **Response check** in that fan's **Advanced** settings.
- **Back to BIOS control:** when you quit, sign out, shut down or sleep, or if FanForge crashes or is stopped with **End task** in Task Manager, every fan goes back to your BIOS fan settings, and graphics card fans go back to the driver. A small watchdog process handles this even if the main app is killed. The one exception is Task Manager's **End process tree**, which stops the watchdog too: fans then stay at their last speed, never below their minimum, until FanForge next starts (or, for graphics card fans, until the PC restarts).
- **Other fan tools:** if FanControl, SpeedFan, Argus Monitor, Fan Xpert, AI Suite 3 or Armoury Crate is running, FanForge only monitors and never touches the fans, so the two can't fight. If MSI Afterburner, EVGA Precision X1 or ASUS GPU Tweak III is running, only the graphics card fans are left alone. A graphics card fan that another program has already set to a manual speed is never taken over.

To pause fan control without quitting, turn on **Monitor only** in Settings or the tray menu. Every fan goes back to the BIOS (and graphics card fans to the driver) while FanForge keeps showing temperatures and speeds, and the safety overrides don't act, just as if FanForge were closed. **Resume control** in the notice turns it off again, and so does restarting FanForge.

You can turn off the critical-temperature and failed-sensor overrides in Settings. A warning stays on screen while either is off, and reaching the critical temperature still shows a red alert. The minimum speeds and the return to BIOS control are always on.

## Supported hardware

**Fan control**

| Chip | Fan control |
|---|---|
| ITE IT8620E, IT8625E, IT8628E, IT8655E, IT8665E, IT8686E, IT8688E, IT8689E, IT8696E, IT8721F, IT8728F, IT8772E | Yes |
| Nuvoton NCT6771F / NCT6775F, NCT6776F, NCT6779D, NCT6791D, NCT6792D, NCT6792D-A, NCT6793D, NCT6795D, NCT6796D, NCT6796D-R / NCT5585D, NCT6797D, NCT6798D, NCT6799D / NCT6796D-S | Yes |
| Nuvoton NCT6686D (MSI) | Yes |
| Nuvoton NCT6687D (MSI) | Yes. On MSI 800-series boards (B840, B850, B860, X870, Z890), only the CPU and Pump headers |
| Nuvoton NCT6683D | Monitoring only |

To see which chip you have, open **Settings → About → Fan controller**. Headers the board doesn't wire as fan headers are hidden, and one whose control pin the board uses for something else shows its speed but can't be controlled. On a chip that isn't in this table, FanForge finds no fan headers and says so. Some larger Gigabyte boards have a second fan chip; fans on that second chip don't show up yet.

**Graphics card fans**

- **NVIDIA:** GeForce cards from the GTX 900 series on with driver fan control (most desktop cards; laptops usually don't allow it), through the driver's NVML. Tested on an RTX 4070 SUPER.
- **AMD Radeon:** not yet; planned.

**Temperatures**

- **CPU:** AMD Ryzen and Intel Core. Per-core temperatures on Intel, and on 8- and 16-core Ryzen 3000 and 5000 desktop CPUs (6- and 12-core models not yet). Per-core readings are for display; curves use the overall CPU temperature.
- **GPU:** NVIDIA through the driver's NVML, and AMD Radeon through Adrenalin (edge, hotspot and memory). Radeon support hasn't been tested on real hardware yet.
- **Drives:** NVMe and SATA SSDs. Spinning hard disks aren't polled, so they can still spin down.

## Running alongside other tools

- **Monitoring tools** like HWiNFO and HWMonitor can run at the same time. LibreHardwareMonitor and OpenHardwareMonitor can too, but FanForge shows a warning while they run, because their fan control would fight FanForge's; leave it off.
- **Other fan controllers** put FanForge into monitoring-only mode while they run (see [Safety](#safety)).
- **MSI Afterburner** and similar GPU tools are fine for overlays and overclocking. While one runs, FanForge leaves the graphics card fans to it and keeps controlling the rest. If you want FanForge to drive the graphics card fans, close the tool, or turn its fan control off and restart it so the fans go back to the driver.
- **Anti-cheat:** FanForge uses PawnIO, a signed kernel driver, to reach the hardware. Some anti-cheat systems are wary of hardware-access drivers. PawnIO stays installed after you quit FanForge. If a game objects, uninstalling PawnIO from **Settings > Apps** removes it, but other tools that use PawnIO (such as FanControl and LibreHardwareMonitor) will stop working too.
- **Windows warnings:** FanForge isn't code-signed yet, so Windows SmartScreen and the administrator prompt show "unknown publisher".

## Troubleshooting

- **No fans, or monitoring only:** the notice at the top of the window says why. Usually PawnIO is missing, another fan tool is running, or the chip isn't supported for fan control yet.
- **A fan doesn't respond:** in the BIOS, set that header to an automatic fan curve (not "Full speed" or "Manual"), using PWM mode for 4-pin fans and DC/Voltage mode for 3-pin fans. Then check the message for that fan on the Fans page.
- **Logs:** **Settings → Open log folder**, or `C:\ProgramData\FanForge\logs`. Your settings are in `C:\ProgramData\FanForge`.
- **Reporting a problem:** open an [issue](https://github.com/josephmurrells/fanforge/issues) with your fan controller chip (Settings → About) and the latest log file.

## Updating

Quit FanForge from the tray, replace `fanforge.exe` with the new one, and run it once. If **Start with Windows** is on, its copy updates itself.

## Uninstalling

1. Turn off **Start with Windows** in Settings.
2. Quit FanForge from the tray.
3. Delete `fanforge.exe`.
4. Optionally delete `C:\ProgramData\FanForge` (your settings), and uninstall PawnIO from **Settings > Apps** if no other tool needs it.

## Building from source

You need Rust 1.95+ and the MSVC toolchain (Visual Studio Build Tools with the C++ workload).

```powershell
cargo build --release
```

The app is `target\release\fanforge.exe`, and the diagnostics tool is `target\release\fanforge-diag.exe`. How FanForge drives each chip, the safety internals and the diagnostics tool are described in [docs/technical.md](docs/technical.md).

## License

Copyright (C) 2026 Joseph Murrells.

FanForge is free software, licensed under the [GNU General Public License v3.0 or later](LICENSE). `crates/hw/src/chips.rs`, `ite.rs` and `nuvoton.rs` are derived from LibreHardwareMonitor and are also covered by the [Mozilla Public License 2.0](LICENSE-MPL-2.0). It embeds signed [PawnIO modules](https://github.com/namazso/PawnIO.Modules) (LGPL-2.1, [license text](crates/hw/modules/LICENSE-LGPL-2.1), source at the [0.2.11 tag](https://github.com/namazso/PawnIO.Modules/tree/0.2.11)). Third-party components keep their own licenses, listed in [NOTICE](NOTICE).
