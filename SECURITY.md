# Security policy

FanForge runs elevated and talks to the hardware through the PawnIO kernel driver, so security problems matter. Examples are a way for an unprivileged process to make FanForge write to the hardware, change its settings or take over its named pipes and objects, or a way to tamper with the PawnIO installer download.

## Reporting a vulnerability

Please don't open a public issue. Report it privately through GitHub's [private vulnerability reporting](https://github.com/josephmurrells/fanforge/security/advisories/new) instead, with the FanForge version or commit, the steps to reproduce it and what an attacker gains.

You should hear back within a week. Once a fix is released, the advisory is published with credit to you, unless you'd rather stay anonymous.

## Supported versions

Only the latest release and the `master` branch get security fixes.

## Out of scope

Bugs in the PawnIO driver or its modules belong to [PawnIO](https://github.com/namazso/PawnIO), and bugs in the graphics drivers to their vendors.
