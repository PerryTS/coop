//! Behavioural tests for `@coop/runtime`'s `kv` and `storage`.
//!
//! These two modules are TypeScript compiled by Perry, so the obvious place to
//! test them is a Perry end-to-end run. That is also the test that does not
//! exist on most machines: the daemon suites that compile TypeScript all
//! self-skip without a built pinned compiler, and a skip reads as a pass.
//!
//! So these run the shipped source under **Node**, which executes TypeScript
//! directly, against a fake `ioredis` and a real temporary directory. What
//! that proves and what it does not:
//!
//! * It proves the logic — key prefixing, the SET-then-EXPIRE TTL path,
//!   traversal rejection, binary round-tripping, listing, and every "refuse
//!   rather than silently do the wrong thing" branch. That is where the bugs
//!   live, and none of it was covered before.
//! * It does not prove Perry lowers these calls. Perry's ioredis binding is a
//!   fixed dispatch table and an unlisted method compiles to `undefined` with
//!   no error, so the method *names* are a separate risk. That is what
//!   `kv_only_calls_ioredis_methods_perry_can_lower` covers, by reading the
//!   pinned compiler's own table.
//!
//! Node is required, not optional. A skip here would be indistinguishable
//! from a pass, which is the failure mode this file exists to avoid.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("worker crate is inside the workspace")
        .to_path_buf()
}

fn runtime_package_src() -> PathBuf {
    workspace_root().join("packages/coop-runtime/src")
}

/// A fake `ioredis` that records every call and implements exactly the
/// commands Perry's dispatch table can reach. `EXPIRE_REPLY` lets a test make
/// EXPIRE fail the way a real server would if the key had already gone.
const FAKE_IOREDIS: &str = r#"
export const CALLS = [];
const DATA = new Map();
export const TTL = new Map();
const EXPIRE_REPLY = process.env.FAKE_EXPIRE_REPLY;

export default class Redis {
  async get(k) {
    CALLS.push(["get", k]);
    return DATA.has(k) ? DATA.get(k) : null;
  }
  async set(k, v) {
    CALLS.push(["set", k, v]);
    DATA.set(k, v);
    return "OK";
  }
  async del(k) {
    CALLS.push(["del", k]);
    return DATA.delete(k) ? 1 : 0;
  }
  async incr(k) {
    CALLS.push(["incr", k]);
    const n = Number(DATA.get(k) || 0) + 1;
    DATA.set(k, String(n));
    return n;
  }
  async expire(k, seconds) {
    CALLS.push(["expire", k, seconds]);
    if (EXPIRE_REPLY !== undefined) return Number(EXPIRE_REPLY);
    if (!DATA.has(k)) return 0;
    TTL.set(k, seconds);
    return 1;
  }
}
"#;

/// A scratch package containing the *shipped* runtime sources plus the fake
/// `ioredis`, so `import Redis from "ioredis"` inside `kv.ts` resolves without
/// installing anything into the repository.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("scratch directory");
        let root = dir.path();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"coop-runtime-behavior","private":true,"type":"module"}"#,
        )
        .unwrap();

        let fake = root.join("node_modules/ioredis");
        std::fs::create_dir_all(&fake).unwrap();
        std::fs::write(
            fake.join("package.json"),
            r#"{"name":"ioredis","version":"0.0.0-fake","type":"module","main":"index.js"}"#,
        )
        .unwrap();
        std::fs::write(fake.join("index.js"), FAKE_IOREDIS).unwrap();

        // Copy rather than symlink: the modules must resolve `ioredis` through
        // this fixture's node_modules, not through anything in the repository.
        for module in ["kv.ts", "storage.ts"] {
            std::fs::copy(runtime_package_src().join(module), root.join(module))
                .unwrap_or_else(|error| panic!("copying {module}: {error}"));
        }

        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn storage_root(&self) -> PathBuf {
        self.dir.path().join("store")
    }

    /// Run `script` as an ES module inside the fixture. Node's own exit status
    /// is the verdict, so the scripts below assert with `node:assert/strict`.
    fn run(&self, script: &str, env: &[(&str, &str)]) -> Output {
        let script_path = self.dir.path().join("drive.mjs");
        std::fs::write(&script_path, script).unwrap();

        let mut command = Command::new(node_binary());
        command.current_dir(self.path()).arg(&script_path);
        // Start from a clean capability environment so a variable leaking in
        // from the developer's shell cannot make an unconfigured-path test
        // pass by accident.
        command.env_remove("COOP_REDIS_PREFIX");
        command.env_remove("COOP_REDIS_URL");
        command.env_remove("COOP_STORAGE_DIR");
        command.env_remove("FAKE_EXPIRE_REPLY");
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().expect("running node")
    }

    fn run_ok(&self, script: &str, env: &[(&str, &str)]) -> String {
        let output = self.run(script, env);
        assert!(
            output.status.success(),
            "node script failed ({}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }
}

/// Node is a hard requirement here. It is already a prerequisite of this
/// repository (the CI job typechecks the runtime package with `tsc` and the
/// benchmarks are `.mjs`), and making it optional would turn every assertion
/// below into something that cannot fail on a machine without it.
fn node_binary() -> &'static str {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let version = Command::new("node").arg("--version").output().expect(
            "node is required to test @coop/runtime's TypeScript modules; \
             install Node 22 or newer",
        );
        assert!(
            version.status.success(),
            "`node --version` failed: {}",
            String::from_utf8_lossy(&version.stderr)
        );
    });
    "node"
}

