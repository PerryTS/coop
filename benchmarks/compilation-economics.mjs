#!/usr/bin/env node

import { execFileSync, spawn } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const options = parseArguments(process.argv.slice(2));
const extension = process.platform === "darwin" ? "dylib" : "so";
const perch = path.join(repository, "target", options.profile, "perch");
const perry = path.join(repository, ".perry-main", "target", "perry-dev", "perry");
const runtimeProvider = path.join(repository, "var", "perch", "lib", `libperry_runtime.${extension}`);
const stdlibProvider = path.join(repository, "var", "perch", "lib", `libperry_stdlib.${extension}`);

for (const required of [perch, perry, runtimeProvider, stdlibProvider]) {
  if (!existsSync(required)) {
    throw new Error(`missing ${required}; build Perch and the pinned Perry providers first`);
  }
}

const trials = [];
for (let trial = 1; trial <= options.trials; trial += 1) {
  const root = mkdtempSync(path.join(os.tmpdir(), "perch-compile-benchmark-"));
  try {
    const result = await runTrial(root, trial);
    trials.push(result);
    for (const scenario of result.scenarios) {
      console.log(formatSample(trial, scenario));
    }
  } finally {
    if (options.keepTemp) {
      console.error(`retained benchmark workspace: ${root}`);
    } else {
      rmSync(root, { recursive: true, force: true });
    }
  }
}

const report = {
  schemaVersion: 1,
  generatedAt: new Date().toISOString(),
  platform: `${process.arch}-${process.platform}`,
  loadAverage: os.loadavg(),
  trials,
  medians: medianScenarios(trials),
};
console.log(JSON.stringify(report, null, 2));
if (options.output) writeFileSync(path.resolve(options.output), `${JSON.stringify(report, null, 2)}\n`);

