# Configuration and CLI

[← README](../../README.md)

Everything lives in `~/.pipsqueak/config.json` and is editable from the pet's
right-click menu. Editing the file by hand is fine. An out-of-range value is
pulled back to something usable and costs only itself, and a file that cannot be
parsed at all is moved to `config.json.bak` rather than overwritten.

| Key | Meaning |
| --- | --- |
| `version` | which build's idea of this file it is; managed for you |
| `pet` | id of the active pet (`byte`, `pip`, `ember`, or your own) |
| `scale` | pet size, 1 to 6 |
| `click_through` | make the pet itself non-interactive too |
| `show_bubble` | show the status cards; off keeps the pet alone, and is remembered |
| `show_scratch` | include sessions rooted in a temp directory |
| `alert_on_waiting` | play a sound when a project starts waiting on you |
| `flash_on_finish` | blink the tray icon when a project finishes |
| `quiet` | do not disturb: no sound, no blink, the pet dozes |
| `hotkey` | chord that shows and hides the pet, or `off` |
| `update_check` | ask GitHub about new releases (off by default) |
| `update_dismissed` | a version you have already been told about |
| `welcomed` | whether the first-run panel has been dismissed |
| `autostart_initialised` | whether the start-with-the-machine decision has been made; managed for you |
| `x` / `y` | window position, saved when you drag the pet |

A config written by a newer version of Pipsqueak is read but never written back
to, so running an older build cannot quietly delete settings it does not know
about.

## Keyboard shortcut

