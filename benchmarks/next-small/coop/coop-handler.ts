// Adapt Coop's compact wire protocol to a REAL Next.js production App Route.
//
// This drives Next's own `AppRouteRouteModule.handle` from the production
// build output, not the userland `GET` export. That distinction is the whole
// point of the fixture: `handle` is what sets up the AsyncLocalStorage work
// stores, resolves the handler for the method, applies `fetchCache`, and
// builds the response. Calling `GET` directly runs the route body and skips
// all of it.
//
// TWO THINGS THIS FIXTURE GOT WRONG BEFORE, BOTH OF WHICH INVALIDATED NUMBERS.
//
// 1. It called `GET(request)`, THREW THE RESULT AWAY, and emitted a hardcoded
//    200 with a hardcoded body. It ran the framework work and then fabricated
//    the answer, so no assertion about the response could ever fail.
//
// 2. It read a `.next-production-bundle/` checked into the repository, which
//    had DRIFTED from `app/api/benchmark/route.ts`: the committed bundle
//    parsed `nextUrl.searchParams`, clamped iterations to 1..10000, and set an
//    `x-perch-benchmark-body` header. The source did none of that. Coop would
//    have been benchmarked against different code than the Node standalone
//    build compiled from source -- not a like-for-like comparison at all.
//
// So the bundle is now BUILT from the same source Node builds from, and the
// committed copy is gone. A build output that lives in git can only drift
// again, silently, and the second failure above is what that costs.
//
// The route path is resolved at build time by `next build` into
// `.next/server/app/api/benchmark/route.js`. Note the current toolchain emits
// a turbopack runtime, while the committed bundle this replaced was webpack --
// another sign of how far that checked-in artifact had drifted.

// The production build output is CommonJS that assigns `module.exports`
// directly and sets no `__esModule` flag, so its interop shape differs by
// toolchain: a namespace import surfaces the exports object under some, and
// under a `default` key under others. A plain default import yields
// `undefined` -- verified, not assumed.
//
// So accept either shape and fail loudly if neither carries a routeModule,
// rather than dereferencing undefined and reporting something confusing.
import * as routeBundleNamespace from "../.next/server/app/api/benchmark/route.js";

// IMPORT ORDER IS LOAD-BEARING. The route bundle must be loaded BEFORE
// `next/server`: loading it installs Next's require hook, and without that
// `next/server` resolves to the edge build, whose module init throws
// "Invariant: AsyncLocalStorage accessed in runtime where it is not
// available". Verified by reordering these two lines and watching it break.
import { NextRequest } from "next/server";

const routeBundle: Record<string, unknown> =
  (routeBundleNamespace as Record<string, unknown>).routeModule !== undefined
    ? (routeBundleNamespace as Record<string, unknown>)
    : (((routeBundleNamespace as Record<string, unknown>).default ??
        {}) as Record<string, unknown>);

const routeModule = routeBundle.routeModule as
  | { handle: (request: object, context: unknown) => Promise<Response> }
  | undefined;
if (routeModule === undefined || typeof routeModule.handle !== "function") {
  throw new Error(
    "production App Route exports are missing: run `next build` in benchmarks/next-small",
  );
}

// Record entry into the real `handle`, keyed by the request object itself so
// concurrent dispatches cannot be confused for one another.
//
// In this adapter a bypass is impossible by construction -- we call `handle`
// ourselves. The guard is here for the refactor that switches to the generated
// `handler(req, res, ctx)` entry, which CAN bypass it, and which is exactly
// the shape Perry's own fixture had to guard. Cheap now, load-bearing later.
const entered = new WeakSet<object>();
const realHandle = routeModule.handle.bind(routeModule);
routeModule.handle = (request: object, context: unknown) => {
  entered.add(request);
  return realHandle(request, context);
};

/** The minimal context `AppRouteRouteModule.handle` reads. Verified against
 *  the compiled runtime rather than guessed: it needs `renderOpts` and
 *  `sharedContext.{buildId,deploymentId}`, and Next fills the rest. */
function routeContext() {
  return {
    params: {},
    prerenderManifest: {
      version: 4,
      routes: {},
      dynamicRoutes: {},
      notFoundRoutes: [],
      preview: {
        previewModeId: "coop-benchmark",
        previewModeSigningKey: "coop-benchmark",
        previewModeEncryptionKey: "coop-benchmark",
      },
    },
    renderOpts: {
      supportsDynamicResponse: true,
      experimental: { dynamicIO: false, authInterrupts: false },
    },
    sharedContext: { buildId: "coop-bench", deploymentId: "coop" },
  };
}

