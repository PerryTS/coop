import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  output: "standalone",
  // Emit ONE server chunk. Next's server bundles otherwise load chunks with a
  // computed require -- webpack: `require("./chunks/" + id + ".js")`,
  // turbopack: `require(path.resolve(RUNTIME_ROOT, chunkPath))` -- and an
  // ahead-of-time compiler cannot statically resolve either, so the chunk
  // never lands in the binary and the route fails at first dispatch.
  //
  // Disabling server-side splitting makes the route self-contained, which is
  // what an AOT host needs. It is also closer to what a serverless bundle
  // looks like, so it is not an exotic configuration.
  webpack: (config, { isServer }) => {
    if (isServer) {
      config.optimization.splitChunks = false;
      config.optimization.runtimeChunk = false;
    }
    return config;
  },
};

export default nextConfig;
