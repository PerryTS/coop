import { NextRequest, NextResponse } from "next/server";

export const dynamic = "force-dynamic";

export function GET(_request: NextRequest) {
  // Keep the workload fixed so both servers execute exactly the same route
  // work. Perry 0.5.1503 does not yet reliably preserve an imported handler's
  // request argument or NextURL.searchParams, so query parsing is deliberately
  // outside this first server-efficiency benchmark.
  const iterations = 100;

  let checksum = 0x811c9dc5;
  for (let index = 0; index < iterations; index += 1) {
    checksum = Math.imul(checksum ^ index, 0x01000193) >>> 0;
  }

  const payload = {
    runtime: "next",
    iterations,
    checksum,
  };

  return NextResponse.json(payload);
}
