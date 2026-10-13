# Local installation

For humans and agents setting up Pipsqueak. First check whether it is already
installed; reuse that setup and preserve its configuration.

## Choose a route

Use the latest public [release](https://github.com/tristanmuzzu/Pipsqueak-Agent-Pets/releases/latest)
for a normal installation. Source on `main` may contain fixes that are not in
the downloadable installers. Do not claim installing a release applies those.

Windows 10/11 uses the x64 setup executable (or MSI). Windows needs WebView2;
the installer handles it if missing.

Linux packages target x64. Ubuntu 22.04 / Debian 12 or newer are the documented
baseline. On Ubuntu/Debian:

```bash
sudo apt install ./Pipsqueak_<version>_amd64.deb
```

Fedora uses `sudo dnf install ./Pipsqueak-<version>-1.x86_64.rpm`.
For an AppImage:

```bash
chmod +x Pipsqueak_<version>_amd64.AppImage
./Pipsqueak_<version>_amd64.AppImage
```

Store an AppImage permanently before launching. It needs GTK 3, WebKitGTK 4.1
and AppIndicator; on Ubuntu/Debian, install `libwebkit2gtk-4.1-0` and
`libayatana-appindicator3-1` if missing. Follow your distribution's FUSE/AppImage
requirements if it cannot mount. Do not run the pet as root.

On GNOME/Wayland, Pipsqueak uses XWayland (`GDK_BACKEND=x11`) to avoid taking
keyboard focus. A native Wayland launch can grab focus or misplace the pet.
macOS is unverified.

## Connect the agent and check it

1. Launch one pet. Keep existing preferences and position.
2. Codex uses `$CODEX_HOME` (default `~/.codex`) automatically. No hooks needed.
   Confirm its local session logs exist; cloud-only chats are not supported.
3. For Claude Code, review the welcome panel's hook and login-startup changes.
   It backs up `~/.claude/settings.json` and preserves foreign hooks. Restart
   Claude Code after registering hooks.
4. Run a short real conversation. Check its card moves from working to done.
   Run another chat to confirm the cards stay separate. Use **Check my setup**
   in the menu if a provider stays disconnected.
5. Click a card and verify your typing still goes to the app you were using.
   Transparent space should let clicks through. These need a native desktop;
   a browser demo does not prove them.

Linux has no app-registered global hotkey. Use the tray or add your own desktop
shortcut for `pipsqueak control toggle`. Do not overwrite an existing binding.

## Build current source

Use Node **22.12+** and Rust stable. Vite 8 needs a recent Node version.
See [building](docs/project/building.md) for Linux libraries or Windows MSVC
requirements. Committed sprites are ready to use.

```bash
git clone https://github.com/tristanmuzzu/Pipsqueak-Agent-Pets.git
cd Pipsqueak-Agent-Pets
npm ci
npm run build
npm test
cargo test --manifest-path src-tauri/Cargo.toml
npm run app:build
```

Install the resulting package from `src-tauri/target/release/bundle/`, then
repeat the real-session checks. Do not install hooks, replace a running pet,
or enable login startup merely to preview frontend changes.

## Remove it

Before deleting an AppImage or removing a Linux package:

```bash
pipsqueak autostart off
pipsqueak uninstall
```

This removes Pipsqueak's Claude hooks, not other tools' hooks. Exit the pet via
its menu, then remove its package. The Windows uninstaller removes the hooks.
Keep local configuration unless the user asks to delete it.
