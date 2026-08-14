#!/usr/bin/env node

import { spawn, execFileSync } from "node:child_process";
import {
  lstatSync,
  mkdirSync,
  readFileSync,
  rmdirSync,
  writeFileSync,
} from "node:fs";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { setTimeout as delay } from "node:timers/promises";

const linuxClockTicksPerSecond = process.platform === "linux"
  ? Number(execFileSync("getconf", ["CLK_TCK"], { encoding: "utf8" }).trim())
  : Number.NaN;
const options = parseArguments(process.argv.slice(2));
const samples = [];
let requestAgent;

for (let trial = 1; trial <= options.trials; trial += 1) {
  const port = options.port;
  await assertPortFree(port);
  const command = options.command.map((part) => replaceTrialTokens(part, port, trial));
  const cgroup = prepareBenchmarkCgroup(options, trial);
  const serverCommand = commandForCgroup(command, cgroup);
  requestAgent = new http.Agent({ keepAlive: true, maxSockets: options.concurrency });
  const loadAverageStart = os.loadavg()[0];
  const started = performance.now();
  const child = spawn(serverCommand[0], serverCommand.slice(1), {
    cwd: options.cwd,
    detached: true,
    env: {
      ...process.env,
      ...Object.fromEntries(
        options.environment.map(([name, value]) => [
          name,
          replaceTrialTokens(value, port, trial),
        ]),
      ),
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let serverLog = "";
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  const captureLog = (chunk) => {
    if (serverLog.length < 32_768) serverLog += chunk;
  };
  child.stdout.on("data", captureLog);
  child.stderr.on("data", captureLog);

  try {
    await waitForTcp(port, child, options.readyTimeoutMs);
    assertCgroupMembership(child.pid, cgroup);
    const startupMs = performance.now() - started;
    const startupCpuMs = processTree(child.pid).cpuMs;
    const readyMemory = await medianMemoryKib(child.pid);
    const readyCgroupKib = cgroupMemoryCurrentKib(cgroup);

    const firstCpuBeforeMs = processTree(child.pid).cpuMs;
    const warmStarted = performance.now();
    await checkedRequest(options, port);
    const warmMs = performance.now() - warmStarted;
    const firstRequestCpuMs = Math.max(0, processTree(child.pid).cpuMs - firstCpuBeforeMs);
    const warmMemory = await medianMemoryKib(child.pid);
    const warmCgroupKib = cgroupMemoryCurrentKib(cgroup);

    const cpuBeforeMs = processTree(child.pid).cpuMs;
    const workloadStarted = performance.now();
    const latenciesMs = await runWorkload(options, port);
    const workloadMs = performance.now() - workloadStarted;
    const cpuMs = Math.max(0, processTree(child.pid).cpuMs - cpuBeforeMs);
    const postMemory = await medianMemoryKib(child.pid);
    const postCgroupKib = cgroupMemoryCurrentKib(cgroup);
    const cgroupPeakKib = cgroupMemoryPeakKib(cgroup);

    const sample = {
      startupMs,
      startupCpuMs,
      readyRssKib: readyMemory.rssKib,
      readyPssKib: readyMemory.pssKib,
      readyPrivateDirtyKib: readyMemory.privateDirtyKib,
      readyCgroupKib,
      warmMs,
      firstRequestCpuMs,
      warmRssKib: warmMemory.rssKib,
      warmPssKib: warmMemory.pssKib,
      warmPrivateDirtyKib: warmMemory.privateDirtyKib,
      warmCgroupKib,
      workloadMs,
      latencyP50Ms: percentile(latenciesMs, 0.50),
      latencyP99Ms: percentile(latenciesMs, 0.99),
      cpuMs,
      postRssKib: postMemory.rssKib,
      postPssKib: postMemory.pssKib,
      postPrivateDirtyKib: postMemory.privateDirtyKib,
      postCgroupKib,
      cgroupPeakKib,
      loadAverageStart,
      loadAverageEnd: os.loadavg()[0],
    };
    samples.push(sample);
    console.log(formatSample(options.name, trial, options.requests, sample));
    if (process.env.PERCH_BENCH_TRACE_SERVER_LOG === "1" && serverLog.trim() !== "") {
      console.log(`server_log name=${JSON.stringify(options.name)} trial=${trial}\n${serverLog.trim()}`);
    }
  } catch (error) {
    if (serverLog.trim() !== "") {
      error.message += `\nserver log:\n${serverLog.trim()}`;
    }
    throw error;
  } finally {
    requestAgent.destroy();
    await stopProcessGroup(child);
    removeBenchmarkCgroup(cgroup);
  }
  await delay(250);
}

console.log(formatResult(options, samples));

function parseArguments(arguments_) {
  const parsed = {
    name: "server",
    cwd: process.cwd(),
    host: "127.0.0.1",
    requestHost: "benchmark.local",
    path: "/api/benchmark?iterations=100",
    expectedRuntime: "",
    trials: 3,
    requests: 20_000,
    concurrency: 50,
    port: 4580,
    readyTimeoutMs: 120_000,
    cgroupRoot: process.env.PERCH_BENCH_CGROUP_ROOT ?? "",
    environment: [],
    command: [],
  };

  let index = 0;
  while (index < arguments_.length) {
    const argument = arguments_[index];
    if (argument === "--") {
      parsed.command = arguments_.slice(index + 1);
      break;
    }
    const value = arguments_[index + 1];
    if (value === undefined) throw new Error(`missing value for ${argument}`);
    switch (argument) {
      case "--name": parsed.name = value; break;
      case "--cwd": parsed.cwd = value; break;
      case "--host": parsed.requestHost = value; break;
      case "--path": parsed.path = value; break;
      case "--expected-runtime": parsed.expectedRuntime = value; break;
      case "--trials": parsed.trials = positiveInteger(argument, value); break;
      case "--requests": parsed.requests = positiveInteger(argument, value); break;
      case "--concurrency": parsed.concurrency = positiveInteger(argument, value); break;
      case "--port": parsed.port = positiveInteger(argument, value); break;
      case "--ready-timeout-ms": parsed.readyTimeoutMs = positiveInteger(argument, value); break;
      case "--cgroup-root": parsed.cgroupRoot = value; break;
      case "--env": {
        const separator = value.indexOf("=");
        if (separator < 1) throw new Error(`invalid --env value: ${value}`);
        parsed.environment.push([value.slice(0, separator), value.slice(separator + 1)]);
        break;
      }
      default: throw new Error(`unknown option: ${argument}`);
    }
    index += 2;
  }
  if (parsed.command.length === 0) throw new Error("expected a command after --");
  return parsed;
}

function prepareBenchmarkCgroup(configuration, trial) {
  if (!configuration.cgroupRoot) return null;
  if (process.platform !== "linux") {
    throw new Error("benchmark cgroups require Linux");
  }
  const root = configuration.cgroupRoot;
  if (!path.isAbsolute(root)) {
    throw new Error("benchmark cgroup root must be absolute");
  }
  try {
    const metadata = lstatSync(root);
    if (metadata.isSymbolicLink() || !metadata.isDirectory()) {
      throw new Error("benchmark cgroup root must be a real directory");
    }
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
    mkdirSync(root);
  }

  const controllers = new Set(readFileSync(path.join(root, "cgroup.controllers"), "utf8").trim().split(/\s+/));
  for (const required of ["memory", "cpu", "pids"]) {
    if (!controllers.has(required)) {
      throw new Error(`benchmark cgroup root does not expose ${required}`);
    }
  }
  writeFileSync(path.join(root, "cgroup.subtree_control"), "+memory +cpu +pids");

  const label = configuration.name.replace(/[^A-Za-z0-9_-]/g, "_");
  const trialPath = path.join(root, `run-${process.pid}-${label}-t${trial}`);
  mkdirSync(trialPath);
  try {
    writeFileSync(path.join(trialPath, "memory.max"), "max");
    writeFileSync(path.join(trialPath, "memory.swap.max"), "max");
    writeFileSync(path.join(trialPath, "memory.oom.group"), "1");
    writeFileSync(path.join(trialPath, "cpu.max"), "max 100000");
    writeFileSync(path.join(trialPath, "pids.max"), "max");
    readFileSync(path.join(trialPath, "memory.peak"), "utf8");
    return trialPath;
  } catch (error) {
    try { rmdirSync(trialPath); } catch {}
    throw error;
  }
}

function commandForCgroup(command, cgroup) {
  if (!cgroup) return command;
  return [
    "/bin/sh",
    "-c",
    'set -e; printf "%s\\n" "$$" > "$1"; shift; exec "$@"',
    "perch-cgroup-exec",
    path.join(cgroup, "cgroup.procs"),
    ...command,
  ];
}

function assertCgroupMembership(pid, cgroup) {
  if (!cgroup) return;
  const mountRoot = "/sys/fs/cgroup";
  if (cgroup !== mountRoot && !cgroup.startsWith(`${mountRoot}/`)) {
    throw new Error(`benchmark cgroup must be below ${mountRoot}: ${cgroup}`);
  }
  const expected = cgroup.slice(mountRoot.length) || "/";
  const membership = readFileSync(`/proc/${pid}/cgroup`, "utf8")
    .split("\n")
    .find((line) => line.startsWith("0::"))
    ?.slice(3);
  if (membership !== expected) {
    throw new Error(
      `server ${pid} joined ${membership ?? "no unified cgroup"}, expected ${expected}`,
    );
  }
  if (cgroupMemoryCurrentKib(cgroup) <= 0) {
    throw new Error(`server cgroup has no accounted memory after readiness: ${cgroup}`);
  }
}

function cgroupMemoryCurrentKib(cgroup) {
  return cgroup ? readCgroupBytes(cgroup, "memory.current") / 1024 : Number.NaN;
}

function cgroupMemoryPeakKib(cgroup) {
  return cgroup ? readCgroupBytes(cgroup, "memory.peak") / 1024 : Number.NaN;
}

function readCgroupBytes(cgroup, name) {
  const value = Number(readFileSync(path.join(cgroup, name), "utf8").trim());
  if (!Number.isFinite(value)) throw new Error(`invalid ${name} for ${cgroup}`);
  return value;
}

function removeBenchmarkCgroup(cgroup) {
  if (!cgroup) return;
  try {
    rmdirSync(cgroup);
  } catch (error) {
    console.error(`warning: failed to remove benchmark cgroup ${cgroup}: ${error.message}`);
  }
}

function replaceTrialTokens(value, port, trial) {
  return value
    .replaceAll("{port}", String(port))
    .replaceAll("{trial}", String(trial));
}

function positiveInteger(name, value) {
  const number = Number(value);
  if (!Number.isInteger(number) || number <= 0) throw new Error(`${name} must be positive`);
  return number;
}

async function assertPortFree(port) {
  const server = net.createServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, options.host, resolve);
  });
  await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
}

