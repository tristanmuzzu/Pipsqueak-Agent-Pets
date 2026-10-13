<p align="center"><img src="assets/portrait-byte.png" width="120" alt="Byte, a pixel-art robot" /></p>

# Pipsqueak

A desktop pet that tells you what Claude Code and Codex are doing.

[![MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![CI](https://github.com/tristanmuzzu/Pipsqueak-Agent-Pets/actions/workflows/ci.yml/badge.svg)](https://github.com/tristanmuzzu/Pipsqueak-Agent-Pets/actions/workflows/ci.yml)
[Download for Windows or Linux](https://github.com/tristanmuzzu/Pipsqueak-Agent-Pets/releases/latest)

I kept checking terminals to see whether an agent had finished or was waiting
for me. Pipsqueak puts that status in the corner of the desktop, with one card
per conversation and a pet underneath.

![Pipsqueak showing working, waiting and finished project cards](assets/demo.png)

This animation uses the native app with simulated sessions and a clean backdrop.
It contains no private chats.

## Install

Download a package from [Releases](https://github.com/tristanmuzzu/Pipsqueak-Agent-Pets/releases/latest).

| Computer | Download |
| --- | --- |
| Windows 10/11 | `Pipsqueak_<version>_x64-setup.exe` |
| Ubuntu / Debian | `Pipsqueak_<version>_amd64.deb` |
| Fedora | `Pipsqueak-<version>-1.x86_64.rpm` |
| Other Linux desktops | `Pipsqueak_<version>_amd64.AppImage` |

For a `.deb`, run `sudo apt install ./Pipsqueak_<version>_amd64.deb`.
For an AppImage, make it executable and keep it in a permanent location before
launching; Claude hooks use that path. See [INSTALL.md](INSTALL.md) for
prerequisites, checks and removal.

**Codex connects automatically** through local session logs. For **Claude Code**,
the welcome panel offers to register hooks. Review that change, then restart
Claude Code. The welcome panel also offers login startup; it can be disabled.

GitHub source includes newer Codex question-reminder and resumed-chat fixes
than the 0.8.0 installers. Build from source if you need those changes.
macOS is untested and login startup is not implemented there.

## Send your agent here

> Set up https://github.com/tristanmuzzu/Pipsqueak-Agent-Pets on my computer.
> Read AGENTS.md and INSTALL.md. Prefer the latest public package unless I need
> an unreleased fix. Preserve my existing hooks and verify a real conversation
> reaches the pet. Tell me before enabling login startup.

## What it shows

- **Working:** the agent's own latest narration, plus elapsed time and actions.
- **Needs you:** a question or approval request. Short automatic approvals are
  delayed so they do not flash as urgent work.
- **Done or failed:** a result you can read without finding the terminal.
- **Several chats:** separate cards, including Claude and Codex in the same repo.

Click a working card for details. Click a finished card to dismiss it. The
arrow opens its chat where that integration is available. Pin a card to keep it
expanded, or filter by agent and project in the pet menu.

![Five projects with the waiting conversation expanded](assets/stack.png)

On Windows, Ctrl+Alt+P toggles the overlay. On Linux, use the tray or bind
`pipsqueak control toggle` in your desktop's keyboard settings. Linux cannot
track your pointer globally, so the pet's cursor-following eyes are Windows-only.

## Pets

Byte, Pip and Ember are included. Click a pet and it hops.

![Byte's idle, working, waiting, failed and reviewing animations](assets/states-byte.png)

A custom pet is a folder with a spritesheet and `pet.json`. Codex pet atlases
are supported too. [Custom pet guide](docs/guides/custom-pets.md).

## Privacy

Session tracking stays on your computer. No account or telemetry. The app reads
local agent transcripts to show narration; those lines can be visible to anyone
looking at your screen. The optional GitHub update check is off by default and
does not download or install anything.

Claude hooks write local session files and never print permission decisions.
Codex tracking needs no hook registration. Cloud sessions are not tracked.
[Integration details](docs/guides/codex.md).

## Build and contribute

Use Node 22.12+ and Rust stable, plus the
[platform build dependencies](docs/project/building.md).

```bash
git clone https://github.com/tristanmuzzu/Pipsqueak-Agent-Pets.git
cd Pipsqueak-Agent-Pets
npm ci
npm run build
npm test
cargo test --manifest-path src-tauri/Cargo.toml
npm run app:build
```

Sprites are committed, so you do not need to regenerate them to build.
`npm run dev` previews the frontend with simulated sessions. It does not test
native transparency, click-through behavior or real agent tracking.

[Contributing](CONTRIBUTING.md) · [Configuration](docs/guides/configuration.md) ·
[How it works](docs/guides/how-it-works.md) · [Known issues](FINDINGS.md)

MIT. Not affiliated with Anthropic or OpenAI. Bug reports with your platform,
agent version and what the card showed are welcome.
