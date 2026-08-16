# The TypeScript subset

Coop runs whatever [Perry](https://github.com/PerryTS/perry) compiles. That is a
large and growing subset of TypeScript, but it is a subset, and this is the
single biggest thing to establish before planning a migration.

## The rule of thumb

If it type-checks under `tsc`, that tells you nothing about whether it compiles
under Perry. Compile early, compile the real dependency tree, and do it before
designing around a library.

## What Coop additionally requires

Beyond Perry's own constraints, a Coop deployment must:

- **Export a `handle` function** from each file named in a `[[handlers]]` entry.
- **Take and return the ABI's type** — `string` for the `@coop/runtime` form,
  `Buffer` for the raw [`COOP`](host-abi.md) form. Explicitly string-typed
  handlers are rejected by the compiler when the binary ABI is expected.
- **Import only from the allowlist** — `@coop/runtime` and the Perry standard
  library surface. There is no route from application code to arbitrary native
  modules.

## Why the restriction exists

It is not incidental. Coop treats the compiler as its primary isolation
boundary (see [Architecture](architecture.md)). The absence of `eval`, dynamic
`Function` construction, and arbitrary native imports is what makes "application
code cannot reach outside the surface it was compiled for" a static property
rather than a hope.

Widening the subset therefore has a security dimension, not just a compatibility
one.

## Checking a package

Perry has tooling for auditing an npm package against its subset and adding it to
`perry.compilePackages`. If you need a dependency, start there rather than
vendoring it and hoping.

Expect the answer to be "no" for anything that:

- uses `eval` or builds functions from strings
- loads native addons
- depends on Node internals beyond the implemented stdlib surface
- relies on `Proxy` in ways Perry has not yet implemented

## Real-world calibration

A production Next.js App Route has been compiled and served under this model — 104
modules into a single application dylib, serving real requests through Next's own
`AppRouteRouteModule.handle`. That is a meaningful demonstration that the subset
reaches real frameworks.

It is not a claim that arbitrary Next.js applications work. Getting there
required fixing six distinct compiler and runtime defects, and the work is
tracked upstream in Perry. Treat framework support as something to verify for
your application, not to assume.