async function waitForTcp(port, child, timeoutMs) {
  const deadline = performance.now() + timeoutMs;
  while (performance.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(`server exited before ready with code ${child.exitCode}`);
    }
    const connected = await new Promise((resolve) => {
      const socket = net.createConnection({ host: options.host, port });
      socket.once("connect", () => { socket.destroy(); resolve(true); });
      socket.once("error", () => resolve(false));
      socket.setTimeout(100, () => { socket.destroy(); resolve(false); });
    });
    if (connected) return;
    await delay(5);
  }
  throw new Error(`server did not listen on ${port} within ${timeoutMs}ms`);
}

async function checkedRequest(configuration, port) {
  const { statusCode, bodyText } = await new Promise((resolve, reject) => {
    const request = http.get({
      hostname: configuration.host,
      port,
      path: configuration.path,
      headers: { Host: configuration.requestHost },
      agent: requestAgent,
    }, (response) => {
      response.setEncoding("utf8");
      let bodyText = "";
      response.on("data", (chunk) => { bodyText += chunk; });
      response.on("end", () => resolve({ statusCode: response.statusCode, bodyText }));
    });
    request.once("error", reject);
  });
  if (statusCode < 200 || statusCode >= 300) throw new Error(`HTTP ${statusCode}: ${bodyText}`);
  const body = JSON.parse(bodyText);
  if (configuration.expectedRuntime && body.runtime !== configuration.expectedRuntime) {
    throw new Error(
      `expected runtime ${configuration.expectedRuntime}, received ${body.runtime}; `
      + `response=${bodyText}`,
    );
  }
  if (body.iterations !== 100 || body.checksum !== 3_726_872_593) {
    throw new Error(`unexpected benchmark response: ${JSON.stringify(body)}`);
  }
}

