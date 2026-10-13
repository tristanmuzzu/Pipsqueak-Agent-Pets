# Contributing

Bug reports, pets, and pull requests are all welcome. It is a small project and
will stay one.

## Reporting something broken

Right-click the pet → **Check my setup** → **Copy report**, and paste that into
the issue. It contains the version, the platform, and which of the four checks
failed; it contains no paths, prompts, or file contents.

A completion that had not happened, work that had stopped, a card stuck on
"needs you": if the pet is showing something untrue, that is the most valuable
kind of report this project can get. Worth more detail than usual, too. What you
were running, and what the card said versus what was actually going on.

## Building

```bash
npm ci
npm run app:dev
```

`npm run dev` serves the frontend alone in a browser with a demo loop standing
in for real sessions, which is the fastest way to work on anything visual.
`?panel=welcome` and `?panel=doctor` render those panels directly.

Full detail in [docs/project/building.md](docs/project/building.md).

## Before opening a pull request

```bash
npm test
cargo test --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path src-tauri/Cargo.toml
```

CI runs the same thing, plus a check that the sprite atlases still match their
generators.

## What the code is trying to be

A few things that are load-bearing rather than stylistic, so a change that
reverses one is probably a bug:

- **The hooks never write to stdout.** Anything printed on `PermissionRequest`
  becomes a permission decision. Nothing this app does may change what Claude
  Code decides.
- **Nothing is claimed that has not been established.** If an event is
  ambiguous, the card keeps saying what it was saying. Silence is always better
  than a confident wrong answer.
- **The display logic lives in `src/derive.js`**, apart from rendering, and
  takes `now` as an argument. That is what makes the debounces and holds
  testable rather than something you have to sit and watch.
- **Nothing moves without meaning.** Motion in the corner of someone's eye is a
  claim that something happened.
- **No new dependencies without a reason that survives the question "what would
  it take to do this by hand?"** The Rust side talks to Windows through a
  handful of declared functions rather than a crate graph; the sprite pipeline
  encodes its own PNGs.

## Pets

A pet is a folder with a `pet.json` and a spritesheet. See
[docs/guides/custom-pets.md](docs/guides/custom-pets.md). If you make one you
are happy to share, open a PR adding it to `public/pets/`, ideally with a
generator alongside it so it can be regenerated and colour-tweaked.

Please don't submit pets built from someone else's logo, mascot, or branding.
What you run on your own machine is your business; what ships in this repo has
to be original.
