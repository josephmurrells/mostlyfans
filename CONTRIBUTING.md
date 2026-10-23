# Contributing to FanForge

Thanks for helping. Bug reports, hardware reports and pull requests are all welcome.

## Reporting bugs and hardware

Use the [issue forms](https://github.com/josephmurrells/fanforge/issues/new/choose). The most useful things to include are:

- your fan controller chip (**Settings → About → Fan controller**);
- the latest log file (**Settings → Open log folder**);
- for motherboard fan problems, the output of `fanforge-diag detect` from an administrator terminal.

Reports from boards FanForge hasn't been tested on are valuable even when everything works. The [README](README.md#supported-hardware) and [docs/technical.md](docs/technical.md#chip-support) say which chips have been tested on real hardware.

## Building and testing

You need Rust 1.95+ and the MSVC toolchain (Visual Studio Build Tools with the C++ workload). Before opening a pull request, run:

```powershell
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same checks on `windows-latest`, plus a `cargo check` on the minimum supported Rust version (1.95). Debug builds don't ask for administrator rights, so the tests run unelevated; the app itself and most `fanforge-diag` commands need an elevated terminal and real hardware.

To check UI changes without the hardware, render every page to PNG files:

```powershell
$env:FANFORGE_SCREENSHOTS = "$PWD\screenshots"
cargo test -p fanforge-gui render_screenshots -- --ignored
```

## Code style

- Idiomatic Rust, formatted with `rustfmt`, with no clippy warnings.
- No code comments. Names and small functions carry the meaning. The one exception is the MPL-2.0 license header in the files derived from LibreHardwareMonitor.
- Safety is never relaxed: minimum duties, the 0 RPM gate, bank 0 for normal ITE access, and handing every fan back to the BIOS or driver on every exit path. A change that touches these needs a clear explanation in the pull request.
- New chip support is based on public sources (datasheets, the Linux hwmon drivers, LibreHardwareMonitor). Where the sources disagree or leave a gap, the chip or header stays monitor-only with a warning. Describe your sources in the pull request and credit them in [NOTICE](NOTICE).

## Licensing of contributions

FanForge is licensed under the [GPL-3.0-or-later](LICENSE). By submitting a contribution you agree that it's your own work, or that you have the right to submit it, and that it's licensed under the same terms.

Please don't copy code from the Linux kernel drivers: they're GPL-2.0-only, which isn't compatible with GPL-3.0. Reading them as a reference for register addresses and procedures is fine, as the existing code does. Code ported from LibreHardwareMonitor (MPL-2.0) must keep or add the MPL-2.0 header in that file.
