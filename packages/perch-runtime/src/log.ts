// @perch/runtime — Structured logging.
//
// Writes JSON-formatted log lines to stdout. perch-worker captures
// stdout via a piped process handle and forwards lines to the daemon,
// which stores them in the SQLite log store and surfaces them in the
// admin UI.
//
// Usage:
//   import { log } from "@perch/runtime";
//   log.info("user signed up", { userId: 42, plan: "pro" });

export const log = {
  debug(msg: string, fields?: Record<string, any>): void {
    emit("debug", msg, fields);
  },
  info(msg: string, fields?: Record<string, any>): void {
    emit("info", msg, fields);
  },
  warn(msg: string, fields?: Record<string, any>): void {
    emit("warn", msg, fields);
  },
  error(msg: string, fields?: Record<string, any>): void {
    emit("error", msg, fields);
  },
};

function emit(level: string, msg: string, fields?: Record<string, any>): void {
  const entry: Record<string, any> = {
    ts: Date.now(),
    level: level,
    msg: msg,
  };
  if (fields) {
    const keys = Object.keys(fields);
    for (let i = 0; i < keys.length; i++) {
      entry[keys[i]] = fields[keys[i]];
    }
  }
  console.log(JSON.stringify(entry));
}
