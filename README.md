### Nix Development Environment

For a reproducible development environment with cross-compilation support:

```bash
# Enter the default development shell
nix develop

# Or for Windows cross-compilation (requires zigbuild)
nix develop .#crossWindows
cargo zigbuild --release --target x86_64-pc-windows-gnu
```