async function runWorkload(configuration, port) {
  let cursor = 0;
  const latenciesMs = new Array(configuration.requests);
  async function worker() {
    while (true) {
      const index = cursor;
      cursor += 1;
      if (index >= configuration.requests) return;
      const started = performance.now();
      await checkedRequest(configuration, port);
      latenciesMs[index] = performance.now() - started;
    }
  }
  await Promise.all(Array.from({ length: configuration.concurrency }, () => worker()));
  return latenciesMs;
}

function processTree(rootPid) {
  const output = execFileSync("ps", ["-axo", "pid=,ppid=,rss=,time="], { encoding: "utf8" });
  const rows = output.trim().split("\n").map((line) => {
    const match = line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(.+)$/);
    return match && {
      pid: Number(match[1]),
      ppid: Number(match[2]),
      rssKib: Number(match[3]),
      cpuMs: processCpuMs(Number(match[1]), match[4]),
    };
  }).filter(Boolean);
  const members = new Set([rootPid]);
  let changed = true;
  while (changed) {
    changed = false;
    for (const row of rows) {
      if (members.has(row.ppid) && !members.has(row.pid)) {
        members.add(row.pid);
        changed = true;
      }
    }
  }
  const tree = rows.filter((row) => members.has(row.pid));
  const smapsReadings = process.platform === "linux"
    ? tree.map((row) => processSmapsRollupKib(row.pid))
    : [];
  return {
    rssKib: tree.reduce((total, row) => total + row.rssKib, 0),
    pssKib: smapsReadings.length > 0 && smapsReadings.every((value) => Number.isFinite(value.pssKib))
      ? smapsReadings.reduce((total, value) => total + value.pssKib, 0)
      : Number.NaN,
    privateDirtyKib: smapsReadings.length > 0 && smapsReadings.every((value) => Number.isFinite(value.privateDirtyKib))
      ? smapsReadings.reduce((total, value) => total + value.privateDirtyKib, 0)
      : Number.NaN,
    cpuMs: tree.reduce((total, row) => total + row.cpuMs, 0),
  };
}