// ──────────────────────────────────────────────────────────────────────
// kv
// ──────────────────────────────────────────────────────────────────────

#[test]
fn kv_prefixes_every_key_and_round_trips_values() {
    let fixture = Fixture::new();
    let out = fixture.run_ok(
        r#"
import assert from "node:assert/strict";
const { CALLS } = await import("ioredis");
const { kv } = await import("./kv.ts");

await kv.set("session:abc", "payload");
assert.equal(await kv.get("session:abc"), "payload",
  "a value written through kv must be readable through kv");
assert.equal(await kv.get("session:missing"), null,
  "an absent key must be null, not undefined");
assert.equal(await kv.del("session:abc"), 1);
assert.equal(await kv.incr("rate:1.2.3.4"), 1);
assert.equal(await kv.incr("rate:1.2.3.4"), 2);

// Every key that reached the server carries the host-assigned prefix. This is
// the isolation property: without it two deployments share one keyspace.
for (const call of CALLS) {
  assert.ok(call[1].startsWith("coop:chirp:"),
    `unprefixed key reached redis: ${JSON.stringify(call)}`);
}
assert.deepEqual(CALLS[0], ["set", "coop:chirp:session:abc", "payload"]);
console.log("OK");
"#,
        &[
            ("COOP_REDIS_PREFIX", "coop:chirp:"),
            ("COOP_REDIS_URL", "redis://127.0.0.1:6379"),
        ],
    );
    assert!(out.contains("OK"));
}

#[test]
fn kv_ttl_is_a_set_then_expire_pair_because_perry_cannot_lower_set_ex() {
    let fixture = Fixture::new();
    fixture.run_ok(
        r#"
import assert from "node:assert/strict";
const { CALLS, TTL } = await import("ioredis");
const { kv } = await import("./kv.ts");

await kv.set("session:abc", "payload", { ex: 3600 });

// Perry's `set` row declares exactly two string arguments, so a third and
// fourth would be dropped and the TTL silently lost. The only reachable path
// is SET followed by EXPIRE.
assert.deepEqual(CALLS, [
  ["set", "coop:chirp:session:abc", "payload"],
  ["expire", "coop:chirp:session:abc", 3600],
]);
assert.equal(TTL.get("coop:chirp:session:abc"), 3600,
  "the TTL must actually be applied, not merely requested");
console.log("OK");
"#,
        &[
            ("COOP_REDIS_PREFIX", "coop:chirp:"),
            ("COOP_REDIS_URL", "redis://127.0.0.1:6379"),
        ],
    );
}