async function executeNextRoute(
  rawMethod: string,
  path: string,
  query: string,
  scheme: string,
  host: string,
  headers: Record<string, string>,
  requestBody: Buffer,
): Promise<{ status: number; headers: [string, string][]; body: Buffer }> {
  const url = `${scheme}://${host}${path}${query === "" ? "" : `?${query}`}`;
  const method = rawMethod.toUpperCase();
  const init: RequestInit & { duplex?: "half" } = { method, headers };

  if (method !== "GET" && method !== "HEAD" && requestBody.length !== 0) {
    init.body = requestBody;
    init.duplex = "half";
  }

  const request = new NextRequest(url, init);
  const response = await routeModule.handle(request, routeContext());

  if (!entered.has(request)) {
    throw new Error("request bypassed AppRouteRouteModule.handle");
  }

  // Refuse to invent a response. Reporting a synthetic 200 here is exactly how
  // the previous version made its own regressions invisible.
  if (response === undefined || response === null) {
    throw new Error("AppRouteRouteModule.handle returned no Response");
  }
  const status = response.status;
  if (typeof status !== "number" || status === 0) {
    throw new Error(`handle returned a Response with no usable status: ${status}`);
  }

  const collected: [string, string][] = [];
  response.headers.forEach((value: string, name: string) => {
    collected.push([name, value]);
  });

  const text = await response.text();
  return { status, headers: collected, body: Buffer.from(text) };
}

type FrameCursor = { frame: Buffer; offset: number };

function readU32(cursor: FrameCursor): number {
  const value = cursor.frame.readUInt32BE(cursor.offset);
  cursor.offset += 4;
  return value;
}

function readText(cursor: FrameCursor): string {
  const length = readU32(cursor);
  const value = cursor.frame.toString("utf8", cursor.offset, cursor.offset + length);
  cursor.offset += length;
  return value;
}

/** Required compact COOP application-library ABI. */
export async function handle(frame: Buffer): Promise<Buffer> {
  if (
    frame.length < 5 ||
    frame[0] !== 0x43 ||
    frame[1] !== 0x4f ||
    frame[2] !== 0x4f ||
    frame[3] !== 0x50 ||
    frame[4] !== 1
  ) {
    throw new Error("invalid Coop HTTP frame");
  }

  const cursor: FrameCursor = { frame, offset: 5 };
  const method = readText(cursor);
  const path = readText(cursor);
  const query = readText(cursor);
  readText(cursor); // remote address is not used by this route
  const scheme = readText(cursor);
  const host = readText(cursor);
  const headerCount = readU32(cursor);
  const headers: Record<string, string> = {};
  for (let index = 0; index < headerCount; index += 1) {
    headers[readText(cursor)] = readText(cursor);
  }
  const bodyLength = readU32(cursor);
  const requestBody = frame.subarray(cursor.offset, cursor.offset + bodyLength);

  const result = await executeNextRoute(
    method,
    path,
    query,
    scheme,
    host,
    headers,
    requestBody,
  );

  let size = 5 + 2 + 4 + 4 + result.body.length;
  const encodedHeaders: [Buffer, Buffer][] = [];
  for (const [name, value] of result.headers) {
    const nameBytes = Buffer.from(name);
    const valueBytes = Buffer.from(value);
    encodedHeaders.push([nameBytes, valueBytes]);
    size += 4 + nameBytes.length + 4 + valueBytes.length;
  }

  const output = Buffer.alloc(size);
  output[0] = 0x43;
  output[1] = 0x4f;
  output[2] = 0x4f;
  output[3] = 0x50;
  output[4] = 2;
  let offset = 5;
  output.writeUInt16BE(result.status, offset);
  offset += 2;
  output.writeUInt32BE(encodedHeaders.length, offset);
  offset += 4;
  for (const [nameBytes, valueBytes] of encodedHeaders) {
    output.writeUInt32BE(nameBytes.length, offset);
    offset += 4;
    nameBytes.copy(output, offset);
    offset += nameBytes.length;
    output.writeUInt32BE(valueBytes.length, offset);
    offset += 4;
    valueBytes.copy(output, offset);
    offset += valueBytes.length;
  }
  output.writeUInt32BE(result.body.length, offset);
  offset += 4;
  result.body.copy(output, offset);
  return output;
}
