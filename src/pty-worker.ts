// pty-worker.ts - Worker for polling PTY output to offload CPU usage from main thread

import { dlopen, FFIType, ptr } from "bun:ffi";
import { Buffer } from "node:buffer";
import { join, dirname, basename } from "node:path";
import { existsSync } from "node:fs";
import { loadLibrary } from "./lib-loader";

const symbols = loadLibrary() as any;

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
let pollInterval = 1;
let running = false;
const decoder = new TextDecoder("utf-8");

async function startReadLoop() {
	if (running) return;
	running = true;

	const buf = Buffer.allocUnsafe(4096);

	while (running) {
		const n = symbols.bun_pty_read(handle, ptr(buf), buf.length, 0); // non-blocking
		if (n > 0) {
			const decoded = decoder.decode(buf.subarray(0, n), { stream: true });
			if (decoded) {
				postMessage({ type: 'data', data: decoded });
			}
		}

		const currentExitCode = symbols.bun_pty_get_exit_code(handle);
		if (currentExitCode !== -1) {
			const remaining = decoder.decode();
			if (remaining) {
				postMessage({ type: 'data', data: remaining });
			}
			postMessage({ type: 'exit', exitCode: currentExitCode });
			break;
		}

		if (n === -2) {
			let exitCode = symbols.bun_pty_get_exit_code(handle);
			while (exitCode === -1) {
				await new Promise(r => setTimeout(r, 1));
				exitCode = symbols.bun_pty_get_exit_code(handle);
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
				symbols.bun_pty_write(handle, ptr(buf), buf.length);
			}
			break;
		case 'resize':
			if (handle >= 0) {
				symbols.bun_pty_resize(handle, msg.cols, msg.rows);
			}
			break;
		case 'kill':
			running = false;
			if (handle >= 0) {
				symbols.bun_pty_kill(handle);
				symbols.bun_pty_close(handle);
			}
			break;
	}
};