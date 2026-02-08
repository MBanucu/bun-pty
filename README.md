# bun-pty

[![NPM Version](https://img.shields.io/npm/v/bun-pty.svg)](https://www.npmjs.com/package/bun-pty)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![Bun Compatible](https://img.shields.io/badge/Bun-%E2%89%A51.0.0-black)](https://bun.sh)

A cross-platform pseudo-terminal (PTY) implementation for Bun, powered by Rust's portable-pty library and Bun's FFI capabilities.

## 🚀 Features

- **Cross-platform** - Works on macOS, Linux, and Windows
- **Simple API** - Clean Promise-based API similar to node-pty
- **Type-safe** - Complete TypeScript definitions included
- **Efficient** - Rust backend with proper error handling and multithreading
- **Event-Driven Architecture** - Zero idle CPU usage through control pipe mechanism
- **Optimized for Concurrency** - Worker-thread polling minimizes main-thread CPU usage for multiple PTYs
- **Unified I/O Abstraction** - Refactored cross-platform I/O helpers for maintainability
- **Adaptive Performance** - Dynamic buffer sizing and latency optimizations
- **Zero dependencies** - No external JavaScript dependencies required
- **Modern** - Built specifically for Bun using its FFI capabilities

## 📦 Installation

```bash
bun add bun-pty
```

## ⚙️ Requirements

- **Bun** 1.0.0 or higher
- **Rust** is only needed if you're building from source (the npm package includes pre-built binaries)

## 📋 Platform Support

| Platform | Status | Notes |
|----------|--------|-------|
| macOS    | ✅     | Fully supported |
| Linux    | ✅     | Fully supported |
| Windows  | ✅     | Fully supported |

## 🚦 Usage

### Basic Example

```typescript
import { spawn } from "bun-pty";

// Create a new terminal
const terminal = spawn("bash", [], {
  name: "xterm-256color",
  cols: 80,
  rows: 24
});

// Handle data from the terminal
terminal.onData((data) => {
  console.log("Received:", data);
});

// Handle terminal exit
terminal.onExit(({ exitCode, signal }) => {
  console.log(`Process exited with code ${exitCode} and signal ${signal}`);
});

// Write to the terminal
terminal.write("echo Hello from Bun PTY\n");

// Resize the terminal
terminal.resize(100, 40);

// Kill the process when done
setTimeout(() => {
  terminal.kill();
}, 5000);
```

### TypeScript Usage

The library includes complete TypeScript definitions. Here's how to use it with full type safety:

```typescript
import { spawn } from "bun-pty";
import type { IPty, IExitEvent, IPtyForkOptions } from "bun-pty";

// Create typed options
const options: IPtyForkOptions = {
  name: "xterm-256color",
  cols: 100,
  rows: 30,
  cwd: process.cwd()
};

// Create a terminal with proper typing
const terminal: IPty = spawn("bash", [], options);

// Typed event handlers
const dataHandler = terminal.onData((data: string) => {
  process.stdout.write(data);
});

const exitHandler = terminal.onExit((event: IExitEvent) => {
  console.log(`Process exited with code: ${event.exitCode}`);
});

// Clean up when done
dataHandler.dispose();
exitHandler.dispose();
```

### Interactive Shell Example

```typescript
import { spawn } from "bun-pty";
import { createInterface } from "node:readline";

// Create a PTY running bash
const pty = spawn("bash", [], {
  name: "xterm-256color",
  cwd: process.cwd()
});

// Forward PTY output to stdout
pty.onData((data) => {
  process.stdout.write(data);
});

// Send user input to the PTY
process.stdin.on("data", (data) => {
  pty.write(data.toString());
});

// Handle PTY exit
pty.onExit(() => {
  console.log("Terminal session ended");
  process.exit(0);
});

// Handle SIGINT (Ctrl+C)
process.on("SIGINT", () => {
  pty.kill();
});
```

## 📖 API Reference

### `spawn(file: string, args: string[], options: IPtyForkOptions): IPty`

Creates and spawns a new pseudoterminal.

- `file`: The executable to launch
- `args`: Arguments to pass to the executable
- `options`: Configuration options
  - `name`: Terminal name (e.g., "xterm-256color")
  - `cols`: Number of columns (default: 80)
  - `rows`: Number of rows (default: 24)
  - `cwd`: Working directory (default: process.cwd())
  - `env`: Environment variables
  - `pollInterval`: **Deprecated** - Polling interval was used in older versions; the event-driven architecture now uses zero idle CPU. This option is accepted for backward compatibility but has no effect.

Returns an `IPty` instance.

### `IPty` Interface

```typescript
interface IPty {
  // Properties
  readonly pid: number;        // Process ID
  readonly cols: number;       // Current columns
  readonly rows: number;       // Current rows
  readonly process: string;    // Process name
  
  // Events
  onData: (listener: (data: string) => void) => IDisposable;
  onExit: (listener: (event: IExitEvent) => void) => IDisposable;
  
  // Methods
  write(data: string): void;   // Write data to terminal
  resize(cols: number, rows: number): void;  // Resize terminal
  kill(signal?: string): void;  // Kill the process
}
```

## 🏗️ Architecture Details

### Code Organization

The Rust backend is organized into platform-specific modules for optimal maintainability:

- **`rust-pty/src/platform/`**: Platform abstraction layer
  - **`io_helpers.rs`**: Unified I/O helpers with cross-platform error handling and traits
   - **`linux/`**: Modular Linux implementation split into:
     - `helpers.rs`: Platform-specific I/O utilities
     - `pty_impl.rs`: Core PtyImpl struct and trait implementations  
     - `threads.rs`: Thread spawning and concurrency logic
     - `mod.rs`: Module exports
   - **`macos.rs`**: macOS-specific PTY implementation
   - **`windows/`**: Modular Windows implementation split into:
     - `helpers.rs`: Platform-specific I/O utilities and error handling
     - `pty_impl.rs`: Core PtyImpl struct and trait implementations
     - `threads.rs`: Thread spawning and concurrency logic
     - `mod.rs`: Module exports
   - **`mod.rs`**: Platform dispatch logic

This modular structure enables easier maintenance, testing, and platform-specific optimizations while keeping the public API unchanged. The recent refactoring introduced unified I/O abstractions that eliminate code duplication and provide consistent error handling across platforms.

### Event-Driven PTY Implementation

bun-pty implements an advanced **Option A** architecture that provides true event-driven PTY operations:

#### Core Components

- **Control Pipe**: Unix pipe (Linux/macOS) or event handle (Windows) for instant thread signaling
- **Event Types**: Enhanced message passing with `Data`, `Write`, `Resize`, `Kill`, and `Exit` messages
- **Blocking FFI**: `bun_pty_wait()` function that blocks until data or control events arrive
- **Poll-Based Read Thread**: Uses OS-level `poll()`/`select()` on both PTY file descriptor and control pipe

#### Platform-Specific Optimizations

- **Linux**: Uses `poll()` for event-driven I/O, with termination signaled exclusively by PTY EOF (eliminating race conditions that could cause data loss in quick-exiting processes). Unified I/O helpers ensure consistent error handling.
- **macOS**: Blocking reads with EOF detection for simple, reliable termination
- **Windows**: Event-driven with `WaitForMultipleObjects` on PTY and control handles, non-blocking pipe reads with full data draining to prevent loss in high-throughput scenarios. Optimized with PeekNamedPipe for reduced latency and adaptive buffer sizing.

#### Thread Architecture

```
Main Thread ──▶ Worker Thread ──▶ Rust FFI ──▶ Read Thread
     │                │                │             │
     │                │                │             ▼
     │                │                │      poll(pty_fd, control_fd)
     │                │                │             │
     │                │                │             ▼
     │                │                │      Data/Control Event
     │                │                │             │
     │                │                │             ▼
     │                │                │      Return to Worker
     │                │                │             │
     │                │                │             ▼
     │                │                │      Fire JavaScript Events
     │                │                │             │
     ▼                ▼                ▼             ▼
User Code ◀───── Event Callbacks ◀─────── bun_pty_wait() ◀─── Msg
```

#### Why Option A?

Traditional PTY implementations use polling loops that consume CPU even when idle. Option A eliminates this by:

1. **Blocking on Events**: Read thread blocks until actual PTY data arrives
2. **Instant Control Response**: Control operations (write/resize/kill) wake the thread immediately
3. **OS-Level Efficiency**: Uses `poll()`/`select()` for true event-driven behavior
4. **Zero Idle CPU**: No busy-waiting or timer-based polling when PTY is inactive

This makes bun-pty ideal for applications that maintain many PTYs simultaneously, such as:
- Terminal multiplexers
- IDE integrated terminals
- SSH connection pools
- Long-running background processes

### Backward Compatibility

The event-driven architecture is fully backward compatible. Existing code using `pollInterval` will continue to work unchanged, though the option is now ignored since polling is no longer used.

bun-pty uses [Bun's built-in test runner](https://bun.com/docs/test) for fast, Jest-compatible testing.

```bash
# Run all tests
bun test

# Run unit tests only
bun run test:unit

# Run integration tests (requires Rust library)
bun run test:integration

# View test coverage
bun run test:coverage
```

## 🔧 Building from Source

If you want to build the package from source:

```bash
# Clone the repository
git clone https://github.com/sursaone/bun-pty.git
cd bun-pty

# Install dependencies
bun install

# Build Rust library and TypeScript
bun run build

# Run tests
bun test
```
### Nix Development Environment

For a reproducible development environment with cross-compilation support:

```bash
# Enter the default development shell
nix develop

# For Windows cross-compilation with Zig linker
nix develop ./windows

# Build the Windows binary inside the shell
cd ../rust-pty
cargo zigbuild --release --target x86_64-pc-windows-gnu

# The output will be in rust-pty/target/x86_64-pc-windows-gnu/release/rust_pty.dll
```

Alternatively, you can build directly without entering the shell:

```bash
nix develop ./windows --command -- sh -c "cd ../rust-pty && cargo zigbuild --release --target x86_64-pc-windows-gnu"
```

If Zig linking fails, the flake falls back to GCC-based cross-compilation. Ensure all dependencies and features are correctly configured in `rust-pty/Cargo.toml`.

## ❓ Troubleshooting

### Prebuilt Binaries

The npm package includes prebuilt binaries for macOS, Linux, and Windows. If you encounter issues with the prebuilt binaries, you can build from source:

```bash
# In your project directory
bun add bun-pty
cd node_modules/bun-pty
bun run build
```

### Common Issues

- **Error: Unable to load shared library**: Make sure you have the necessary system libraries installed.
- **Process spawn fails**: Check if you have the required permissions and paths.

## 📚 Documentation

- [CHANGELOG.md](./CHANGELOG.md) - Version history and changes

## 📄 License

This project is licensed under the [MIT License](LICENSE).

## 🙏 Credits

- Built specifically for [Bun](https://bun.sh/)
- Uses [portable-pty](https://github.com/wez/wezterm/tree/main/pty) from WezTerm for cross-platform PTY support
- Inspired by [node-pty](https://github.com/microsoft/node-pty) for the API design