function processCpuMs(pid, psTime) {
  if (process.platform !== "linux") return parseCpuTime(psTime);
  try {
    const stat = readFileSync(`/proc/${pid}/stat`, "utf8");
    const close = stat.lastIndexOf(")");
    if (close < 0) throw new Error("missing process name terminator");
    const fields = stat.slice(close + 1).trim().split(/\s+/);
    if (fields.length <= 12) throw new Error("truncated process stat");
    const ticks = Number(fields[11]) + Number(fields[12]);
    return ticks * 1000 / linuxClockTicksPerSecond;
  } catch {
    return parseCpuTime(psTime);
  }
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

function parseCpuTime(raw) {
  const dayParts = raw.trim().split("-");
  const days = dayParts.length === 2 ? Number(dayParts[0]) : 0;
  const clock = dayParts.at(-1).split(":").map(Number);
  const seconds = clock.pop();
  const minutes = clock.pop() ?? 0;
  const hours = clock.pop() ?? 0;
  return (((days * 24 + hours) * 60 + minutes) * 60 + seconds) * 1000;
}

async function medianMemoryKib(pid) {
  const rssReadings = [];
  const pssReadings = [];
  const privateDirtyReadings = [];
  for (let index = 0; index < 7; index += 1) {
    const memory = processTree(pid);
    rssReadings.push(memory.rssKib);
    pssReadings.push(memory.pssKib);
    privateDirtyReadings.push(memory.privateDirtyKib);
    await delay(15);
  }
  return {
    rssKib: median(rssReadings),
    pssKib: pssReadings.every(Number.isFinite) ? median(pssReadings) : Number.NaN,
    privateDirtyKib: privateDirtyReadings.every(Number.isFinite)
      ? median(privateDirtyReadings)
      : Number.NaN,
  };
}

async function stopProcessGroup(child) {
  if (child.exitCode !== null) return;
  try { process.kill(-child.pid, "SIGTERM"); } catch {}
  await waitForExit(child, 1_000);
  if (child.exitCode === null) {
    try { process.kill(-child.pid, "SIGKILL"); } catch {}
    await waitForExit(child, 1_000);
  }
}

async function waitForExit(child, timeoutMs) {
  if (child.exitCode !== null) return;
  await Promise.race([
    new Promise((resolve) => child.once("exit", resolve)),
    delay(timeoutMs),
  ]);
}

function median(values) {
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.floor(sorted.length / 2)];
}

function percentile(values, fraction) {
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.ceil(fraction * sorted.length) - 1];
}