#[test]
fn kv_throws_when_expire_does_not_apply_rather_than_leaking_an_eternal_key() {
    let fixture = Fixture::new();
    fixture.run_ok(
        r#"
import assert from "node:assert/strict";
const { kv } = await import("./kv.ts");

// SET succeeds, EXPIRE reports "no such key". Because the pair is not atomic,
// this is reachable in production; reporting success would leave a key that
// never expires and a caller that believes it will.
await assert.rejects(
  () => kv.set("session:abc", "payload", { ex: 60 }),
  /TTL was not applied/,
);
console.log("OK");
"#,
        &[
            ("COOP_REDIS_PREFIX", "coop:chirp:"),
            ("COOP_REDIS_URL", "redis://127.0.0.1:6379"),
            ("FAKE_EXPIRE_REPLY", "0"),
        ],
    );
}

#[test]
fn kv_refuses_to_run_unprefixed_or_unconfigured() {
    let fixture = Fixture::new();

    // No prefix: every deployment on the box would share one keyspace. An
    // unset prefix must not degrade to "no prefix".
    fixture.run_ok(
        r#"
import assert from "node:assert/strict";
const { CALLS } = await import("ioredis");
const { kv } = await import("./kv.ts");
for (const op of [() => kv.get("k"), () => kv.set("k", "v"),
                  () => kv.del("k"), () => kv.incr("k")]) {
  await assert.rejects(op, /COOP_REDIS_PREFIX is not set/);
}
assert.deepEqual(CALLS, [], "nothing may reach redis without a key prefix");
console.log("OK");
"#,
        &[("COOP_REDIS_URL", "redis://127.0.0.1:6379")],
    );

    // Prefix but no URL: the host has not configured Redis at all. Perry's
    // `new Redis()` would happily default to rediss://127.0.0.1:6379.
    fixture.run_ok(
        r#"
import assert from "node:assert/strict";
const { kv } = await import("./kv.ts");
await assert.rejects(() => kv.get("k"), /COOP_REDIS_URL is not set/);
console.log("OK");
"#,
        &[("COOP_REDIS_PREFIX", "coop:chirp:")],
    );
}

#[test]
fn kv_rejects_a_ttl_redis_would_truncate() {
    let fixture = Fixture::new();
    fixture.run_ok(
        r#"
import assert from "node:assert/strict";
const { CALLS } = await import("ioredis");
const { kv } = await import("./kv.ts");

// EXPIRE has one-second resolution. Accepting 0.5 or 0 would store the value
// with a TTL the caller did not ask for, or none at all.
for (const bad of [0, -1, 0.5, Number.NaN, Number.POSITIVE_INFINITY]) {
  await assert.rejects(() => kv.set("k", "v", { ex: bad }),
    (error) => error instanceof RangeError || error instanceof TypeError,
    `ex=${bad} must be refused`);
}
assert.deepEqual(CALLS, [], "a refused TTL must not leave the value stored");

// An absent option object still means "no expiry", not "invalid".
await kv.set("k", "v");
await kv.set("k", "v", {});
console.log("OK");
"#,
        &[
            ("COOP_REDIS_PREFIX", "coop:chirp:"),
            ("COOP_REDIS_URL", "redis://127.0.0.1:6379"),
        ],
    );
}

