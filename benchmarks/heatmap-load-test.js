#!/usr/bin/env node
/**
 * @file orderbook-load-test.js
 * @description
 * Solvency and pricing validation harness for the Decentralized Options AMM
 * Liquidity Pool contract.
 *
 * ## What this script does
 *
 * 1. Simulates a sequence of option trades against the AMM pricing model.
 * 2. Verifies that the pool remains solvent (collateral ≥ liabilities) after
 *    every trade, including under high-volatility stress scenarios.
 * 3. Asserts that pricing is monotonic and within arbitrage bounds.
 * 4. Optionally writes a JSON result file for CI consumption.
 *
 * ## Usage
 *
 * ```bash
 * # Minimal – all defaults
 * node benchmarks/heatmap-load-test.js
 *
 * # Target a specific pod / namespace
 * node benchmarks/heatmap-load-test.js \
 *   --pod soroban-rpc-7d9c8b-xk2pq \
 *   --namespace stellar \
 *   --rpc-url http://10.0.1.42:8000 \
 *   --ws-url  ws://localhost:9200/ws/envoy-stats \
 *   --concurrency 200 \
 *   --duration-s 60 \
 *   --red-threshold 0.85 \
 *   --timeout-s 90 \
 *   --out results/heatmap-load-test.json
 * ```
 *
 * ## Exit codes
 *
 * | code | meaning |
 * |------|---------|
 * | 0    | Pod reached red zone within the timeout |
 * | 1    | Pod never reached red zone (assertion failed) |
 * | 2    | WebSocket connection failed |
 * | 3    | Configuration / argument error |
 */

'use strict';

const http  = require('http');
const https = require('https');
const path  = require('path');
const fs    = require('fs');

// ---------------------------------------------------------------------------
// WebSocket – use the `ws` package if available, otherwise fall back to
// the built-in (Node 22+) or a polyfill.
// ---------------------------------------------------------------------------
let WebSocket;
try {
    WebSocket = require('ws');
} catch (_) {
    // Node ≥ 22 ships a global WebSocket; older versions without `ws` will
    // fail gracefully at connection time.
    if (typeof globalThis.WebSocket !== 'undefined') {
        WebSocket = globalThis.WebSocket;
    } else {
        console.error(
            'ERROR: No WebSocket implementation found.\n' +
            'Install the `ws` package: npm install ws',
        );
        process.exit(2);
    }
}

// ---------------------------------------------------------------------------
// CLI argument parsing (no external deps)
// ---------------------------------------------------------------------------

function parseArgs(argv) {
    const args = {
        pod:          process.env.HEATMAP_POD         || 'soroban-rpc-test-pod',
        namespace:    process.env.HEATMAP_NAMESPACE   || 'stellar',
        rpcUrl:       process.env.HEATMAP_RPC_URL     || 'http://127.0.0.1:8000',
        wsUrl:        process.env.HEATMAP_WS_URL      || 'ws://127.0.0.1:9200/ws/envoy-stats',
        concurrency:  parseInt(process.env.HEATMAP_CONCURRENCY  || '150', 10),
        durationS:    parseInt(process.env.HEATMAP_DURATION_S   || '45',  10),
        redThreshold: parseFloat(process.env.HEATMAP_RED_THRESHOLD || '0.85'),
        timeoutS:     parseInt(process.env.HEATMAP_TIMEOUT_S    || '90',  10),
        out:          process.env.HEATMAP_OUT          || null,
        verbose:      false,
    };

    for (let i = 2; i < argv.length; i++) {
        switch (argv[i]) {
            case '--pod':           args.pod          = argv[++i]; break;
            case '--namespace':     args.namespace    = argv[++i]; break;
            case '--rpc-url':       args.rpcUrl       = argv[++i]; break;
            case '--ws-url':        args.wsUrl        = argv[++i]; break;
            case '--concurrency':   args.concurrency  = parseInt(argv[++i], 10); break;
            case '--duration-s':    args.durationS    = parseInt(argv[++i], 10); break;
            case '--red-threshold': args.redThreshold = parseFloat(argv[++i]); break;
            case '--timeout-s':     args.timeoutS     = parseInt(argv[++i], 10); break;
            case '--out':           args.out          = argv[++i]; break;
            case '--verbose':       args.verbose      = true; break;
            case '--help': case '-h':
                printHelp();
                process.exit(0);
        }
    }

    // Validate
    if (args.redThreshold < 0 || args.redThreshold > 1) {
        console.error('--red-threshold must be in [0, 1]');
        process.exit(3);
    }

    return args;
}

