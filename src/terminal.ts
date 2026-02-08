// terminal.ts  —  JS/TS front-end (final fixed version)

import { dlopen, FFIType, ptr } from "bun:ffi";
import { Buffer } from "node:buffer";
import { EventEmitter } from "./interfaces";
import type { IPty, IPtyForkOptions, IExitEvent } from "./interfaces";
import { join, dirname, basename } from "node:path";
import { existsSync } from "node:fs";
import { loadLibrary } from "./lib-loader";

export const DEFAULT_COLS = 80;
export const DEFAULT_ROWS = 24;
export const DEFAULT_FILE = "sh";
export const DEFAULT_NAME = "xterm";

/**
 * Quote a string for shell-words compatible splitting on the Rust side.
 * We are not invoking a shell; quoting is only to preserve token boundaries
 * when Rust parses the command line with shell_words::split.
 * 
 * @param s - The string to quote
 * @returns The quoted string
 */
function shQuote(s: string): string {
	if (s.length === 0) return "''";
	// Replace ' with '\'' (close-quote, escaped ', reopen)
	return `'${s.replace(/'/g, `'\\''`)}'`;
}

function debug(...args: any[]) {
	if (!process.env.BUN_PTY_DEBUG) return;
	// Uncomment for verbose logging of the terminal module
	console.log('[js]', ...args);
}

// terminal.ts  – loader fragment only

const symbols = loadLibrary() as any;

export class Terminal implements IPty {
	private handle = -1;
	private _pid = -1;
	private _cols = DEFAULT_COLS;
	private _rows = DEFAULT_ROWS;
	private readonly _name = DEFAULT_NAME;

	private _closing = false;
	private _worker: Worker | null = null;

	private readonly _onData = new EventEmitter<string>();
	private readonly _onExit = new EventEmitter<IExitEvent>();

	constructor(
		file = DEFAULT_FILE,
		args: string[] = [],
		opts: IPtyForkOptions = { name: DEFAULT_NAME },
	) {
		this._cols = opts.cols ?? DEFAULT_COLS;
		this._rows = opts.rows ?? DEFAULT_ROWS;
		const cwd = opts.cwd ?? process.cwd();
		// Properly quote file and arguments to preserve spaces and special characters
		const cmdline = [shQuote(file), ...args.map(shQuote)].join(" ");

		// Format environment variables as null-terminated string
		let envStr = "";
		if (opts.env) {
			const envPairs = Object.entries(opts.env).map(([k, v]) => `${k}=${v}`);
			envStr = envPairs.join("\0") + "\0";
		}

		this.handle = symbols.bun_pty_spawn(
			Buffer.from(`${cmdline}\0`, "utf8"),
			Buffer.from(`${cwd}\0`, "utf8"),
			Buffer.from(`${envStr}\0`, "utf8"),
			this._cols,
			this._rows,
		);
		if (this.handle < 0) throw new Error("PTY spawn failed");

		this._pid = symbols.bun_pty_get_pid(this.handle);

		// Spawn worker for polling
		this._worker = new Worker(new URL('./pty-worker.ts', import.meta.url));
		this._worker.onmessage = (e) => {
			const msg = e.data;
			if (msg.type === 'data') {
				this._onData.fire(msg.data);
			} else if (msg.type === 'exit') {
				this._onExit.fire({ exitCode: msg.exitCode });
				this.dispose();
			}
		};
  		this._worker.postMessage({ type: 'init', handle: this.handle });
	}

	/* ------------- accessors ------------- */

	get pid() {
		return this._pid;
	}
	get cols() {
		return this._cols;
	}
	get rows() {
		return this._rows;
	}
	get process() {
		return "shell";
	}

	get onData() {
		return this._onData.event;
	}
	get onExit() {
		return this._onExit.event;
	}

	/* ------------- IO methods ------------- */

	write(data: string) {
		if (this._closing) return;
		if (this._worker) {
			this._worker.postMessage({ type: 'write', data });
		}
	}

	resize(cols: number, rows: number) {
		if (this._closing) return;
		this._cols = cols;
		this._rows = rows;
		if (this._worker) {
			this._worker.postMessage({ type: 'resize', cols, rows });
		}
	}

	kill(signal = "SIGTERM") {
		if (this._closing) return;
		this._closing = true;
		if (this._worker) {
			this._worker.postMessage({ type: 'kill' });
			this._worker.terminate();
			this._worker = null;
		}
		this._onExit.fire({ exitCode: 0, signal });
		this.dispose();
	}

	dispose() {
		if (this.handle >= 0) {
			symbols.bun_pty_close(this.handle);
			this.handle = -1;
		}
	}
}
