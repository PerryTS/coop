// @perch/runtime — Object storage.
//
// Files on disk in v0, wrapped in an S3-ish interface. perch-worker sets
// PERCH_STORAGE_DIR to a per-deployment directory; all paths are relative
// to that root.
//
// Usage:
//   import { storage } from "@perch/runtime";
//   await storage.put("uploads/avatar.jpg", imageBytes, { contentType: "image/jpeg" });
//   const bytes = await storage.get("uploads/avatar.jpg");
//
// ──────────────────────────────────────────────────────────────────────
// On-disk layout
// ──────────────────────────────────────────────────────────────────────
//
//   <PERCH_STORAGE_DIR>/          one directory per deployment, created and
//                                 owned by perch-worker, never by this module
//     objects/<key>               the bytes, verbatim
//     meta/<key>.json             {"contentType": "..."} when one was given
//
// Two trees rather than a sidecar next to each object, because `list()`
// enumerates `objects/` and a sidecar sharing that tree would be listed as if
// it were an object. Deployment isolation comes from the root: the host hands
// this module a directory that belongs to exactly one deployment, and every
// key is resolved under it after the traversal check in `objectPath`. That is
// the same shape as `kv`'s key prefix — the host picks the namespace, the
// application picks only what goes inside it.
//
// ──────────────────────────────────────────────────────────────────────
// What Perry's fs actually does, and what this module does about it
// ──────────────────────────────────────────────────────────────────────
//
// This module is compiled by Perry, not Node. Three deviations shape it:
//
//  1. `fs.writeFileSync` DOES NOT THROW on failure. Perry's runtime returns 0
//     and codegen discards the result (crates/perry-codegen/src/expr/calls/
//     fs.rs), so a full disk, a permissions error or a bad path all look like
//     a successful write. `put` therefore verifies the write by stat'ing the
//     result and comparing byte counts. Do not remove that check on the
//     grounds that "writeFileSync throws" — under Perry it does not.
//
//  2. `fs.readFileSync(path, "utf8")` is lossy above 0x80 in Perry's current
//     stdlib. `get` therefore returns a Buffer, read with the no-encoding form
//     which is binary-exact, and the caller decodes if it wants text. The
//     stub this replaces returned `string | null`, which could not have
//     carried the `imageBytes` the module's own usage example passes in.
//
//  3. `fs.readdirSync` returns [] for a missing directory where Node throws
//     ENOENT. `list()` depends on neither: it tests with `existsSync` first,
//     because "this deployment has not written anything yet" is the ordinary
//     state and must list as empty on both. It also walks with plain
//     `readdirSync` + `statSync` rather than `{recursive:true}`, to stay on
//     the most heavily exercised part of the surface.

import * as fs from "fs";

const STORAGE_DIR = process.env.PERCH_STORAGE_DIR || "";

const OBJECTS = "objects";
const META = "meta";

/// Fail before touching the disk when the host did not scope this deployment.
/// The old default of "./storage" resolved relative to the worker's working
/// directory, which is shared by every deployment on the box — an unset root
/// is a data-isolation break, not a default worth having.
function checkConfigured(): void {
  if (STORAGE_DIR === "") {
    throw new Error(
      "storage is not available: PERCH_STORAGE_DIR is not set. The object " +
      "store is scoped per deployment by the host, and storage requires a " +
      "dedicated worker process — set isolation.class = \"dedicated\" in " +
      "perch.toml, and configure [paths] storage_dir in runtime.toml."
    );
  }
}

function root(): string {
  let dir = STORAGE_DIR;
  while (dir.length > 1 && dir.charAt(dir.length - 1) === "/") {
    dir = dir.substring(0, dir.length - 1);
  }
  return dir;
}

/// Reject any key that could resolve outside the deployment's own directory,
/// or that names something other than a single object.
///
/// This is the isolation boundary. It runs before every filesystem call, and
/// it rejects rather than sanitises: silently rewriting "../../etc/passwd" to
/// "etc/passwd" would store the object somewhere the caller did not ask for.
function checkKey(key: string): void {
  if (typeof key !== "string" || key.length === 0) {
    throw new TypeError("storage key must be a non-empty string");
  }
  if (key.charAt(0) === "/") {
    throw new TypeError("storage key must be relative, got '" + key + "'");
  }
  if (key.indexOf("\\") >= 0) {
    throw new TypeError(
      "storage key must not contain a backslash, got '" + key + "'"
    );
  }
  if (key.indexOf("\0") >= 0) {
    throw new TypeError("storage key must not contain a NUL byte");
  }
  const segments = key.split("/");
  for (let i = 0; i < segments.length; i++) {
    const segment = segments[i];
    if (segment === "") {
      throw new TypeError(
        "storage key must not contain an empty path segment, got '" + key + "'"
      );
    }
    if (segment === "." || segment === "..") {
      throw new TypeError(
        "storage key must not contain a '" + segment + "' segment, got '" +
        key + "'"
      );
    }
  }
}

function objectPath(key: string): string {
  return root() + "/" + OBJECTS + "/" + key;
}

function metaPath(key: string): string {
  return root() + "/" + META + "/" + key + ".json";
}