function formatSample(name, trial, requests, sample) {
  return `sample name=${JSON.stringify(name)} trial=${trial} load1_start=${sample.loadAverageStart.toFixed(2)} load1_end=${sample.loadAverageEnd.toFixed(2)} startup_ms=${sample.startupMs.toFixed(3)} startup_cpu_ms=${sample.startupCpuMs.toFixed(3)} ready_rss_mib=${mib(sample.readyRssKib)} ready_pss_mib=${mib(sample.readyPssKib)} ready_private_dirty_mib=${mib(sample.readyPrivateDirtyKib)} ready_cgroup_mib=${mib(sample.readyCgroupKib)} first_request_ms=${sample.warmMs.toFixed(3)} usable_cold_start_ms=${(sample.startupMs + sample.warmMs).toFixed(3)} first_request_cpu_ms=${sample.firstRequestCpuMs.toFixed(3)} warm_rss_mib=${mib(sample.warmRssKib)} warm_pss_mib=${mib(sample.warmPssKib)} warm_private_dirty_mib=${mib(sample.warmPrivateDirtyKib)} warm_cgroup_mib=${mib(sample.warmCgroupKib)} requests=${requests} workload_ms=${sample.workloadMs.toFixed(3)} requests_per_second=${rate(requests, sample.workloadMs)} latency_p50_ms=${sample.latencyP50Ms.toFixed(3)} latency_p99_ms=${sample.latencyP99Ms.toFixed(3)} server_cpu_us_per_request=${cpuPerRequest(requests, sample.cpuMs)} post_rss_mib=${mib(sample.postRssKib)} post_pss_mib=${mib(sample.postPssKib)} post_private_dirty_mib=${mib(sample.postPrivateDirtyKib)} post_cgroup_mib=${mib(sample.postCgroupKib)} cgroup_peak_mib=${mib(sample.cgroupPeakKib)}`;
}

function formatResult(configuration, values) {
  const field = (name) => median(values.map((sample) => sample[name]));
  const usableColdStarts = values.map((sample) => sample.startupMs + sample.warmMs);
  return `RESULT name=${JSON.stringify(configuration.name)} trials=${values.length} load1_start_median=${field("loadAverageStart").toFixed(2)} load1_end_median=${field("loadAverageEnd").toFixed(2)} startup_median_ms=${field("startupMs").toFixed(3)} startup_cpu_median_ms=${field("startupCpuMs").toFixed(3)} ready_rss_median_mib=${mib(field("readyRssKib"))} ready_pss_median_mib=${mib(field("readyPssKib"))} ready_private_dirty_median_mib=${mib(field("readyPrivateDirtyKib"))} ready_cgroup_median_mib=${mib(field("readyCgroupKib"))} first_request_median_ms=${field("warmMs").toFixed(3)} usable_cold_start_median_ms=${median(usableColdStarts).toFixed(3)} first_request_cpu_median_ms=${field("firstRequestCpuMs").toFixed(3)} warm_rss_median_mib=${mib(field("warmRssKib"))} warm_pss_median_mib=${mib(field("warmPssKib"))} warm_private_dirty_median_mib=${mib(field("warmPrivateDirtyKib"))} warm_cgroup_median_mib=${mib(field("warmCgroupKib"))} requests=${configuration.requests} concurrency=${configuration.concurrency} workload_median_ms=${field("workloadMs").toFixed(3)} requests_per_second=${rate(configuration.requests, field("workloadMs"))} latency_p50_median_ms=${field("latencyP50Ms").toFixed(3)} latency_p99_median_ms=${field("latencyP99Ms").toFixed(3)} server_cpu_us_per_request=${cpuPerRequest(configuration.requests, field("cpuMs"))} post_rss_median_mib=${mib(field("postRssKib"))} post_pss_median_mib=${mib(field("postPssKib"))} post_private_dirty_median_mib=${mib(field("postPrivateDirtyKib"))} post_cgroup_median_mib=${mib(field("postCgroupKib"))} cgroup_peak_median_mib=${mib(field("cgroupPeakKib"))}`;
}

function mib(kib) { return (kib / 1024).toFixed(3); }
function rate(requests, milliseconds) { return (requests * 1000 / milliseconds).toFixed(3); }
function cpuPerRequest(requests, milliseconds) { return (milliseconds * 1000 / requests).toFixed(3); }
