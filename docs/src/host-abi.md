# The host ABI

Two protocols, both defined in `crates/coop-host-abi` so the daemon, the worker,
and any test harness share one vocabulary.

## Daemon ↔ worker

Length-prefixed JSON over a per-deployment Unix domain socket: a `u32`
big-endian length followed by the payload. Requests are `WorkerRequest`,
responses are `WorkerResponse`.

`ABI_VERSION` is checked on connect and both sides refuse to talk across a
mismatch.

## Host ↔ application dylib: `COOP`

Applications exchange a compact binary frame carried in Perry `Buffer` values.
This is the **only** supported application-library HTTP ABI — there is no JSON
or Base64 fallback, deliberately, so raw body bytes survive the boundary
unmodified.

Relevant constants:

| constant | meaning |
|---|---|
| `ABI_VERSION` | daemon↔worker socket protocol |
| `APP_LIBRARY_ABI_VERSION` | the on-disk Perry application-library contract |
| `APP_LIBRARY_BOUNDARY_VERSION` | Coop's cached native-boundary audit; bumping it forces existing images back through the audit before their cached result is trusted |

These are independent. A change to the socket protocol does not invalidate
compiled application images, and vice versa.

## Handler calling conventions

An application library records its handler's calling convention in a sidecar
manifest (`HandlerAbi`):

- `Bare` — `extern "C" fn(f64) -> f64`
- the closure form — `extern "C" fn(i64, f64) -> f64` with a zero environment

The worker reads this rather than guessing, so a handler compiled under one
convention cannot be invoked under the other.

## Writing a `COOP` handler directly

Most handlers should use [`@coop/runtime`](runtime-api.md), which hides this.
When you need to avoid JSON entirely — large bodies, binary uploads — you can
decode the frame yourself. `examples/landing/handlers/contact.ts` is a worked
example; the shape is length-prefixed fields:

```ts
function readBytes(frame: Buffer, cursor: { offset: number }): Buffer {
  if (cursor.offset + 4 > frame.length) throw new Error("truncated COOP length");
  const length = frame.readUInt32BE(cursor.offset);
  cursor.offset += 4;
  if (cursor.offset + length > frame.length) throw new Error("truncated COOP field");
  const value = frame.subarray(cursor.offset, cursor.offset + length);
  cursor.offset += length;
  return value;
}
```

Handlers written this way take and return `Buffer`, not `string`.

## Exported symbols

A compiled application exports a deliberately minimal surface — the module
initialiser and the handler. Dead stripping is enabled and imports are bound at
link time. The worker pins the symbols it resolves (`symbol_pin.rs`) so a later
`dlopen` in the same process cannot shadow them.
