# Contributing to ZDM

Thanks for helping out. ZDM is a small project — Tauri v2 + Rust backend, Vite + TypeScript frontend — and contributions stay close to how the code already works.

## Setup

Follow [Building from source](README.md#building-from-source): install the Tauri system dependencies, then `npm install`. You need Node.js 18+ and a recent stable Rust toolchain.

## Workflow

1. Fork the repository and create a branch from `main` for your change.
2. Make the change, keeping it focused — one feature or fix per pull request.
3. Run the checks below.
4. Open a pull request against `main` with a short description of what changed, why, and how you tested it. If it fixes an issue, link the issue.

## Checks

```bash
npm run build            # TypeScript (strict) + Vite build; must pass
cd src-tauri
cargo test               # 127 tests: 99 unit + 28 integration; must pass
npm run tauri build      # full release build, if you touched the backend or packaging
```

If you changed download behaviour, add or extend a test in `src-tauri/tests/download_integration.rs` — it runs a local Range server, so no network is needed. Pure logic (segment splitting, version parsing, state decisions) belongs in unit tests next to the code it covers.

## Style

- Match the existing style of the file you are editing. Rust follows the standard `rustfmt` layout; TypeScript is strict (`tsconfig.json`) and the frontend keeps one icon set in `src/icons.ts`.
- Keep the UI dark and minimal: the design system is locked in [`design.md`](design.md) and the tokens live in `src/tokens.css`. No gradients, no glow, no shadows on resting surfaces; colour only on data.
- UI labels are in English; notifications and some status and error messages are in Indonesian. Keep new strings consistent with the surrounding text.
- Never include secrets, credentials, or personal data in code, tests, or screenshots.

## Reporting bugs

Use the bug report template: ZDM version, how you installed it (AppImage / binary / built from source), Linux distribution, steps to reproduce, what you expected, and what happened instead. If a download misbehaved, include the URL only if it is public — never paste URLs carrying credentials or tokens.
