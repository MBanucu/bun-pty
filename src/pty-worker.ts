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
	pollInterval: number;  // Kept for backward compatibility but not used
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
let running = false;
const decoder = new TextDecoder("utf-8");

async function startReadLoop() {
	if (running) return;
	running = true;

	const buf = Buffer.allocUnsafe(4096);
	const typeBuf = Buffer.alloc(4); // int32 for event type

	while (running) {
		const n = symbols.bun_pty_wait(handle, ptr(buf), buf.length, ptr(typeBuf));

		const eventType = typeBuf.readInt32LE(0);

		if (eventType === 0) { // DATA
			if (n > 0) {
				const decoded = decoder.decode(buf.subarray(0, n), { stream: true });
				if (decoded) {
					postMessage({ type: 'data', data: decoded });
				}
			}
		} else if (eventType === 1) { // EXIT
			const remaining = decoder.decode();  // Flush decoder
			if (remaining) {
				postMessage({ type: 'data', data: remaining });
			}
			postMessage({ type: 'exit', exitCode: n });
			break;
		} else if (eventType === 2) { // CONTROL_EVENT
			// Handle control events if needed
			// For now, just continue
		}
	}

	// Final cleanup on exit
	running = false;
}

onmessage = (e: MessageEvent<Message>) => {
	const msg = e.data;
	switch (msg.type) {
		case 'init':
			handle = msg.handle;
			// pollInterval kept for backward compatibility but not used in event-driven mode
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
			}
			break;
	}
};