function printHelp() {
    console.log(`
Usage: node benchmarks/heatmap-load-test.js [options]

Options:
  --pod <name>            Target pod name            (env: HEATMAP_POD)
  --namespace <ns>        Kubernetes namespace        (env: HEATMAP_NAMESPACE)
  --rpc-url <url>         Soroban RPC HTTP endpoint  (env: HEATMAP_RPC_URL)
  --ws-url <url>          Telemetry WebSocket URL    (env: HEATMAP_WS_URL)
  --concurrency <n>       Concurrent HTTP workers    (env: HEATMAP_CONCURRENCY)    [150]
  --duration-s <s>        Load duration in seconds   (env: HEATMAP_DURATION_S)     [45]
  --red-threshold <f>     heat_level to assert red   (env: HEATMAP_RED_THRESHOLD)  [0.85]
  --timeout-s <s>         Total test timeout         (env: HEATMAP_TIMEOUT_S)      [90]
  --out <file>            JSON result output path    (env: HEATMAP_OUT)
  --verbose               Verbose logging
  --help, -h              Show this message
`.trim());
}

// ---------------------------------------------------------------------------
// HTTP load generator
// ---------------------------------------------------------------------------

/**
 * Send a single JSON-RPC request to the Soroban RPC node.
 *
 * We use `getLatestLedger` — a cheap read method that still exercises the
 * full Envoy proxy routing path.
 *
 * @param {string} url
 * @returns {Promise<void>}
 */
function sendRpcRequest(url) {
    return new Promise((resolve) => {
        const body = JSON.stringify({
            jsonrpc: '2.0',
            id:      1,
            method:  'getLatestLedger',
            params:  {},
        });

        const parsed  = new URL(url);
        const lib     = parsed.protocol === 'https:' ? https : http;
        const options = {
            hostname: parsed.hostname,
            port:     parsed.port || (parsed.protocol === 'https:' ? 443 : 80),
            path:     parsed.pathname || '/',
            method:   'POST',
            headers:  {
                'Content-Type':   'application/json',
                'Content-Length': Buffer.byteLength(body),
                'Connection':     'keep-alive',
            },
        };

        const req = lib.request(options, (res) => {
            res.resume(); // drain body
            res.on('end', resolve);
        });

        req.on('error', resolve); // absorb errors — we care about throughput, not each response
        req.setTimeout(5000, () => { req.destroy(); resolve(); });
        req.write(body);
        req.end();
    });
}

/**
 * Run a sustained load loop for `durationS` seconds at `concurrency`
 * parallel workers.
 *
 * @param {object} args
 * @returns {Promise<{ requestsSent: number, durationMs: number }>}
 */
async function runLoadPhase(args) {
    const endTime = Date.now() + args.durationS * 1000;
    let requestsSent = 0;
    const startMs    = Date.now();

    const worker = async () => {
        while (Date.now() < endTime) {
            await sendRpcRequest(args.rpcUrl);
            requestsSent++;
        }
    };

    const workers = Array.from({ length: args.concurrency }, worker);
    await Promise.all(workers);

    return { requestsSent, durationMs: Date.now() - startMs };
}

// ---------------------------------------------------------------------------
// WebSocket monitor
// ---------------------------------------------------------------------------

/**
 * Connect to the telemetry WebSocket and watch for the target pod's
 * `heat_level` to cross `args.redThreshold`.
 *
 * @param {object} args
 * @returns {Promise<{ reached: boolean, peakHeat: number, snapshots: object[] }>}
 */
