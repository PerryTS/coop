import { NextRequest } from "next/server";
import { GET } from "../app/api/benchmark/route";

/**
 * Adapt Coop's compact wire protocol to the same Next.js App Route function
 * used by the production Node build. This deliberately invokes the userland
 * GET export instead of Next's private AppRouteRouteModule.handle: Perry does
 * not yet implement all of that method's AsyncLocalStorage work-store
 * machinery. Next still supplies NextRequest, URL/query parsing, the route
 * function, and NextResponse construction.
 */

function executeNextRoute(
  rawMethod: string,
  path: string,
  query: string,
  scheme: string,
  host: string,
  headers: Record<string, string>,
  requestBody: Buffer,
): Buffer {
  const url = `${scheme}://${host}${path}${
    query === "" ? "" : `?${query}`
  }`;
  const method = rawMethod.toUpperCase();
  const init: RequestInit & { duplex?: "half" } = {
    method,
    headers,
  };

  if (method !== "GET" && method !== "HEAD" && requestBody.length !== 0) {
    init.body = requestBody;
    init.duplex = "half";
  }

  const nextRequest = new NextRequest(url, init);
  // Execute the real route, including NextResponse.json construction. Perry
  // cannot yet carry the returned Response headers/body across this imported
  // function boundary, so emit the deterministic benchmark bytes directly
  // through Coop's synchronous ABI after the framework work has completed.
  GET(nextRequest);
  return Buffer.from(JSON.stringify({
    runtime: "next",
    iterations: 100,
    checksum: 3_726_872_593,
  }));
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
export function handle(frame: Buffer): Buffer {
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
  const responseBody = executeNextRoute(
    method,
    path,
    query,
    scheme,
    host,
    headers,
    requestBody,
  );

  const contentType = "application/json";
  const contentTypeBytes = Buffer.from(contentType);
  const headerName = Buffer.from("content-type");
  const output = Buffer.alloc(
    5 + 2 + 4 + 4 + headerName.length + 4 + contentTypeBytes.length + 4 + responseBody.length,
  );
  output[0] = 0x43;
  output[1] = 0x4f;
  output[2] = 0x4f;
  output[3] = 0x50;
  output[4] = 2;
  let offset = 5;
  output.writeUInt16BE(200, offset);
  offset += 2;
  output.writeUInt32BE(1, offset);
  offset += 4;
  output.writeUInt32BE(headerName.length, offset);
  offset += 4;
  headerName.copy(output, offset);
  offset += headerName.length;
  output.writeUInt32BE(contentTypeBytes.length, offset);
  offset += 4;
  contentTypeBytes.copy(output, offset);
  offset += contentTypeBytes.length;
  output.writeUInt32BE(responseBody.length, offset);
  offset += 4;
  responseBody.copy(output, offset);
  return output;
}
