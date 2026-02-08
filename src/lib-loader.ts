// lib-loader.ts - Shared library loading and path resolution for bun-pty

import { dlopen, FFIType } from "bun:ffi";
import { join, dirname, basename } from "node:path";
import { existsSync } from "node:fs";

export function resolveLibPath(): string {
	const env = process.env.BUN_PTY_LIB;
	if (env && existsSync(env)) return env;

	// For bun compile: use statically analyzable require with inline ternary.
	// Bun evaluates process.platform and process.arch at compile time and only
	// bundles the file for the target platform. The ternary MUST be inline
	// in the template literal for Bun's static analysis to work.
	// See: https://github.com/sursaone/bun-pty/issues/19
	try {
		// @ts-ignore - require returns path for binary files in Bun
		const embeddedPath = require(`../rust-pty/target/release/${process.platform === "win32" ? "rust_pty.dll" : process.platform === "darwin" ? (process.arch === "arm64" ? "librust_pty_arm64.dylib" : "librust_pty.dylib") : process.arch === "arm64" ? "librust_pty_arm64.so" : "librust_pty.so"}`);
		if (embeddedPath) return embeddedPath;
	} catch {
		// Not running as compiled binary, fall through to dynamic resolution
	}

	// Fallback: dynamic resolution for development scenarios
	const platform = process.platform;
	const arch = process.arch;

	// Try both architecture-specific and generic filenames
	const filenames =
		platform === "darwin"
			? arch === "arm64"
				? ["librust_pty_arm64.dylib", "librust_pty.dylib"]
				: ["librust_pty.dylib"]
			: platform === "win32"
			? ["rust_pty.dll"]
			: arch === "arm64"
			? ["librust_pty_arm64.so", "librust_pty.so"]
			: ["librust_pty.so"];

	// Start from the current module's location
	const base = Bun.fileURLToPath(import.meta.url);
	const fileDir = dirname(base);
	const dirName = basename(fileDir);

	// Handle both development (src/terminal.ts) and production (dist/terminal.js) cases
	// If we're in src/ or dist/, go up one level to get the project root
	const here = (dirName === "src" || dirName === "dist")
		? dirname(fileDir) // Go up one level from src/ or dist/
		: fileDir; // Otherwise use the directory as-is

	const basePaths = [
		join(here, "rust-pty", "target", "release"),       // Direct path from project root
		join(here, "..", "bun-pty", "rust-pty", "target", "release"), // monorepo setups
		join(process.cwd(), "node_modules", "bun-pty", "rust-pty", "target", "release"),
	];

	const fallbackPaths = [];
	for (const basePath of basePaths) {
		for (const filename of filenames) {
			fallbackPaths.push(join(basePath, filename));
		}
	}

	for (const path of fallbackPaths) {
		if (existsSync(path)) return path;
	}

	throw new Error(
		`librust_pty shared library not found.\nChecked:\n  - BUN_PTY_LIB=${env ?? "<unset>"}\n  - ${fallbackPaths.join("\n  - ")}\n\nSet BUN_PTY_LIB or ensure one of these paths contains the file.`
	);
}

export const ffiDefinitions = {
	bun_pty_spawn: {
		args: [FFIType.cstring, FFIType.cstring, FFIType.cstring, FFIType.i32, FFIType.i32],
		returns: FFIType.i32,
	},
	bun_pty_write: {
		args: [FFIType.i32, FFIType.pointer, FFIType.i32],
		returns: FFIType.i32,
	},
	bun_pty_read: {
		args: [FFIType.i32, FFIType.pointer, FFIType.i32, FFIType.i32],
		returns: FFIType.i32,
	},
	bun_pty_resize: {
		args: [FFIType.i32, FFIType.i32, FFIType.i32],
		returns: FFIType.i32,
	},
	bun_pty_kill: { args: [FFIType.i32], returns: FFIType.i32 },
	bun_pty_get_pid: { args: [FFIType.i32], returns: FFIType.i32 },
	bun_pty_get_exit_code: { args: [FFIType.i32], returns: FFIType.i32 },
	bun_pty_close: { args: [FFIType.i32], returns: FFIType.void },
};

export function loadLibrary(): ReturnType<typeof dlopen>["symbols"] {
	const libPath = resolveLibPath();
	try {
		const lib = dlopen(libPath, ffiDefinitions);
		return lib.symbols;
	} catch (error) {
		console.error("Failed to load PTY library:", error);
		throw error;
	}
}