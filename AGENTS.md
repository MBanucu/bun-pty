# Agent Commands

## Lint and Typecheck

- Rust check (Windows target): `nix develop ./windows --command -- sh -c "cd rust-pty && cargo check --target x86_64-pc-windows-gnu"`
- TypeScript typecheck: `bunx tsc --noEmit`