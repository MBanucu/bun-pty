// pty-worker.ts - Worker for reading PTY output with event-driven blocking

import { dlopen, FFIType, ptr } from "bun:ffi";
import { Buffer } from "node:buffer";
import { join, dirname, basename } from "node:path";
import { existsSync } from "node:fs";
import { loadLibrary } from "./lib-loader";

const symbols = loadLibrary() as any;

interface InitMessage {
	type: 'init';
	handle: number;
}

type Message = InitMessage;

let handle = -1;
let running = false;
const decoder = new TextDecoder("utf-8");

async function startReadLoop() {
	if (running) return;
	running = true;

	const buf = Buffer.allocUnsafe(4096);

	while (running) {
		const n = symbols.bun_pty_read(handle, ptr(buf), buf.length, 1); // blocking=1

		if (n > 0) { // DATA
			const decoded = decoder.decode(buf.subarray(0, n), { stream: true });
			if (decoded) {
				postMessage({ type: 'data', data: decoded });
			}
		} else if (n === -2) { // CHILD_EXITED
			const remaining = decoder.decode();  // Flush decoder
			if (remaining) {
				postMessage({ type: 'data', data: remaining });
			}
			const exitCode = symbols.bun_pty_get_exit_code(handle);
			postMessage({ type: 'exit', exitCode });
			break;
		} else if (n < 0) {
			// Other error, perhaps break
			break;
		}
		// n === 0 means no data, continue loop
	}

	// Final cleanup on exit
	running = false;
}

onmessage = (e: MessageEvent<Message>) => {
	const msg = e.data;
	switch (msg.type) {
		case 'init':
			handle = msg.handle;
			startReadLoop();
			break;
	}
};