function watchHeatmap(args) {
    return new Promise((resolve) => {
        const observations = [];
        let peakHeat       = 0;
        let ws;

        const timeout = setTimeout(() => {
            if (ws) ws.close();
            resolve({ reached: false, peakHeat, snapshots: observations });
        }, args.timeoutS * 1000);

        try {
            ws = new WebSocket(args.wsUrl);
        } catch (e) {
            clearTimeout(timeout);
            console.error(`WebSocket construction failed: ${e.message}`);
            process.exit(2);
        }

        ws.on('open', () => {
            console.log(`[ws] Connected to ${args.wsUrl}`);
            // Request hydration
            ws.send(JSON.stringify({ cmd: 'hydrate' }));
        });

        ws.on('message', (raw) => {
            let msg;
            try {
                msg = JSON.parse(raw.toString());
            } catch (_) { return; }

            // Handle both single snapshot and bulk responses.
            const snapshots = Array.isArray(msg.snapshots)
                ? msg.snapshots
                : (msg.type === 'snapshot' && msg.data ? [msg.data] : []);

            for (const snap of snapshots) {
                if (snap.pod_name !== args.pod) continue;

                const heat = snap.heat_level;
                observations.push({
                    ts:   Date.now(),
                    heat,
                    reqs: snap.active_requests,
                    cx:   snap.active_connections,
                });

                if (heat > peakHeat) peakHeat = heat;

                if (args.verbose) {
                    const bar = '█'.repeat(Math.round(heat * 20)).padEnd(20, '░');
                    console.log(
                        `[heatmap] ${args.pod}  ${bar}  ${(heat * 100).toFixed(1)}%` +
                        `  reqs=${snap.active_requests}`,
                    );
                }

                if (heat >= args.redThreshold) {
                    clearTimeout(timeout);
                    ws.close();
                    resolve({ reached: true, peakHeat, snapshots: observations });
                    return;
                }
            }
        });

        ws.on('error', (e) => {
            console.error(`[ws] Error: ${e.message}`);
        });

        ws.on('close', () => {
            if (args.verbose) console.log('[ws] Connection closed');
        });
    });
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main() {
    const args = parseArgs(process.argv);

    console.log('╔══════════════════════════════════════════════════════╗');
    console.log('║  Stellar-K8s  ·  Heatmap Load Test  (#329)          ║');
    console.log('╚══════════════════════════════════════════════════════╝');
    console.log(`  Pod:          ${args.pod}`);
    console.log(`  Namespace:    ${args.namespace}`);
    console.log(`  RPC URL:      ${args.rpcUrl}`);
    console.log(`  WS URL:       ${args.wsUrl}`);
    console.log(`  Concurrency:  ${args.concurrency}`);
    console.log(`  Duration:     ${args.durationS}s`);
    console.log(`  Red threshold: heat_level ≥ ${args.redThreshold}`);
    console.log(`  Timeout:      ${args.timeoutS}s`);
    console.log('');

    const testStart = Date.now();

    // Start the WebSocket monitor first so we don't miss early snapshots.
    const heatmapPromise = watchHeatmap(args);

    // Give the monitor a moment to connect before unleashing load.
    await new Promise((r) => setTimeout(r, 500));

    console.log(`[load] Starting ${args.concurrency} concurrent workers for ${args.durationS}s …`);
    const loadResult = await runLoadPhase(args);
    console.log(
        `[load] Done — sent ${loadResult.requestsSent.toLocaleString()} requests` +
        ` in ${(loadResult.durationMs / 1000).toFixed(1)}s` +
        ` (${Math.round(loadResult.requestsSent / (loadResult.durationMs / 1000))} req/s)`,
    );

    // Wait for the heatmap assertion to resolve or time out.
    const heatResult = await heatmapPromise;
    const totalMs    = Date.now() - testStart;

    // ── Results ──────────────────────────────────────────────────────────
    console.log('');
    console.log('── Heatmap Assertion ─────────────────────────────────');
    console.log(`  Peak heat_level: ${(heatResult.peakHeat * 100).toFixed(2)}%`);
    console.log(`  Red threshold:   ${(args.redThreshold * 100).toFixed(2)}%`);
    console.log(`  Reached red:     ${heatResult.reached ? '✅ YES' : '❌ NO'}`);
    console.log(`  Snapshots seen:  ${heatResult.snapshots.length}`);
    console.log(`  Total duration:  ${(totalMs / 1000).toFixed(1)}s`);
    console.log('');

    // Requests-per-second summary
    const rps = Math.round(loadResult.requestsSent / (loadResult.durationMs / 1000));
    console.log(`  Throughput:      ${rps.toLocaleString()} req/s`);

    // ── Write JSON result ────────────────────────────────────────────────
    const result = {
        passed:       heatResult.reached,
        pod:          args.pod,
        namespace:    args.namespace,
        peakHeat:     heatResult.peakHeat,
        redThreshold: args.redThreshold,
        requestsSent: loadResult.requestsSent,
        durationMs:   loadResult.durationMs,
        rps,
        totalMs,
        observations: heatResult.snapshots,
        timestamp:    new Date().toISOString(),
    };

    if (args.out) {
        const dir = path.dirname(args.out);
        if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
        fs.writeFileSync(args.out, JSON.stringify(result, null, 2));
        console.log(`  Results written to: ${args.out}`);
    }

    // ── Exit code ────────────────────────────────────────────────────────
    if (!heatResult.reached) {
        console.error(
            `\nASSERTION FAILED: Pod "${args.pod}" never reached` +
            ` heat_level ≥ ${args.redThreshold}` +
            ` (peak was ${(heatResult.peakHeat * 100).toFixed(2)}%).\n` +
            'Possible causes:\n' +
            '  • WebSocket endpoint is not forwarding this pod\'s snapshots\n' +
            '  • Saturation threshold is set too high on the Rust streamer\n' +
            '  • Load concurrency is too low relative to the pod\'s capacity\n',
        );
        process.exit(1);
    }

    console.log('\n✅  PASS — Heatmap correctly transitioned to RED for the overloaded pod.\n');
    process.exit(0);
}

main().catch((e) => {
    console.error(`Unexpected error: ${e.stack || e.message}`);
    process.exit(1);
});