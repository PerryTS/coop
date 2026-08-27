# Quick start

## Prerequisites

- A **Perry** checkout. The commit Coop is built against is pinned in
  `perry-main.lock` at the repository root — the `commit` field, not the version
  string. Perry's version does not change on every commit, so it cannot tell you
  whether your checkout matches.
- The Rust toolchain named in `rust-toolchain.toml` at the repository root.
  It is a dated nightly, not stable: `perry-runtime` uses nightly-only float
  intrinsics, and because `.perry-main` is excluded from this workspace Perry's
  own toolchain file does not apply to builds run from here. rustup installs it
  on first use. Also LLVM 22, for Perry's in-process backend.
- Node, for the developer tooling and test fixtures.

## 1. Build the shared providers

```bash
./scripts/build-perry-libraries.sh
```

This is the step that makes Coop different from an ordinary Perry build. It
temporarily switches `perry-runtime` to `crate-type = ["dylib"]`, builds it with
the symbol-suppression `stdlib` feature, and links `coop-perry-stdlib-shared`
against it. The outputs are `.dylib`/`.so` files — **not** `.a` archives, so the
usual "rebuild the static wrapper crates" advice from Perry's own docs does not
apply here.

The script restores the manifest on exit via a trap, including on failure.

To confirm the build actually produced new artifacts, compare the provider
`sha256` and the manifest's recorded `perry_commit` — not file timestamps.

## 2. Build Coop

```bash
cargo build --release -p coop-daemon -p coop-worker -p coop-cli
```

## 3. Run an example

```bash
./target/release/coop-cli dev ./examples/landing
```

`dev` compiles the deployment and serves it locally. The first compile is slow —
Perry is doing real code generation — and subsequent ones are cached by content.

## 4. Write your own

A deployment is a directory containing `coop.toml` and your handlers:

```
myapp/
├── coop.toml
└── handlers/
    └── hello.ts
```

```toml
name = "myapp"
version = "0.1.0"

[hosts]
domains = ["myapp.test"]

[[handlers]]
file = "handlers/hello.ts"
path = "/hello"
method = "GET"
```

```ts
// handlers/hello.ts
import { CoopRequest, respond } from "@coop/runtime";

export function handle(reqJson: string): string {
  const req = new CoopRequest(reqJson);
  return respond(200, { "content-type": "text/plain" }, `hello from ${req.path}`);
}
```

Then:

```bash
./target/release/coop-cli dev ./myapp
curl -H 'Host: myapp.test' http://127.0.0.1:PORT/hello
```

## Troubleshooting

**The worker refuses to load the app.** You will see an ABI mismatch naming two
Perry identities. That is the provider guard working: the application was
compiled against a different runtime than the one loaded. Rebuild the providers
(step 1) and recompile the app. See [Shared runtime providers](providers.md).

**A handler compiles under `tsc` but not under Perry.** Perry implements a subset.
See [The TypeScript subset](subset.md).

**Tests appear to pass but nothing ran.** Several suites self-skip when fixtures
are missing, and a skip is an early `return` — indistinguishable from a pass in
the summary line. Run with `--nocapture` and grep for `skip:`.
