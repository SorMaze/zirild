# ADR 0001 — Build takeover through `.cargo/config.toml`

- **Status:** accepted
- **Date:** 2026-10-27
- **Target release:** 0.2.0

## Context

Today every Zirild build is opt-in and per-invocation:

```
cargo zirild -target=x86_64-unknown-linux-gnu build
```

Two things follow from that. First, the target must be repeated on every
command, which nobody wants to type. Second, and more importantly, anything that
runs Cargo directly — rust-analyzer, an IDE task, a teammate's `cargo build`, a
CI script — never sees Zirild at all. Those builds need their own `CC`, `CXX`,
`AR`, and linker configuration, which is exactly the manual setup Zirild exists
to remove.

The requested feature was a persistent switch, originally imagined as a marker
key in `Cargo.toml` (`zirild_build = true`) written by `cargo zirild build-on`.

## Decision

Ship two takeover modes, both written into `.cargo/config.toml` as a delimited
managed block that Zirild owns:

| Command | Takes over | Writes |
| --- | --- | --- |
| `cargo zirild build-on -target=<zig-target>` | target selection, native compilation, final link | `[build] target`, `[env]`, `[target.<triple>] linker` |
| `cargo zirild cxx-on [-target=<zig-target>]` | native compilation only | `[env]` |
| `cargo zirild build-off` / `cxx-off` | — | removes the block |

`build-on` makes a bare `cargo build` a full cross build through Zig.
`cxx-on` leaves Cargo's own target and linker alone and only routes C, C++, and
`ar` through Zig, so the host build still links with the system linker. When
`-target` is omitted, `cxx-on` infers the host target from `rustc -vV`.

`Cargo.toml` is **not** modified.

## Why not `Cargo.toml`

Cargo does not read manifest data to change its own toolchain. A marker key in
the manifest can only be read by Zirild itself, and Zirild cannot read it either
without gaining a TOML parser. Six properties were measured on 2026-10-27 before
settling on the design:

| Property | Result |
| --- | --- |
| `[env]` from `.cargo/config.toml` reaches build scripts | yes — `CC`/`CXX`/`AR` read back as set, `None` without the config |
| `[env]` reaches the linker child process | yes — a probe linker read `CARGO_ZIRILD_PROBE=sentinel` |
| `[env]` reaches an external Cargo subcommand | no — `cargo zirild` did not see `CARGO_ZIRILD_ZIG_PATH` |
| `CC = "cargo-zirild --wrapper=cc"` keeps its argument | no — cc-rs dropped it and ran `"cargo-zirild" "-E" …` |
| `[package] zirild_build = true` | warns `unused manifest key`, so even as metadata it is the wrong slot |
| Duplicate `[env]` tables in one config file | TOML rejects the whole file, which is why conflicts must be refused up front |

The third and fourth rows remove the two shortcuts. Zirild cannot read project
configuration through Cargo's environment, so a manifest marker would force a
TOML parser and break the crate's zero-dependency property. And because `CC`
cannot carry arguments, each role needs its own executable name rather than one
binary with a mode flag.

## Consequences

**Positive.**

- A bare `cargo build`, the IDE, and CI inherit Zig with no extra tooling.
- No TOML parser is needed. Writing is string assembly and removal is a
  delimited block cut, so the zero-dependency property holds.
- Wrapper paths are absolute, which is legitimate precisely because the config
  file is not committed.
- `build-off` is a complete undo: removing the block restores the previous
  behavior, and the file is deleted when the block was its only content.

**Negative and accepted.**

- `cargo run` cannot work after `build-on`, because `[build] target` makes the
  artifact a foreign binary. This is inherent to full takeover, not a defect.
- `cxx-on` cannot serve musl-style targets: the final link stays with rustc's
  own driver, which passes `-nostartfiles` and its bundled CRT objects, and Zig
  injects its own `crt1.o`. That combination conflicts on `_start`. Use
  `build-on` for those targets.
- `cargo build --target X` after `build-on` overrides `[build] target` and falls
  back to the system linker, because only one `[target.*]` entry was written.
- The wrapper binaries live outside the project, so upgrading cargo-zirild
  leaves the previous version's directory behind. Re-running `*-on` points the
  config at the new one.

## Environment precedence

Cargo's `[env]` entries do not replace a variable that is already in the
environment unless `force = true`. Measured with a probe variable: exported as
`from-env` and configured as `from-config` without `force`, a build script saw
`from-env`; adding `force = true` made it see `from-config`. A `CC` exported in
a shell would otherwise defeat the takeover silently, without any error.

The native compiler variables — `CC`, `CXX`, `AR`, `CARGO_TARGET_<T>_AR` — are
therefore written forced. Zirild's own `CARGO_ZIRILD_*` variables are written
unforced on purpose, so an explicit `cargo zirild -target=...` still wins over
the recorded default instead of being overwritten by the config file.

## Conflict policy

`.cargo/config.toml` is a file Zirild does not own. The rules are deliberately
conservative:

1. No file — create it containing only the block.
2. Existing block — replace it wholesale, so re-running is idempotent.
3. No block, but a section the block needs already exists (`[env]` always;
   `[build]` and `[target.<triple>]` for `build-on`) — refuse and name the
   section. Appending would produce a duplicate table and Cargo would refuse the
   entire file, turning a configuration mistake into an unbuildable project.
4. `*-off` only removes the block. It refuses when no block is present, and it
   refuses when the block belongs to the other mode, so a typo cannot silently
   tear down the wrong takeover.

## Alternatives considered

- **A `[alias]` in the config** — still requires typing a different command, so
  IDEs and teammates keep bypassing Zirild. Rejected.
- **`[build] rustc-wrapper` only** — wraps rustc but runs after Cargo has
  already launched build scripts, so `CC` would not be in place in time.
  Rejected as a substitute; `[env]` is the only channel that runs early enough.
- **A `build.rs` shim** — cannot influence its own compilation environment.
  Rejected.
- **A committed config with relative wrapper paths** — requires per-project
  wrapper copies and depends on how Cargo resolves `linker` paths, which is not
  worth the fragility. Rejected in favor of uncommitted absolute paths.

## Future work

Multiple simultaneous targets would need one `[target.<triple>]` entry per
target and a way to select between them; the single-block design deliberately
does not attempt this. Android takeover is also out of scope for 0.2.0, since it
needs NDK paths in the environment and the NDK selection is currently validated
per invocation.
