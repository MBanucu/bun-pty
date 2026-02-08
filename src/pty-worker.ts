// pty-worker.ts - Worker for reading PTY output with adaptive polling to minimize CPU usage

import { dlopen, FFIType, ptr } from "bun:ffi";
import { Buffer } from "node:buffer";
import { join, dirname, basename } from "node:path";
import { existsSync } from "node:fs";
import { loadLibrary } from "./lib-loader";

const symbols = loadLibrary() as any;

interface InitMessage {
	type: 'init';
	handle: number;
	pollInterval: number;  // Used as minInterval
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
let minInterval = 1;  // ms, for active/low-latency polling
let maxInterval = 50;  // ms, for idle/high-efficiency
let backoffFactor = 2;  // Multiplier for exponential backoff
let idleThreshold = 100;  // Loops without data before starting backoff
let currentInterval = minInterval;
let idleLoops = 0;
let running = false;
const decoder = new TextDecoder("utf-8");

async function startReadLoop() {
	if (running) return;
	running = true;

	const buf = Buffer.allocUnsafe(4096);

	while (running) {
		const n = symbols.bun_pty_read(handle, ptr(buf), buf.length, 0);  // Non-blocking read

		if (n > 0) {
			// Data received: post and reset polling to fast mode
			const decoded = decoder.decode(buf.subarray(0, n), { stream: true });
			if (decoded) {
				postMessage({ type: 'data', data: decoded });
			}
			idleLoops = 0;
			currentInterval = minInterval;
		} else if (n === -2) {
			// Child exited: wait for valid exit code if needed
			let exitCode = symbols.bun_pty_get_exit_code(handle);
			while (exitCode === -1 && running) {
				await new Promise(r => setTimeout(r, minInterval));
				exitCode = symbols.bun_pty_get_exit_code(handle);
			}
			if (running) {
				const remaining = decoder.decode();  // Flush decoder
				if (remaining) {
					postMessage({ type: 'data', data: remaining });
				}
				postMessage({ type: 'exit', exitCode });
			}
			break;
		} else if (n < 0) {
			// Error: flush and exit
			const remaining = decoder.decode();
			if (remaining) {
				postMessage({ type: 'data', data: remaining });
			}
			break;
		}

		// Check for exit every loop, in case process exited without EOF
		const currentExitCode = symbols.bun_pty_get_exit_code(handle);
		if (currentExitCode !== -1) {
			const remaining = decoder.decode();
			if (remaining) {
				postMessage({ type: 'data', data: remaining });
			}
			postMessage({ type: 'exit', exitCode: currentExitCode });
			break;
		}

		// No data: increment idle and back off if threshold met
		idleLoops++;
		if (idleLoops > idleThreshold) {
			currentInterval = Math.min(currentInterval * backoffFactor, maxInterval);
		}

		// Yield with current adaptive interval
		await new Promise(r => setTimeout(r, currentInterval));
	}

	// Final cleanup on exit
	running = false;
}

onmessage = (e: MessageEvent<Message>) => {
	const msg = e.data;
	switch (msg.type) {
		case 'init':
			handle = msg.handle;
			minInterval = msg.pollInterval || 1;  // Allow override as min
			currentInterval = minInterval;
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