function parentDir(path: string): string {
  const slash = path.lastIndexOf("/");
  return slash <= 0 ? "/" : path.substring(0, slash);
}

/// Byte length of what we are about to write, so the post-write stat can tell
/// a successful write from a silently failed one. `data.length` is wrong for a
/// non-ASCII string: it counts UTF-16 units, not bytes.
function byteLength(data: string | Buffer): number {
  return Buffer.isBuffer(data) ? data.length : Buffer.byteLength(data);
}

function removeIfPresent(path: string): void {
  if (fs.existsSync(path)) {
    fs.unlinkSync(path);
  }
}

/// Collect object keys under `dir`, depth first.
///
/// `prefix` is the key path accumulated so far, not a filter; filtering
/// happens in `list` so that a partial-segment prefix such as "up" still
/// matches "uploads/a.jpg", the way S3 behaves.
function collectKeys(dir: string, prefix: string, out: string[]): void {
  // Perry returns [] for a missing directory where Node throws ENOENT. Test
  // for it explicitly rather than depending on either: an empty store — no
  // `objects/` yet — is the ordinary state of a deployment that has not
  // written anything, and it must list as empty under both.
  if (!fs.existsSync(dir)) return;
  const entries = fs.readdirSync(dir);
  for (let i = 0; i < entries.length; i++) {
    const name = entries[i];
    const child = dir + "/" + name;
    const key = prefix === "" ? name : prefix + "/" + name;
    if (fs.statSync(child).isDirectory()) {
      collectKeys(child, key, out);
    } else {
      out.push(key);
    }
  }
}

export const storage = {
  /// Store bytes under `key`, replacing anything already there.
  ///
  /// `data` may be a Buffer (binary-exact) or a string (written as UTF-8).
  /// `contentType` is recorded alongside the object; v0 has no reader for it,
  /// but recording it means a later `head()` or static-serving path does not
  /// have to guess, and re-putting without one clears the stale value.
  async put(
    key: string,
    data: string | Buffer,
    opts?: { contentType?: string }
  ): Promise<void> {
    checkConfigured();
    checkKey(key);
    if (data === undefined || data === null) {
      throw new TypeError("storage data must be a string or Buffer");
    }

    const path = objectPath(key);
    fs.mkdirSync(parentDir(path), { recursive: true });
    fs.writeFileSync(path, data);

    // Perry's writeFileSync swallows its errors — see the header. Without
    // this the caller cannot distinguish a stored object from a lost one.
    const expected = byteLength(data);
    if (!fs.existsSync(path)) {
      throw new Error(
        "storage put failed: '" + key + "' was not written to " + path
      );
    }
    const written = fs.statSync(path).size;
    if (written !== expected) {
      throw new Error(
        "storage put failed: '" + key + "' is " + written + " bytes on disk, " +
        "expected " + expected
      );
    }

    const contentType = opts?.contentType;
    const meta = metaPath(key);
    if (contentType !== undefined && contentType !== "") {
      fs.mkdirSync(parentDir(meta), { recursive: true });
      fs.writeFileSync(meta, JSON.stringify({ contentType: contentType }));
    } else {
      removeIfPresent(meta);
    }
  },

  /// Read the bytes stored under `key`, or null if there is no such object.
  ///
  /// Returns a Buffer rather than a string: Perry's utf8 read is lossy above
  /// 0x80, so a string return could not round-trip the binary payloads this
  /// interface exists to carry. Decode with `bytes.toString("utf8")` when the
  /// object is known to be text.
  async get(key: string): Promise<Buffer | null> {
    checkConfigured();
    checkKey(key);
    const path = objectPath(key);
    if (!fs.existsSync(path)) return null;
    return fs.readFileSync(path);
  },

  /// Remove `key` and its recorded metadata. Deleting an absent key succeeds,
  /// matching S3 and matching `del`'s void return — there is no reply channel
  /// for "it wasn't there".
  async del(key: string): Promise<void> {
    checkConfigured();
    checkKey(key);
    removeIfPresent(objectPath(key));
    removeIfPresent(metaPath(key));
  },

  /// List stored keys in lexicographic order.
  ///
  /// `prefix` matches on the raw key string, not on whole path segments, so
  /// "up" matches "uploads/a.jpg". `limit` caps the returned count.
  async list(opts?: { prefix?: string; limit?: number }): Promise<string[]> {
    checkConfigured();

    const limit = opts?.limit;
    if (limit !== undefined) {
      if (typeof limit !== "number" || !Number.isInteger(limit) || limit < 0) {
        throw new RangeError("storage list { limit } must be a non-negative integer");
      }
      if (limit === 0) return [];
    }

    const keys: string[] = [];
    collectKeys(root() + "/" + OBJECTS, "", keys);
    keys.sort();

    const prefix = opts?.prefix;
    const matched: string[] = [];
    for (let i = 0; i < keys.length; i++) {
      if (prefix === undefined || prefix === "" || keys[i].indexOf(prefix) === 0) {
        matched.push(keys[i]);
        if (limit !== undefined && matched.length >= limit) break;
      }
    }
    return matched;
  },
};