`Ctrl+Alt+P` shows and hides the pet from anywhere, so you never have to go
looking for the tray icon. **Windows only** — see [On Linux](#on-linux) below
for the equivalent.

If pressing it appears to do nothing, open **Check my setup**. The keyboard
shortcut row says which chord registered and how many times it has actually
been pressed. Registering a chord and receiving one are separate things: a
program with a low-level keyboard hook can swallow the keypress on its way
past, and from inside Pipsqueak that looks exactly like nobody pressing it.

The chord belongs to the running app, which makes **Hide** and **Quit** two
different things. Hiding puts the pet away and leaves the process alive, so the
chord and the tray icon still work. Quitting stops it, and nothing on the
keyboard can bring back a program that is not running: start it from the Start
menu again, or run `pipsqueak control on`, which starts it if it has to.

Windows gives a chord to whichever program asks for it first, so if something
else already owns `Ctrl+Alt+P`, Pipsqueak takes the next one it can get and the
right-click menu shows which. Set `hotkey` in `config.json` to pick your own, or
to `off` to register nothing at all. Any combination of `Ctrl`, `Alt`, `Shift`
and `Win` plus a letter, digit or function key works. A chord with no modifier
is refused, since it would take that key away from everything else on the
machine.

### On Linux

There is no global hotkey, and this is a limit rather than a missing feature.
Wayland has no way for an ordinary application to claim a chord the compositor
is not already routing to it — that is the point of the design — and the portal
meant to replace it is not carried by every desktop. `hotkey` in `config.json`
is ignored, and the log says so at startup rather than reporting a chord that
would never fire.

The desktop's own keyboard settings do the job properly. Bind a custom shortcut
to:

```bash
pipsqueak control toggle
```

which starts the overlay if it is not running, so it is the only command the
shortcut needs. On GNOME: Settings → Keyboard → View and Customise Shortcuts →
Custom Shortcuts.

## When it stops behaving

`~/.pipsqueak/log.txt` records what the overlay did: when it started and from
which path, which hotkey it registered, when the page had to be reloaded, and
when a session was retired. It is capped at a quarter of a megabyte and keeps
the newest half.

The failure worth knowing about is the quiet one. The window can be present,
visible, correctly positioned and painting nothing, which looks exactly like
the pet having vanished. Hiding and showing it does not help, because an empty
page is empty either way. The overlay now notices this itself: the frontend
reports in on every render, and 45 seconds of silence triggers a reload, logged.

The reload is a ping first: an occluded window has its timers throttled to
about one a minute by Windows, which is silence without being death, and
reloading a healthy page is itself a visible blank and repaint. **Reload the
overlay** in the tray menu forces one on demand.

## After a reboot

Pipsqueak registers itself to start with the machine on its first run, because
an overlay that is not running is indistinguishable from one that is broken —
and the shortcut that would bring it back belongs to the process that is not
there. On Windows that is a value under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`; on Linux it is
`~/.config/autostart/pipsqueak.desktop`, which every XDG-compliant desktop
reads at login. Either way **Check my setup** has a row for it, and:

```bash
pipsqueak autostart status   # also: on, off
```

Turning it off is remembered, so it stays off. If the program is installed to
a new location, the entry is corrected the next time it runs, since one
pointing at the old path is present, plausible, and starts nothing.

If the pet is running but Claude Code events never arrive, the program may have
been removed from disk while it was still running. Windows keeps a deleted
program running, and Unix keeps the inode alive for anything that has the file
open, so on both it looks healthy while every hook points at a path that no
longer exists. The overlay checks for this and says so, and **Check my setup**
reports it under "Hook program".

Running from an **AppImage** is the easy way to reach that state: moving or
deleting the file after the hooks are registered leaves Claude Code calling
something that is not there. The registered command is the `.AppImage` itself
rather than the temporary mount it runs from, so keeping it in one place is
enough; if it has already moved, run `pipsqueak install` from the new location
to point the hooks at it.

## CLI

```bash
pipsqueak                 # run the overlay
pipsqueak control on      # show it, starting it if needed (also: off, toggle, quit, status)
pipsqueak control byte    # switch pet
pipsqueak autostart on    # start with the machine (also: off, status)
pipsqueak sessions        # what the overlay would draw right now, as JSON
pipsqueak doctor          # the same report as "Check my setup", in a terminal
pipsqueak install         # register the Claude Code hooks
pipsqueak uninstall       # remove them (backs up settings.json first)
pipsqueak hook <Event>    # internal: consume one hook payload from stdin
```

`sessions` answers "why is the card saying that". A card is a session file
joined to two things that are not in it — the chat the desktop app knows
about, and what the transcript says is still running — so both were previously
only visible by looking at the pet.

If the hook payloads themselves are in question, `mkdir ~/.pipsqueak/payloads`
and every hook writes its raw payload there until the directory is removed.
They contain prompts and assistant text, which is why it is a directory you
have to make rather than a setting you might leave on.

On Windows the binary is a GUI-subsystem app, so CLI output is also written to
`~/.pipsqueak/last-cli-result.txt`.

## From inside Claude Code

There is a small plugin so you never have to open a terminal:

```bash
/plugin marketplace add tristanmuzzu/Pipsqueak-Agent-Pets
```

```bash
/plugin install pipsqueak@pipsqueak
```

| Command | Effect |
| --- | --- |
| `/pipsqueak:pet` | toggle the pet |
| `/pipsqueak:pet on` | show it, starting Pipsqueak if it isn't running |
| `/pipsqueak:pet off` | hide it |
| `/pipsqueak:pet byte` | switch pet |
| `/pipsqueak:pet status` | is it running? |
| `/pipsqueak:pet quit` | close it entirely |

The command runs while the skill expands, so it takes effect immediately
without spending a turn and without a console window appearing. Under the hood
it is `pipsqueak control <action>`, which writes a one-line instruction to
`~/.pipsqueak/command.json` for the running overlay to pick up, and starts the
overlay first if it isn't up.

## Uninstalling

1. Right-click the pet → **Quit**.
2. Run `pipsqueak uninstall`, which removes the hooks and backs up
   `settings.json` first. On Windows the uninstaller does this for you; a `.deb`
   or `.rpm` removal and a deleted AppImage do not, so run it before the program
   is gone.
3. Remove the app: **Add or remove programs** on Windows,
   `sudo apt remove pipsqueak` or `sudo dnf remove Pipsqueak` for the packages,
   or delete the `.AppImage`.
4. Delete `~/.pipsqueak` if you want the config gone too, and
   `~/.config/autostart/pipsqueak.desktop` if `pipsqueak autostart off` was
   never run.
