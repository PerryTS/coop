import { NextRequest, NextResponse } from "next/server";

export const dynamic = "force-dynamic";

export function GET(_request: NextRequest) {
  // Keep the workload fixed so both servers execute exactly the same route
  // work.
  //
  // This used to be a workaround: the Perry pin of the day could not reliably
  // preserve an imported handler's request argument or NextURL.searchParams,
  // so query parsing had to stay out. #8036 lifted that, and the pin now
  // includes it. The fixed workload stays anyway, for the original reason
  // rather than the incidental one -- a benchmark whose work depends on the
  // request is not comparable run to run. Query parsing belongs in a
  // correctness fixture, not this one.
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
