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

const STORAGE_DIR = process.env.PERCH_STORAGE_DIR || "./storage";

export const storage = {
  async put(key: string, data: string, opts?: { contentType?: string }): Promise<void> {
    // TODO: wire to Perry's fs stdlib
    //   import fs from "fs";
    //   const path = STORAGE_DIR + "/" + key;
    //   fs.mkdirSync(path.substring(0, path.lastIndexOf("/")), { recursive: true });
    //   fs.writeFileSync(path, data);
    console.log(JSON.stringify({ _perch_storage: "put", key, contentType: opts?.contentType }));
  },

  async get(key: string): Promise<string | null> {
    console.log(JSON.stringify({ _perch_storage: "get", key }));
    return null;
  },

  async del(key: string): Promise<void> {
    console.log(JSON.stringify({ _perch_storage: "del", key }));
  },

  async list(opts?: { prefix?: string; limit?: number }): Promise<string[]> {
    console.log(JSON.stringify({ _perch_storage: "list", prefix: opts?.prefix }));
    return [];
  },
};