/// The pinned Perry compiler resolves `client.<method>(...)` through a fixed
/// table. A method with no row does not become a call — it evaluates to
/// `undefined`, with no compile error and no runtime error. So a typo, or a
/// perfectly reasonable command like `setex` or `ttl`, is a silent no-op that
/// no behavioural test using a fake client can catch.
///
/// This reads the pinned compiler's own table and asserts `kv.ts` stays inside
/// it. It is the compile-side half of the coverage.
#[test]
fn kv_only_calls_ioredis_methods_perry_can_lower() {
    let table = workspace_root()
        .join(".perry-main/crates/perry-codegen/src/lower_call/native_table/databases.rs");
    assert!(
        table.is_file(),
        "the pinned Perry checkout is missing at {}; run scripts/sync-perry-main.sh",
        table.display()
    );
    let source = std::fs::read_to_string(&table).unwrap();

    // Rows are `NativeModSig { module: "ioredis", ..., method: "get", ... }`.
    let mut lowerable: Vec<String> = Vec::new();
    for row in source.split("NativeModSig {").skip(1) {
        let row = row.split("},").next().unwrap_or(row);
        if !row.contains(r#"module: "ioredis""#) {
            continue;
        }
        if let Some(rest) = row.split(r#"method: ""#).nth(1) {
            if let Some(name) = rest.split('"').next() {
                lowerable.push(name.to_string());
            }
        }
    }
    assert!(
        lowerable.contains(&"get".to_string()) && lowerable.contains(&"expire".to_string()),
        "failed to parse the ioredis dispatch table; got {lowerable:?}"
    );

    let kv_source = std::fs::read_to_string(runtime_package_src().join("kv.ts")).unwrap();
    let mut used: Vec<String> = Vec::new();
    for call in kv_source.split("client.").skip(1) {
        if let Some(name) = call.split('(').next() {
            let name = name.trim();
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric()) {
                used.push(name.to_string());
            }
        }
    }
    assert!(
        !used.is_empty(),
        "kv.ts makes no ioredis calls at all — it is still a stub"
    );
    for method in &used {
        assert!(
            lowerable.contains(method),
            "kv.ts calls client.{method}(), which has no row in Perry's ioredis \
             dispatch table {}. Perry would compile it to `undefined` with no \
             error. Lowerable methods: {lowerable:?}",
            table.display()
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// storage
// ──────────────────────────────────────────────────────────────────────

#[test]
fn storage_round_trips_bytes_exactly_and_lays_them_out_under_the_deployment_root() {
    let fixture = Fixture::new();
    let root = fixture.storage_root();
    fixture.run_ok(
        r#"
import assert from "node:assert/strict";
const { storage } = await import("./storage.ts");

// Bytes above 0x80 are the point: Perry's utf8 read is lossy there, so a
// string-returning `get` could not carry the image payloads this interface
// advertises.
const bytes = Buffer.from([0x00, 0x7f, 0x80, 0xff, 0xfe, 0x0a]);
await storage.put("uploads/avatar.jpg", bytes, { contentType: "image/jpeg" });

const read = await storage.get("uploads/avatar.jpg");
assert.ok(Buffer.isBuffer(read), "get must return a Buffer, not a lossy string");
assert.deepEqual([...read], [...bytes], "bytes must round-trip exactly");

assert.equal(await storage.get("uploads/nothing.jpg"), null,
  "an absent key must be null");

// Strings are written as UTF-8 and come back byte-identical.
await storage.put("notes/hello.txt", "héllo");
assert.equal((await storage.get("notes/hello.txt")).toString("utf8"), "héllo");
console.log("OK");
"#,
        &[("COOP_STORAGE_DIR", root.to_str().unwrap())],
    );

    // The documented layout, asserted from outside the module.
    assert!(
        root.join("objects/uploads/avatar.jpg").is_file(),
        "objects live under <root>/objects/<key>"
    );
    let meta = std::fs::read_to_string(root.join("meta/uploads/avatar.jpg.json")).unwrap();
    assert!(
        meta.contains("image/jpeg"),
        "contentType must be recorded rather than dropped, got {meta}"
    );
    assert!(
        !root.join("meta/notes/hello.txt.json").exists(),
        "no contentType means no metadata file"
    );
}

#[test]
fn storage_refuses_keys_that_would_escape_the_deployment_directory() {
    let fixture = Fixture::new();
    let root = fixture.storage_root();
    fixture.run_ok(
        r#"
import assert from "node:assert/strict";
import fs from "node:fs";
const { storage } = await import("./storage.ts");

// This is the isolation boundary. Each of these resolves outside the
// deployment's own directory, or names something that is not a single object.
const escapes = [
  "../escape", "a/../../escape", "..", ".", "/etc/passwd",
  "a//b", "a/", "", "a\\b", "a\0b",
];
for (const key of escapes) {
  for (const op of [() => storage.put(key, "x"), () => storage.get(key),
                    () => storage.del(key)]) {
    await assert.rejects(op, TypeError, `key ${JSON.stringify(key)} must be refused`);
  }
}

// A legal key that merely *contains* dots is fine — the check is on segments,
// not on substrings.
await storage.put("a..b/c.d.txt", "fine");
assert.equal((await storage.get("a..b/c.d.txt")).toString(), "fine");
console.log("OK");
"#,
        &[("COOP_STORAGE_DIR", root.to_str().unwrap())],
    );

    assert!(
        !root.parent().unwrap().join("escape").exists(),
        "a traversing key must not have written outside the storage root"
    );
}

#[test]
fn storage_delete_is_idempotent_and_removes_metadata() {
    let fixture = Fixture::new();
    let root = fixture.storage_root();
    fixture.run_ok(
        r#"
import assert from "node:assert/strict";
const { storage } = await import("./storage.ts");

await storage.put("uploads/a.jpg", "x", { contentType: "image/jpeg" });
// Establish that there was something to delete. Without this the whole test
// passes against a `del` that does nothing, because nothing was ever stored.
assert.notEqual(await storage.get("uploads/a.jpg"), null,
  "put must store the object before del can be said to remove it");

await storage.del("uploads/a.jpg");
assert.equal(await storage.get("uploads/a.jpg"), null);

// S3 semantics: deleting an absent key succeeds. `del` returns void, so there
// is no channel to report "it wasn't there" anyway.
await storage.del("uploads/a.jpg");
await storage.del("never/existed");

// Re-putting without a contentType must clear the previous one rather than
// leaving a stale type attached to new bytes.
await storage.put("uploads/b", "x", { contentType: "text/plain" });
await storage.put("uploads/b", "y");
assert.equal((await storage.get("uploads/b")).toString(), "y",
  "re-putting must replace the bytes, not append to them");
console.log("OK");
"#,
        &[("COOP_STORAGE_DIR", root.to_str().unwrap())],
    );

    assert!(
        root.join("objects/uploads/b").is_file(),
        "the surviving object proves these paths are the ones del operates on"
    );
    assert!(!root.join("meta/uploads/a.jpg.json").exists());
    assert!(!root.join("objects/uploads/a.jpg").exists());
    assert!(
        !root.join("meta/uploads/b.json").exists(),
        "a put without a contentType must clear the stale one"
    );
}

#[test]
fn storage_list_is_sorted_and_honours_prefix_and_limit() {
    let fixture = Fixture::new();
    let root = fixture.storage_root();
    fixture.run_ok(
        r#"
import assert from "node:assert/strict";
const { storage } = await import("./storage.ts");

assert.deepEqual(await storage.list(), [], "an empty store lists nothing");

for (const key of ["uploads/b.jpg", "uploads/a.jpg", "uploads/nested/c.jpg",
                   "notes/x.txt"]) {
  await storage.put(key, "x", { contentType: "application/octet-stream" });
}

assert.deepEqual(await storage.list(), [
  "notes/x.txt", "uploads/a.jpg", "uploads/b.jpg", "uploads/nested/c.jpg",
]);
assert.deepEqual(await storage.list({ prefix: "uploads/" }), [
  "uploads/a.jpg", "uploads/b.jpg", "uploads/nested/c.jpg",
]);
// A partial-segment prefix matches, the way S3 does.
assert.deepEqual(await storage.list({ prefix: "uploads/a" }), ["uploads/a.jpg"]);
assert.deepEqual(await storage.list({ limit: 2 }), ["notes/x.txt", "uploads/a.jpg"]);
assert.deepEqual(await storage.list({ limit: 0 }), []);
assert.deepEqual(await storage.list({ prefix: "nothing/" }), []);

// The metadata tree must never be listed as if it held objects.
assert.ok(!(await storage.list()).some((k) => k.endsWith(".json")));

await assert.rejects(() => storage.list({ limit: -1 }), RangeError);
console.log("OK");
"#,
        &[("COOP_STORAGE_DIR", root.to_str().unwrap())],
    );
}

#[test]
fn storage_refuses_to_run_without_a_host_assigned_root() {
    let fixture = Fixture::new();
    fixture.run_ok(
        r#"
import assert from "node:assert/strict";
import fs from "node:fs";
const { storage } = await import("./storage.ts");

// The old default was "./storage", relative to the worker's working
// directory — which every deployment on the box shares.
for (const op of [() => storage.put("k", "v"), () => storage.get("k"),
                  () => storage.del("k"), () => storage.list()]) {
  await assert.rejects(op, /COOP_STORAGE_DIR is not set/);
}
assert.ok(!fs.existsSync("./storage"),
  "an unconfigured storage must not fall back to a shared relative directory");
console.log("OK");
"#,
        &[],
    );
}
