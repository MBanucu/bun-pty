// pty-worker.ts - Worker for polling PTY output to offload CPU usage from main thread

import { dlopen, FFIType, ptr } from "bun:ffi";
import { Buffer } from "node:buffer";
import { join, dirname, basename } from "node:path";
import { existsSync } from "node:fs";

function resolveLibPath(): string {
	const env = process.env.BUN_PTY_LIB;
	if (env && existsSync(env)) return env;

	const platform = process.platform;
	const arch = process.arch;

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

	const base = Bun.fileURLToPath(import.meta.url);
	const fileDir = dirname(base);
	const dirName = basename(fileDir);
	
	const here = (dirName === "src" || dirName === "dist")
		? dirname(fileDir)
		: fileDir;

	const basePaths = [
		join(here, "rust-pty", "target", "release"),
		join(here, "..", "bun-pty", "rust-pty", "target", "release"),
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

	throw new Error(`librust_pty shared library not found in worker.`);
}

const libPath = resolveLibPath();

let lib: any;
try {
	lib = dlopen(libPath, {
		bun_pty_write: { args: [FFIType.i32, FFIType.pointer, FFIType.i32], returns: FFIType.i32 },
		bun_pty_read: { args: [FFIType.i32, FFIType.pointer, FFIType.i32], returns: FFIType.i32 },
		bun_pty_resize: { args: [FFIType.i32, FFIType.i32, FFIType.i32], returns: FFIType.i32 },
		bun_pty_kill: { args: [FFIType.i32], returns: FFIType.i32 },
		bun_pty_get_pid: { args: [FFIType.i32], returns: FFIType.i32 },
		bun_pty_get_exit_code: { args: [FFIType.i32], returns: FFIType.i32 },
		bun_pty_close: { args: [FFIType.i32], returns: FFIType.void },
	});
} catch (error) {
	console.error("Failed to load lib in worker", error);
}

interface InitMessage {
	type: 'init';
	handle: number;
	pollInterval: number;
}

interface WriteMessage {
	type: 'write';
	data: string;
}

interface ResizeMessage {
	type: 'resize';
	cols: number;
	rows: number;
}

interface KillMessage {
	type: 'kill';
}

type Message = InitMessage | WriteMessage | ResizeMessage | KillMessage;

let handle = -1;
let pollInterval = 50;
let running = false;
const decoder = new TextDecoder("utf-8");

async function startReadLoop() {
	if (running) return;
	running = true;

	const buf = Buffer.allocUnsafe(4096);

	while (running) {
		const n = lib.symbols.bun_pty_read(handle, ptr(buf), buf.length);
		if (n > 0) {
			const decoded = decoder.decode(buf.subarray(0, n), { stream: true });
			if (decoded) {
				postMessage({ type: 'data', data: decoded });
			}
		}

		const currentExitCode = lib.symbols.bun_pty_get_exit_code(handle);
		if (currentExitCode !== -1) {
			const remaining = decoder.decode();
			if (remaining) {
				postMessage({ type: 'data', data: remaining });
			}
			postMessage({ type: 'exit', exitCode: currentExitCode });
			break;
		}

		if (n === -2) {
			let exitCode = lib.symbols.bun_pty_get_exit_code(handle);
			while (exitCode === -1) {
				await new Promise(r => setTimeout(r, 1));
				exitCode = lib.symbols.bun_pty_get_exit_code(handle);
			}
			const remaining = decoder.decode();
			if (remaining) {
				postMessage({ type: 'data', data: remaining });
			}
			postMessage({ type: 'exit', exitCode });
			break;
		} else if (n < 0) {
			const remaining = decoder.decode();
			if (remaining) {
				postMessage({ type: 'data', data: remaining });
			}
			break;
		} else {
			await new Promise(r => setTimeout(r, pollInterval));
		}
	}
}

onmessage = (e: MessageEvent<Message>) => {
	const msg = e.data;
	switch (msg.type) {
		case 'init':
			handle = msg.handle;
			pollInterval = msg.pollInterval;
			startReadLoop();
			break;
		case 'write':
			if (handle >= 0) {
				const buf = Buffer.from(msg.data, "utf8");
				lib.symbols.bun_pty_write(handle, ptr(buf), buf.length);
			}
			break;
		case 'resize':
			if (handle >= 0) {
				lib.symbols.bun_pty_resize(handle, msg.cols, msg.rows);
			}
			break;
		case 'kill':
			running = false;
			if (handle >= 0) {
				lib.symbols.bun_pty_kill(handle);
				lib.symbols.bun_pty_close(handle);
			}
			break;
	}
};