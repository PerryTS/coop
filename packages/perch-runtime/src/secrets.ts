// @perch/runtime — Secrets accessor.
//
// perch-worker decrypts the deployment's secrets file at startup and
// sets PERCH_SECRET_<NAME> env vars. This module provides a clean
// accessor that returns the value or throws if the secret isn't
// configured.
//
// Usage:
//   import { secrets } from "@perch/runtime";
//   const token = secrets.get("POSTMARK_TOKEN");

export const secrets = {
  get(name: string): string {
    const envKey = "PERCH_SECRET_" + name;
    const value = process.env[envKey];
    if (value === undefined || value === "") {
      throw new Error(
        "Secret '" + name + "' not found. " +
        "Add it to the deployment's secrets file and list it in " +
        "perch.toml [capabilities] secrets = [\"" + name + "\"]."
      );
    }
    return value;
  },
};