async function runTrial(root, trial) {
  const port = await freePort();
  const paths = {
    deployments: path.join(root, "deployments"),
    compiled: path.join(root, "compiled"),
    sockets: path.join(root, "sockets"),
    storage: path.join(root, "storage"),
    logs: path.join(root, "logs"),
    acme: path.join(root, "acme"),
  };
  for (const directory of Object.values(paths)) mkdirSync(directory, { recursive: true });
  const deployment = path.join(paths.deployments, "compile-small");
  createFixture(deployment);
  const config = path.join(root, "runtime.toml");
  writeFileSync(config, runtimeConfig(paths, root, port));

  const child = spawn(perch, ["--config", config], {
    cwd: repository,
    detached: true,
    env: { ...process.env, RUST_LOG: "info" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  let log = "";
  const capture = (chunk) => {
    log += chunk;
    if (log.length > 8 * 1024 * 1024) log = log.slice(-8 * 1024 * 1024);
  };
  child.stdout.on("data", capture);
  child.stderr.on("data", capture);

  const scenarios = [];
  try {
    const coldLogStart = log.length;
    const coldStarted = performance.now();
    const coldSampler = sampleProcessTree(child.pid);
    await waitForBody(port, "dep-v1:source-v1", child, options.timeoutMs);
    const cold = await coldSampler.stop();
    scenarios.push(scenarioResult(
      "cold_start_and_activation",
      performance.now() - coldStarted,
      cold,
      log.slice(coldLogStart),
    ));

    scenarios.push(await adminAction("warm_no_op", child, () => {}, "reload", "reload", () => log, port));

    writeFileSync(path.join(deployment, "handlers", "main.ts"), handlerSource("source-v2"));
    scenarios.push(await adminAction(
      "one_module_change",
      child,
      () => {},
      "reload",
      "reload",
      () => log,
      port,
      "dep-v1:source-v2",
    ));

    writeFileSync(path.join(deployment, "node_modules", "bench-dep", "index.js"), dependencySource("dep-v2"));
    scenarios.push(await adminAction(
      "dependency_change",
      child,
      () => {},
      "reload",
      "reload",
      () => log,
      port,
      "dep-v2:source-v2",
    ));

    writeFileSync(path.join(deployment, "public", "index.html"), "<h1>static v2</h1>\n");
    scenarios.push(await adminAction(
      "static_only_change",
      child,
      () => {},
      "reload",
      "reload",
      () => log,
      port,
      "dep-v2:source-v2",
    ));

    const artifactState = await fetchAdminJson(port, "/_perch/admin/deployments/compile-small/artifacts");
    const rollbackPackage = artifactState.previous?.[0];
    if (!rollbackPackage) throw new Error("artifact state did not expose a rollback package");
    scenarios.push(await adminAction(
      "rollback",
      child,
      () => {},
      `rollback/${encodeURIComponent(rollbackPackage)}`,
      "rollback",
      () => log,
      port,
      "dep-v2:source-v2",
    ));
    const replacementSoak = options.soakCycles > 0
      ? await runReplacementSoak(child, () => log, port, deployment, paths.compiled)
      : null;
    return { trial, loadAverage: os.loadavg()[0], scenarios, replacementSoak };
  } catch (error) {
    error.message += `\ndaemon log:\n${log.slice(-32_768)}`;
    throw error;
  } finally {
    await stopProcessGroup(child);
  }
}

async function runReplacementSoak(child, readLog, port, deployment, compiled) {
  const logStart = readLog().length;
  const baselineMemory = await fetchAdminJson(port, "/_perch/admin/deployments/compile-small/memory");
  const baseline = {
    ...resourceCheckpoint(child.pid, 0),
    arenaLiveBytes: baselineMemory.arena_live_bytes,
    arenaReservedBytes: baselineMemory.arena_reserved_bytes,
  };
  const checkpoints = [baseline];
  let trafficRequests = 0;
  const trafficErrors = [];
  let stopTraffic = false;
  const traffic = (async () => {
    while (!stopTraffic) {
      try {
        const response = await applicationRequest(port);
        trafficRequests += 1;
        if (response.status !== 200 || response.body !== "dep-v2:source-v2") {
          trafficErrors.push(`HTTP ${response.status} ${JSON.stringify(response.body)}`);
        }
      } catch (error) {
        trafficErrors.push(error.message);
      }
      if (trafficErrors.length >= 10) return;
    }
  })();
  const started = performance.now();
  try {
    for (let cycle = 1; cycle <= options.soakCycles; cycle += 1) {
      writeFileSync(path.join(deployment, "public", "index.html"), `<h1>soak ${cycle}</h1>\n`);
      const response = await fetch(`http://127.0.0.1:${port}/_perch/admin/deployments/compile-small/reload`, {
        method: "POST",
        headers: {
          authorization: `Basic ${Buffer.from("perch:test-secret").toString("base64")}`,
          "x-perch-confirm": "reload",
        },
        signal: AbortSignal.timeout(options.timeoutMs),
      });
      const body = await response.text();
      if (!response.ok) throw new Error(`replacement soak cycle ${cycle} failed with HTTP ${response.status}: ${body}`);
      // Reload returns only after activation and the router update. A retrying
      // readiness loop would hide an atomic-publication regression, so verify
      // the first response immediately.
      const live = await applicationRequest(port);
      if (live.status !== 200 || live.body !== "dep-v2:source-v2") {
        throw new Error(`replacement soak cycle ${cycle} published HTTP ${live.status} ${JSON.stringify(live.body)}`);
      }
      if (cycle % 10 === 0 || cycle === options.soakCycles) {
        const artifactState = await fetchAdminJson(port, "/_perch/admin/deployments/compile-small/artifacts");
        const health = await fetchAdminJson(port, "/_perch/admin/deployments/compile-small/health");
        const memory = await fetchAdminJson(port, "/_perch/admin/deployments/compile-small/memory");
        if (artifactState.packages.length > 3) {
          throw new Error(`replacement soak retained ${artifactState.packages.length} packages at cycle ${cycle}`);
        }
        if (health.outcome !== "success") {
          throw new Error(`replacement soak health is ${JSON.stringify(health)} at cycle ${cycle}`);
        }
        checkpoints.push({
          ...resourceCheckpoint(child.pid, cycle),
          packages: artifactState.packages.length,
          activePackage: artifactState.active,
          activationMs: health.duration_ms,
          arenaLiveBytes: memory.arena_live_bytes,
          arenaReservedBytes: memory.arena_reserved_bytes,
        });
      }
    }
  } finally {
    stopTraffic = true;
    await traffic;
  }
  await delay(100);
  const finalMemory = await fetchAdminJson(port, "/_perch/admin/deployments/compile-small/memory");
  const final = {
    ...resourceCheckpoint(child.pid, options.soakCycles),
    arenaLiveBytes: finalMemory.arena_live_bytes,
    arenaReservedBytes: finalMemory.arena_reserved_bytes,
  };
  const soakLog = readLog().slice(logStart);
  if (soakLog.includes("Perry compiler process completed")) {
    throw new Error("static-only replacement soak unexpectedly spawned Perry");
  }
  if (soakLog.includes("preloading application library in daemon")) {
    throw new Error("static-only replacement soak loaded a second native application image");
  }
  const runtimeReuses = countOccurrences(
    soakLog,
    "reused initialized live application runtime for byte-identical code",
  );
  if (runtimeReuses !== options.soakCycles) {
    throw new Error(`replacement soak reused the initialized runtime ${runtimeReuses}/${options.soakCycles} times`);
  }
  if (trafficErrors.length > 0) {
    throw new Error(`replacement traffic observed ${trafficErrors.length} errors: ${trafficErrors.slice(0, 3).join("; ")}`);
  }
  if (final.rssKib > baseline.rssKib + 64 * 1024) {
    throw new Error(`replacement soak retained more than 64 MiB RSS: ${baseline.rssKib} -> ${final.rssKib} KiB`);
  }
  if (Number.isFinite(baseline.threads) && Number.isFinite(final.threads) && final.threads > baseline.threads + 2) {
    throw new Error(`replacement soak leaked threads: ${baseline.threads} -> ${final.threads}`);
  }
  if (Number.isFinite(baseline.fds) && Number.isFinite(final.fds) && final.fds > baseline.fds + 4) {
    throw new Error(`replacement soak leaked descriptors: ${baseline.fds} -> ${final.fds}`);
  }
  const measured = checkpoints.slice(Math.floor(checkpoints.length / 2));
  const rssSlopeKibPerCycle = linearSlope(measured, "rssKib");
  const pssSlopeKibPerCycle = linearSlope(measured, "pssKib");
  const privateDirtySlopeKibPerCycle = linearSlope(measured, "privateDirtyKib");
  const threadSlopePerCycle = linearSlope(measured, "threads");
  const fdSlopePerCycle = linearSlope(measured, "fds");
  const arenaLiveBytesPerCycle = linearSlope(measured, "arenaLiveBytes");
  const arenaReservedBytesPerCycle = linearSlope(measured, "arenaReservedBytes");
  if (Number.isFinite(rssSlopeKibPerCycle) && rssSlopeKibPerCycle > 64) {
    throw new Error(`replacement soak retained RSS at ${rssSlopeKibPerCycle.toFixed(3)} KiB/cycle`);
  }
  if (Number.isFinite(pssSlopeKibPerCycle) && pssSlopeKibPerCycle > 64) {
    throw new Error(`replacement soak retained PSS at ${pssSlopeKibPerCycle.toFixed(3)} KiB/cycle`);
  }
  if (Number.isFinite(privateDirtySlopeKibPerCycle) && privateDirtySlopeKibPerCycle > 64) {
    throw new Error(
      `replacement soak retained private-dirty memory at ${privateDirtySlopeKibPerCycle.toFixed(3)} KiB/cycle`,
    );
  }
  if (Number.isFinite(threadSlopePerCycle) && threadSlopePerCycle > 0.01) {
    throw new Error(`replacement soak retained threads at ${threadSlopePerCycle.toFixed(4)}/cycle`);
  }
  if (Number.isFinite(fdSlopePerCycle) && fdSlopePerCycle > 0.02) {
    throw new Error(`replacement soak retained descriptors at ${fdSlopePerCycle.toFixed(4)}/cycle`);
  }
  // Reserved arena space is a high-water mark and may grow in block-sized
  // steps, but live bytes must not grow with every replacement/request. Keep
  // a small allowance for sampling noise and bounded caches while failing the
  // pinned Perry behavior (~22 KiB/cycle) decisively.
  if (Number.isFinite(arenaLiveBytesPerCycle) && arenaLiveBytesPerCycle > 1024) {
    throw new Error(
      `replacement soak retained live Perry arena data at ${arenaLiveBytesPerCycle.toFixed(3)} bytes/cycle`,
    );
  }
  return {
    cycles: options.soakCycles,
    wallMs: performance.now() - started,
    trafficRequests,
    trafficErrors: trafficErrors.length,
    compilerSpawned: false,
    runtimeReuses,
    baseline,
    final,
    checkpoints,
    retainedResourceSlope: {
      rssKibPerCycle: rssSlopeKibPerCycle,
      pssKibPerCycle: Number.isFinite(pssSlopeKibPerCycle) ? pssSlopeKibPerCycle : null,
      privateDirtyKibPerCycle: Number.isFinite(privateDirtySlopeKibPerCycle)
        ? privateDirtySlopeKibPerCycle
        : null,
      threadsPerCycle: Number.isFinite(threadSlopePerCycle) ? threadSlopePerCycle : null,
      fdsPerCycle: Number.isFinite(fdSlopePerCycle) ? fdSlopePerCycle : null,
      arenaLiveBytesPerCycle: Number.isFinite(arenaLiveBytesPerCycle) ? arenaLiveBytesPerCycle : null,
      arenaReservedBytesPerCycle: Number.isFinite(arenaReservedBytesPerCycle) ? arenaReservedBytesPerCycle : null,
    },
    artifactNamespace: path.relative(repository, path.join(compiled, "compile-small")),
  };
}

function countOccurrences(text, needle) {
  let count = 0;
  let offset = 0;
  while ((offset = text.indexOf(needle, offset)) !== -1) {
    count += 1;
    offset += needle.length;
  }
  return count;
}

function linearSlope(samples, field) {
  const points = samples
    .map((sample) => [sample.cycle, sample[field]])
    .filter(([, value]) => Number.isFinite(value));
  if (points.length < 2) return Number.NaN;
  const meanX = points.reduce((sum, [x]) => sum + x, 0) / points.length;
  const meanY = points.reduce((sum, [, y]) => sum + y, 0) / points.length;
  let numerator = 0;
  let denominator = 0;
  for (const [x, y] of points) {
    numerator += (x - meanX) * (y - meanY);
    denominator += (x - meanX) ** 2;
  }
  return denominator === 0 ? Number.NaN : numerator / denominator;
}

async function fetchAdminJson(port, pathname) {
  const response = await fetch(`http://127.0.0.1:${port}${pathname}`, {
    headers: {
      authorization: `Basic ${Buffer.from("perch:test-secret").toString("base64")}`,
    },
    signal: AbortSignal.timeout(options.timeoutMs),
  });
  const body = await response.text();
  if (!response.ok) throw new Error(`${pathname} failed with HTTP ${response.status}: ${body}`);
  return JSON.parse(body);
}

async function adminAction(name, child, mutate, action, confirmation, readLog, port, expectedBody = "dep-v1:source-v1") {
  mutate();
  const logStart = readLog().length;
  const started = performance.now();
  const sampler = sampleProcessTree(child.pid);
  const response = await fetch(`http://127.0.0.1:${port}/_perch/admin/deployments/compile-small/${action}`, {
    method: "POST",
    headers: {
      authorization: `Basic ${Buffer.from("perch:test-secret").toString("base64")}`,
      "x-perch-confirm": confirmation,
    },
  });
  const responseBody = await response.text();
  if (!response.ok) throw new Error(`${name} failed with HTTP ${response.status}: ${responseBody}`);
  await waitForBody(port, expectedBody, child, options.timeoutMs);
  const sampled = await sampler.stop();
  return scenarioResult(name, performance.now() - started, sampled, readLog().slice(logStart));
}

function scenarioResult(name, wallMs, sampled, log) {
  return {
    name,
    wallMs,
    cpuMs: sampled.cpuMs,
    peakTreeRssKib: sampled.peakRssKib,
    finalTreeRssKib: sampled.final.rssKib,
    finalTreePssKib: sampled.final.pssKib,
    finalTreePrivateDirtyKib: sampled.final.privateDirtyKib,
    phasesMs: parsePhases(log),
    compilerPeakRssBytes: numberField(log, "peak_rss_bytes"),
    codegenCache: parseCodegenCache(log),
    packageCacheHit: log.includes("dylib is up-to-date, skipping compile"),
    compiledCodeReuse: log.includes("reused verified compiled application image"),
    compilerSpawned: log.includes("Perry compiler process completed"),
  };
}

function createFixture(deployment) {
  mkdirSync(path.join(deployment, "handlers"), { recursive: true });
  mkdirSync(path.join(deployment, "public"), { recursive: true });
  mkdirSync(path.join(deployment, "node_modules", "bench-dep"), { recursive: true });
  writeFileSync(path.join(deployment, "perch.toml"), `name = "compile-small"

[hosts]
domains = ["compile-small.test"]

[[handlers]]
file = "handlers/main.ts"
path = "/"
method = "GET"

[[static]]
directory = "./public"
path = "/static"

[activation]
path = "/"
method = "GET"
requests = 2
expected_status = 200
`);
  writeFileSync(path.join(deployment, "package.json"), JSON.stringify({ name: "compile-small", type: "module" }, null, 2));
  writeFileSync(path.join(deployment, "handlers", "main.ts"), handlerSource("source-v1"));
  writeFileSync(path.join(deployment, "node_modules", "bench-dep", "package.json"), JSON.stringify({ name: "bench-dep", type: "module", main: "index.js" }, null, 2));
  writeFileSync(path.join(deployment, "node_modules", "bench-dep", "index.js"), dependencySource("dep-v1"));
  writeFileSync(path.join(deployment, "public", "index.html"), "<h1>static v1</h1>\n");
}

function dependencySource(value) {
  return `export const marker = ${JSON.stringify(value)};\n`;
}

function handlerSource(sourceVersion) {
  return `import { marker } from "bench-dep";

export function handle(_frame: Buffer): Buffer {
  const body = Buffer.from(marker + ":" + ${JSON.stringify(sourceVersion)});
  const output = Buffer.alloc(5 + 2 + 4 + 4 + body.length);
  output[0] = 0x50; output[1] = 0x43; output[2] = 0x48; output[3] = 0x32; output[4] = 2;
  let offset = 5;
  output.writeUInt16BE(200, offset); offset += 2;
  output.writeUInt32BE(0, offset); offset += 4;
  output.writeUInt32BE(body.length, offset); offset += 4;
  body.copy(output, offset);
  return output;
}
`;
}

function runtimeConfig(paths, root, port) {
  // bcrypt cost 4 for the benchmark-only password `test-secret`.
  const passwordHash = "$2y$04$kU4tpFv4IIOFA0b502hIceOq/so4ykm4at0sjqiwr7YWkd4egTTPm";
  return `[http]
listen_http = "127.0.0.1:${port}"

[paths]
deployments_dir = ${toml(paths.deployments)}
compiled_dir = ${toml(paths.compiled)}
sockets_dir = ${toml(paths.sockets)}
storage_dir = ${toml(paths.storage)}
logs_dir = ${toml(paths.logs)}
acme_cache_dir = ${toml(paths.acme)}
state_db = ${toml(path.join(root, "state.sqlite"))}
perry_binary = ${toml(perry)}
perry_runtime_library = ${toml(runtimeProvider)}
perry_stdlib_library = ${toml(stdlibProvider)}

[execution]
compile_concurrency = 1
compile_march = "generic"
watch_deployments = false
artifact_retention_count = 3
artifact_retention_days = 0

[tls]
mode = "off"

[admin]
path = "/_perch/admin"
password_hash = ${toml(passwordHash)}
`;
}

function toml(value) {
  return JSON.stringify(value);
}

async function waitForBody(port, expected, child, timeoutMs) {
  const deadline = performance.now() + timeoutMs;
  let lastObserved = "no response";
  while (performance.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`daemon exited with code ${child.exitCode}`);
    try {
      const response = await applicationRequest(port);
      lastObserved = `HTTP ${response.status} ${JSON.stringify(response.body)}`;
      if (response.status >= 200 && response.status < 300 && response.body === expected) return;
    } catch (error) {
      lastObserved = error.message;
    }
    await delay(25);
  }
  throw new Error(`expected body ${JSON.stringify(expected)} was not ready within ${timeoutMs} ms; last observed ${lastObserved}`);
}

function applicationRequest(port) {
  return new Promise((resolve, reject) => {
    const request = http.get({
      hostname: "127.0.0.1",
      port,
      path: "/",
      headers: { host: "compile-small.test" },
    }, (response) => {
      const chunks = [];
      response.on("data", (chunk) => chunks.push(chunk));
      response.on("end", () => resolve({
        status: response.statusCode,
        body: Buffer.concat(chunks).toString("utf8"),
      }));
    });
    request.once("error", reject);
  });
}

function sampleProcessTree(pid) {
  const baseline = processTree(pid);
  let peakRssKib = baseline.rssKib;
  let peakCpuMs = baseline.cpuMs;
  const interval = setInterval(() => {
    try {
      const sample = processTree(pid, false);
      peakRssKib = Math.max(peakRssKib, sample.rssKib);
      peakCpuMs = Math.max(peakCpuMs, sample.cpuMs);
    } catch {}
  }, 10);
  return {
    async stop() {
      clearInterval(interval);
      await delay(20);
      const final = processTree(pid);
      peakRssKib = Math.max(peakRssKib, final.rssKib);
      peakCpuMs = Math.max(peakCpuMs, final.cpuMs);
      return {
        peakRssKib,
        cpuMs: Math.max(0, peakCpuMs - baseline.cpuMs),
        final,
      };
    },
  };
}

function processTree(rootPid, includeSmaps = true) {
  const output = execFileSync("ps", ["-axo", "pid=,ppid=,rss=,time="], { encoding: "utf8" });
  const rows = output.trim().split("\n").map((line) => {
    const match = line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(.+)$/);
    return match && { pid: Number(match[1]), ppid: Number(match[2]), rssKib: Number(match[3]), cpuMs: parseCpuTime(match[4]) };
  }).filter(Boolean);
  const members = new Set([rootPid]);
  for (let changed = true; changed;) {
    changed = false;
    for (const row of rows) {
      if (members.has(row.ppid) && !members.has(row.pid)) {
        members.add(row.pid);
        changed = true;
      }
    }
  }
  const tree = rows.filter((row) => members.has(row.pid));
  const smaps = includeSmaps && process.platform === "linux"
    ? tree.map((row) => processSmapsRollupKib(row.pid))
    : [];
  return {
    rssKib: tree.reduce((total, row) => total + row.rssKib, 0),
    pssKib: smaps.length > 0 && smaps.every((value) => Number.isFinite(value.pssKib))
      ? smaps.reduce((sum, value) => sum + value.pssKib, 0)
      : Number.NaN,
    privateDirtyKib: smaps.length > 0 && smaps.every((value) => Number.isFinite(value.privateDirtyKib))
      ? smaps.reduce((sum, value) => sum + value.privateDirtyKib, 0)
      : Number.NaN,
    cpuMs: tree.reduce((total, row) => total + row.cpuMs, 0),
  };
}

function processSmapsRollupKib(pid) {
  try {
    const contents = readFileSync(`/proc/${pid}/smaps_rollup`, "utf8");
    const pss = contents.match(/^Pss:\s+(\d+)\s+kB$/m);
    const privateDirty = contents.match(/^Private_Dirty:\s+(\d+)\s+kB$/m);
    return {
      pssKib: pss ? Number(pss[1]) : Number.NaN,
      privateDirtyKib: privateDirty ? Number(privateDirty[1]) : Number.NaN,
    };
  } catch {
    return { pssKib: Number.NaN, privateDirtyKib: Number.NaN };
  }
}

function resourceCheckpoint(pid, cycle) {
  const resources = processTree(pid);
  return {
    cycle,
    rssKib: resources.rssKib,
    pssKib: resources.pssKib,
    privateDirtyKib: resources.privateDirtyKib,
    cpuMs: resources.cpuMs,
    threads: processThreadCount(pid),
    fds: processFileDescriptorCount(pid),
  };
}

function processThreadCount(pid) {
  try {
    if (process.platform === "linux") return readFileSync(`/proc/${pid}/status`, "utf8").match(/^Threads:\s+(\d+)$/m)?.[1] * 1;
    if (process.platform === "darwin") return Math.max(0, execFileSync("ps", ["-M", "-p", String(pid)], { encoding: "utf8" }).trim().split("\n").length - 1);
  } catch {}
  return Number.NaN;
}

function processFileDescriptorCount(pid) {
  try {
    if (process.platform === "linux") return execFileSync("find", [`/proc/${pid}/fd`, "-mindepth", "1", "-maxdepth", "1"], { encoding: "utf8" }).trim().split("\n").filter(Boolean).length;
    return execFileSync("lsof", ["-a", "-p", String(pid), "-Fn"], { encoding: "utf8" }).split("\n").filter((line) => /^f\d+$/.test(line)).length;
  } catch {
    return Number.NaN;
  }
}

function parseCpuTime(raw) {
  const dayParts = raw.trim().split("-");
  const days = dayParts.length === 2 ? Number(dayParts[0]) : 0;
  const clock = dayParts.at(-1).split(":").map(Number);
  const seconds = clock.pop();
  const minutes = clock.pop() ?? 0;
  const hours = clock.pop() ?? 0;
  return (((days * 24 + hours) * 60 + minutes) * 60 + seconds) * 1000;
}

function parsePhases(log) {
  const phases = {};
  for (const line of log.split("\n")) {
    const match = line.match(/phase=\"?([^\"\s]+)\"?.*outcome=\"?([^\"\s]+)\"?.*elapsed_ms=([0-9.]+)/);
    if (match && match[2] === "success") phases[match[1]] = Number(match[3]);
  }
  return phases;
}

function parseCodegenCache(log) {
  const matches = [...log.matchAll(/Perry codegen cache result.*hits=(\d+) misses=(\d+)/g)];
  if (matches.length === 0) return null;
  const match = matches.at(-1);
  return { hits: Number(match[1]), misses: Number(match[2]) };
}

function numberField(log, name) {
  const matches = [...log.matchAll(new RegExp(`${name}=([0-9.]+)`, "g"))];
  return matches.length > 0 ? Number(matches.at(-1)[1]) : null;
}

function formatSample(trial, scenario) {
  const cache = scenario.codegenCache ? `${scenario.codegenCache.hits}/${scenario.codegenCache.hits + scenario.codegenCache.misses}` : "n/a";
  return `compile-sample trial=${trial} scenario=${scenario.name} wall_ms=${scenario.wallMs.toFixed(3)} cpu_ms=${scenario.cpuMs.toFixed(3)} peak_tree_rss_mib=${(scenario.peakTreeRssKib / 1024).toFixed(3)} final_tree_pss_mib=${Number.isFinite(scenario.finalTreePssKib) ? (scenario.finalTreePssKib / 1024).toFixed(3) : "n/a"} final_tree_private_dirty_mib=${Number.isFinite(scenario.finalTreePrivateDirtyKib) ? (scenario.finalTreePrivateDirtyKib / 1024).toFixed(3) : "n/a"} compiler_peak_rss_mib=${scenario.compilerPeakRssBytes === null ? "n/a" : (scenario.compilerPeakRssBytes / 1024 / 1024).toFixed(3)} codegen_cache=${cache} package_hit=${scenario.packageCacheHit} code_reuse=${scenario.compiledCodeReuse} compiler_spawned=${scenario.compilerSpawned}`;
}

function medianScenarios(trials) {
  const names = trials[0]?.scenarios.map((scenario) => scenario.name) ?? [];
  return Object.fromEntries(names.map((name) => {
    const scenarios = trials.map((trial) => trial.scenarios.find((scenario) => scenario.name === name));
    return [name, {
      wallMs: median(scenarios.map((scenario) => scenario.wallMs)),
      cpuMs: median(scenarios.map((scenario) => scenario.cpuMs)),
      peakTreeRssKib: median(scenarios.map((scenario) => scenario.peakTreeRssKib)),
      compilerPeakRssBytes: nullableMedian(scenarios.map((scenario) => scenario.compilerPeakRssBytes)),
    }];
  }));
}

function median(values) {
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.floor(sorted.length / 2)];
}

function nullableMedian(values) {
  const finite = values.filter(Number.isFinite);
  return finite.length === 0 ? null : median(finite);
}

async function freePort() {
  const server = net.createServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = server.address().port;
  await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  return port;
}

async function stopProcessGroup(child) {
  if (child.exitCode !== null) return;
  try { process.kill(-child.pid, "SIGTERM"); } catch {}
  await Promise.race([new Promise((resolve) => child.once("exit", resolve)), delay(2_000)]);
  if (child.exitCode === null) {
    try { process.kill(-child.pid, "SIGKILL"); } catch {}
  }
}

function parseArguments(arguments_) {
  const parsed = { trials: 3, profile: "debug", timeoutMs: 120_000, output: "", keepTemp: false, soakCycles: 0 };
  for (let index = 0; index < arguments_.length; index += 1) {
    const argument = arguments_[index];
    if (argument === "--keep-temp") {
      parsed.keepTemp = true;
      continue;
    }
    const value = arguments_[index += 1];
    if (value === undefined) throw new Error(`missing value for ${argument}`);
    if (argument === "--trials") parsed.trials = positiveInteger(argument, value);
    else if (argument === "--profile") parsed.profile = value;
    else if (argument === "--timeout-ms") parsed.timeoutMs = positiveInteger(argument, value);
    else if (argument === "--output") parsed.output = value;
    else if (argument === "--soak-cycles") parsed.soakCycles = positiveInteger(argument, value);
    else throw new Error(`unknown option ${argument}`);
  }
  return parsed;
}

function positiveInteger(name, value) {
  const number = Number(value);
  if (!Number.isInteger(number) || number <= 0) throw new Error(`${name} must be positive`);
  return number;
}
