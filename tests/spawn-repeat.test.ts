import { describe, expect, it } from 'bun:test'
import type { Subprocess } from 'bun'
import { Terminal } from '../src/terminal'

describe('PTY Echo Behavior', () => {
    class TestSpawner {
        readonly subprocess: Subprocess<'ignore', 'pipe', 'pipe'>
        stderrOutput = ''
        stdoutOutput = ''
        readonly testNumber: number
        constructor(testNumber: number) {
            this.testNumber = testNumber
            this.subprocess = Bun.spawn({
                cmd: [
                    'bun',
                    'test',
                    'tests/spawn-repeat.test.ts',
                    '--test-name-pattern',
                    'should receive initial data once',
                ],
                stdout: 'pipe',
                stderr: 'pipe',
                env: { ...process.env, SYNC_TESTS: '1' },
            })

            const { stdout, stderr } = this.subprocess

                ; (async () => {
                    const decoder = new TextDecoder()
                    const reader = stderr.getReader()
                    try {
                        while (true) {
                            const { done, value } = await reader.read()
                            this.stderrOutput += decoder.decode(value, { stream: true })
                            if (done) break
                        }
                        this.stderrOutput += decoder.decode() // Flush any remaining buffered data
                    } finally {
                        reader.releaseLock()
                    }
                })()
                ; (async () => {
                    const decoder = new TextDecoder()
                    const reader = stdout.getReader()
                    try {
                        while (true) {
                            const { done, value } = await reader.read()
                            this.stdoutOutput += decoder.decode(value, { stream: true })
                            if (done) break
                        }
                        this.stdoutOutput += decoder.decode() // Flush any remaining buffered data
                    } finally {
                        reader.releaseLock()
                    }
                })()
        }
    }

    it('should receive initial data reproducibly', async () => {
        const start = Date.now()
        const maxRuntime = 1000
        let runnings = 1
        const spawned: TestSpawner[] = []
        while (Date.now() - start < maxRuntime) {
            runnings++
            const testSpawner = new TestSpawner(runnings)
            spawned.push(testSpawner)
        }
        let errorMessage = ''
        errorMessage += `[TEST] Spawned ${runnings} subprocesses in ${Date.now() - start}ms.\n`
        const timeout = new Promise<void>((resolve) => {
            setTimeout(() => resolve(), 20000)
        })
        const all = Promise.all(spawned.map((s) => s.subprocess.exited))
        await Promise.race([all, timeout])
        const stillRunning = spawned.filter((s) => s.subprocess.exitCode === null)
        if (stillRunning.length > 0) {
            errorMessage += `[TEST] Timeout reached after 20s with ${stillRunning.length} subprocesses still running.\n`
            await new Promise((resolve) => setTimeout(resolve, 1000))
            stillRunning.forEach((s) => {
                errorMessage += `[TEST] Subprocess ${s.testNumber} stderr: ${s.stderrOutput}\n`
                errorMessage += `[TEST] Subprocess ${s.testNumber} stdout: ${s.stdoutOutput}\n`
            })
        }
        const exitCodeNonZero = spawned.filter(
            (s) => s.subprocess.exitCode !== null && s.subprocess.exitCode !== 0
        )
        if (exitCodeNonZero.length > 0) {
            errorMessage += `[TEST] ${exitCodeNonZero.length} subprocesses exited with non-zero exit code.\n`
            await new Promise((resolve) => setTimeout(resolve, 1000))
            exitCodeNonZero.forEach((s) => {
                errorMessage += `[TEST] Subprocess ${s.testNumber} stderr: ${s.stderrOutput}\n`
                errorMessage += `[TEST] Subprocess ${s.testNumber} stdout: ${s.stdoutOutput}\n`
            })
        }
        expect(stillRunning.length + exitCodeNonZero.length, errorMessage).toBe(0)
    }, 60000)
    // Platform detection for cross-platform tests
    const isWindows = process.platform === "win32";
    const echoCommand = (text: string) =>
        isWindows
            ? { cmd: "cmd.exe", args: ["/c", `echo ${text}`] }
            : { cmd: "echo", args: [text] };
    it.skipIf(!process.env.SYNC_TESTS)(
        'should receive initial data once',
        async () => {
            let hasExited = false;

            const { cmd, args } = echoCommand("sync test");
            console.log("[TEST] Starting terminal with command:", cmd, args);
            const terminal = new Terminal(cmd, args);

            const exitPromise = new Promise<void>((resolve) => {
                terminal.onExit(() => {
                    console.log("[TEST] Process exited");
                    hasExited = true;
                    resolve();
                });
            });
            const dataPromise = new Promise<string>((resolve, reject) => {
                let dataReceived = "";
                terminal.onData((data) => {
                    console.log("[TEST] Received data:", data);
                    dataReceived += data;
                    if (dataReceived.includes("sync test"))
                        resolve(dataReceived);
                });
                const timeout = 5000;
                setTimeout(() => { reject(new Error("Timeout waiting for data")); }, timeout); // Timeout to avoid hanging test
            });

            const dataReceived = await Promise.race([exitPromise, dataPromise]);

            expect(dataReceived).toBeTypeOf("string");
            expect(dataReceived).toContain("sync test");
        },
        10000
    )

})
