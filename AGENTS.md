# Development entry point

Use README.md for the product and `docs/project/building.md` for build, test and
release details. Inspect package.json and the lockfile before choosing commands.
`src/derive.js` owns testable display/state decisions; preserve its explicit time
inputs. `src-tauri/` owns the native desktop application. Provider compatibility
is product functionality, not an instruction to start an external coding agent.

For display logic or manifest changes, run `npm test`. Frontend work uses
`npm run dev` or `npm run build`; browser demos do not prove native transparency,
tray, hit testing or actual session integration. Native changes may need
`cargo test --manifest-path src-tauri/Cargo.toml` and scoped desktop verification.
`npm run check:sprites` regenerates and restores atlases: use only for sprite work.
Do not install hooks, rebuild native bundles, alter the user's desktop or publish
a release merely to make a small source edit. Release work follows the existing
checklist and current authorization; `verify:release` alone is not live QA.
