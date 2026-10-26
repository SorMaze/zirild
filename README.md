# cargo-zirild

[GitHub repository](https://github.com/SorMaze/zirild)

`cargo-zirild` is a Cargo subcommand for building, checking, testing, and
running Rust projects for a selected target. It creates temporary tool wrappers
for Rust linking and native C/C++ dependencies, so projects do not need a
custom `.cargo/config.toml` or manually configured `CC`, `CXX`, linker, `AR`,
or `dlltool` variables. The executable has no third-party Rust dependencies.

The long-term goal is one cross-platform final-link path driven by Zig. Rust
source is still compiled by `rustc`; Zirild controls the native compiler,
archiver, and linker selected around Cargo.

## Toolchain policy

| Target or mode | C/C++ and archives | Final linker | Current status |
| --- | --- | --- | --- |
| GNU-family targets | `zig cc`, `zig c++`, `zig ar` | Zig | primary path |
| Linux musl targets | `zig cc`, `zig c++`, `zig ar` | Zig | supported; Zig's bundled musl CRT replaces rustc's self-contained CRT |
| Windows GNU raw-dylib | Zig tools and `zig dlltool` | Zig | supported |
| Windows MSVC | system MSVC tools | system MSVC linker | compatibility exception |
| Android, default mode | Zig tools | Zig | `check` works; Android libc linking is currently unavailable in Zig |
| Android with `-ndkfallback` | NDK Clang and LLVM ar | NDK LLD | explicit fallback; not Zig-linked |

Zirild never switches an Android build to NDK Clang/LLD silently. The fallback
requires `-ndkfallback` and emits a prominent build warning.

## Requirements

- A Rust toolchain with the target installed: `rustup target add <triple>` for
  every target you build.
- A Zig installation that provides `zig cc`, `zig c++`, and `zig ar`, and that
  can cross-compile to the selected target.

Zig versions differ in the C/C++ driver and LLD arguments Zirild has to work
around, so the version in use matters. The support matrix above was proven
against Zig 0.16.0, the newest tagged release at that time, and Zirild does not
impose a minimum-version check. Every build log therefore starts with the
toolchain it resolved:

```text
+ cargo build --target x86_64-unknown-linux-musl (Zig 0.16.0 / x86_64-linux-musl / ReleaseSafe)
```

If a build fails on a different Zig release, report it with that banner.

## Install

```powershell
cargo install cargo-zirild
```

Set `ZIG_HOME` to the Zig installation directory or directly to `zig`/`zig.exe`:

```powershell
$env:ZIG_HOME = 'C:\tools\zig'
```

Zig lookup order is:

1. `--zig-path=<path>`;
2. `CARGO_ZIRILD_ZIG_PATH`, `ZIG`, `ZIG_HOME`, then legacy `Zig_home`;
3. `zig` from `PATH`.

The legacy `-zigpatch=<path>` spelling remains accepted. Zirild validates the
selected executable with `zig version` before Cargo starts.

The selected Rust target must also be installed in the active Rust toolchain:

```powershell
rustup target add x86_64-pc-windows-gnu
```

## Quick start

```powershell
# build is the default Cargo command
cargo zirild -target=x86_64-windows-gnu

# Build a Linux GNU target using an explicit Zig executable.
cargo zirild -target=x86_64-unknown-linux-gnu --zig-path=D:\tools\zig\zig.exe

# Build and run a native Windows MSVC target.
cargo zirild -target=x86_64-pc-windows-msvc run

# Pass arguments to the produced executable.
cargo zirild -target=x86_64-pc-windows-msvc run -- --example-argument
```

Both Zig target names and recognized Rust target triples are accepted. For
example, `x86_64-windows-gnu` maps to `x86_64-pc-windows-gnu`, while the short
`x86_64-pc-windows` form selects `x86_64-pc-windows-msvc`.

Do not pass Cargo's `--target`; Zirild's `-target` is the single source of truth
and is converted to the corresponding Rust target triple.

## Build takeover

Two commands write a managed block into `.cargo/config.toml` so that a plain
`cargo build` goes through Zig without naming a target or setting `CC`:

```powershell
# A plain `cargo build` becomes a full Zig cross build: target, C/C++, and link.
cargo zirild build-on -target=x86_64-unknown-linux-gnu

# Only C/C++ and ar go through Zig. Cargo keeps its own target and linker.
# Without -target, the host target is inferred from `rustc -vV`.
cargo zirild cxx-on

cargo zirild build-off   # or cxx-off
```

The block is delimited and Zirild owns it: re-running replaces it, and the off
command removes it, deleting the file when the block was all it contained. If
the file already defines a section the block needs (`[env]`, or `[build]` and
`[target.<triple>]` for `build-on`), Zirild refuses and leaves the file
untouched, because a duplicate table would make Cargo reject the entire file.

The configuration is **not meant to be committed**: it records absolute wrapper
paths for this machine, under `%LOCALAPPDATA%\cargo-zirild\wrappers\<version>`
(on Unix, `$XDG_DATA_HOME` or `~/.local/share`). Run the command once per
machine. Two limits follow from the design:

- After `build-on`, `cargo run` cannot execute the cross-compiled result.
- `cxx-on` is not enough for musl targets, where the final link must also be
  Zig's; use `build-on` for those.

`build-on` rejects MSVC and Android targets. MSVC keeps its own toolchain by
policy, and Android needs NDK selection on every invocation. The reasoning and
the measurements behind it are recorded in
[`docs/decisions/0001-build-takeover.md`](docs/decisions/0001-build-takeover.md).

## Target triple support

The tables below distinguish real end-to-end validation from code paths that
only appear compatible. A target in the theoretical table still requires its
Rust standard library from `rustup target add`, and may require target system
libraries or an SDK. Unless a target appears in the verified or theoretical
table, do not assume that the current release supports it.

### Verified and supported

These targets have completed real mixed Rust/C/C++ builds. Runnable host or WSL
outputs were executed; Android outputs were inspected but not run on a device.

| Rust target triple | Native and final-link path | Verified result |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | Zig | ELF built and ran in Ubuntu on WSL2 |
| `x86_64-unknown-linux-musl` | Zig | statically linked PIE built and ran in WSL2 Linux |
| `x86_64-pc-windows-gnu` | Zig | PE built and ran on Windows |
| `x86_64-pc-windows-msvc` | system MSVC compatibility path | PE built and ran on Windows; final link is not Zig |
| `aarch64-linux-android` | explicit `-ndkfallback` | NDK Clang/LLD produced an ELF64 AArch64 PIE; device execution is unverified |
| `x86_64-linux-android` | explicit `-ndkfallback` | NDK Clang/LLD produced an ELF64 x86-64 PIE; device execution is unverified |

### Theoretically supported, not end-to-end validated

For the Zig rows below, Zirild's Rust-to-Zig mapping is defined and the current
Zig toolchain accepted a native C object compilation for the mapped target.
That probe does not prove Rust final linking, C++ runtime availability, or
execution on the destination system.

| Rust target triple or triples | Expected path | Current evidence boundary |
| --- | --- | --- |
| `aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl` | Zig | target mapping and native C compilation only |
| `arm-unknown-linux-gnueabi`, `arm-unknown-linux-gnueabihf`, `arm-unknown-linux-musleabi`, `arm-unknown-linux-musleabihf` | Zig | target mapping and native C compilation only |
| `loongarch64-unknown-linux-gnu`, `loongarch64-unknown-linux-musl` | Zig | target mapping and native C compilation only |
| `powerpc-unknown-linux-gnu` | Zig | target mapping and native C compilation only |
| `powerpc64-unknown-linux-gnu`, `powerpc64-unknown-linux-musl` | Zig | target mapping and native C compilation only |
| `powerpc64le-unknown-linux-gnu`, `powerpc64le-unknown-linux-musl` | Zig | target mapping and native C compilation only |
| `s390x-unknown-linux-gnu`, `sparc64-unknown-linux-gnu` | Zig | target mapping and native C compilation only |
| `aarch64-apple-darwin`, `x86_64-apple-darwin` | Zig | basic target mapping and native C compilation only; Apple SDK/framework projects are not covered |
| `i686-pc-windows-msvc`, `aarch64-pc-windows-msvc`, `arm64ec-pc-windows-msvc` | system MSVC compatibility path | orchestration path exists, but the required cross-MSVC tools and outputs were not validated |
| `armv7-linux-androideabi`, `i686-linux-android` | explicit `-ndkfallback` | NDK compiler/ABI mapping exists, but no complete artifact was validated |

### Currently unsupported by the implemented path

| Rust target triple or family | Why it is not currently supported |
| --- | --- |
| `aarch64-linux-android`, `armv7-linux-androideabi`, `i686-linux-android`, and `x86_64-linux-android` without `-ndkfallback` | strict Zig mode can run `check`, but the current Zig distribution cannot final-link Android libc |
| `arm-linux-androideabi`, `thumbv7neon-linux-androideabi` | not included in Zirild's explicit Android target and NDK compiler mapping |
| `i586-unknown-linux-*`, `i686-unknown-linux-*` | Rust architecture names are not yet normalized to Zig's `x86` architecture spelling |
| `armv5te-unknown-linux-*`, `armv7-unknown-linux-*`, `thumbv7neon-unknown-linux-*` | CPU-specific Rust architecture names are not yet normalized to Zig's `arm`/`thumb` target model |
| `riscv64a23-unknown-linux-*`, `riscv64gc-unknown-linux-*` | Rust architecture/profile names are not yet normalized to Zig's `riscv64` target plus CPU features |
| `i686-pc-windows-gnu`, `*-pc-windows-gnullvm` | the implemented Windows Zig-link and runtime rewrite is validated only for x86-64 GNU; `gnullvm` is not mapped to a Zig ABI |
| `*-apple-ios`, `*-apple-tvos`, `*-apple-watchos`, `*-apple-visionos` | no Apple mobile target or SDK integration path is implemented |
| `*-unknown-freebsd`, `*-unknown-netbsd`, `*-pc-solaris`, `*-unknown-illumos`, `*-unknown-linux-ohos` | no Rust-vendor-to-Zig target conversion and target SDK/sysroot policy is implemented |
| `wasm32-*`, `*-unknown-uefi`, `*-unknown-none*`, CUDA, and other bare-metal or accelerator targets | they require target-specific link semantics rather than Zirild's current hosted C/C++ linker-wrapper path |
| custom target JSON and minimum-glibc-suffixed targets | no separate Cargo-target/Zig-target model exists for these forms yet |

## Command reference

```text
cargo zirild -target=<zig-target> [zirild options] [cargo command] [cargo options]
```

Supported Cargo commands are `build` (default), `check`, `run`, `test`,
`bench`, `rustc`, `clippy`, `doc`, and `asm` (dispatched to the installed
cargo-show-asm plugin with Zirild's wrapper environment, so
`cargo zirild -target=<target> asm <function>` inspects the cross-compiled
code; `-target` is forwarded as cargo-asm's `--target` unless the asm
arguments provide one). Other arguments, including `--package`,
`--bin`, `--features`, `--locked`, and arguments after `--`, are passed to the
selected Cargo command.

When selecting a command explicitly, place it before its Cargo options:

```powershell
cargo zirild -target=x86_64-pc-windows-msvc run --release -- --example-argument
```

After the command, Zirild no longer guesses whether an argument value such as
`run` is another command. If the command is omitted, the first non-Zirild
argument starts the default `build` arguments. Cargo's `-Z <feature>` is passed
through unchanged; Zirild does not reserve `-Z`.

### Takeover commands

These replace the build entry point instead of running Cargo. They accept no
options beyond `-target` and `--zig-opt`.

| Command | Meaning |
| --- | --- |
| `build-on -target=<target>` | write `[build] target`, `[env]`, and `[target.<triple>] linker` so a plain `cargo build` is a full Zig build |
| `cxx-on [-target=<target>]` | write only `[env]`, routing C/C++ and `ar` through Zig |
| `build-off` | remove the managed block written by `build-on` |
| `cxx-off` | remove the managed block written by `cxx-on` |

`--zig-opt=<mode>` is accepted by the `-on` commands and becomes the
optimisation mode recorded in the environment. A wrapped build cannot observe
Cargo's profile, so after takeover `cargo build --release` does not change the
Zig mode; set it when writing the block.

### General options

| Option | Meaning |
| --- | --- |
| `-target=<target>` | required Zig target or recognized Rust target triple |
| `--zig-path=<path>` | select a Zig directory or executable for this invocation |
| `--release` | Cargo release profile and Zig `ReleaseFast` |
| `--zig-release-small` | select Zig `ReleaseSmall` and force native `-Oz` |
| `--zig-opt=<mode>` | select `debug`, `safe`, `fast`, `small`, or a full Zig mode name |
| `--windows-runtime=<policy>` | Windows GNU runtime policy: `auto`, `zig`, or `preserve` |
| `--preserve-linker-args` | alias for `--windows-runtime=preserve` |
| `--trace` | print native wrapper invocation details |
| `-h`, `--help`, `help`, `?` | show command help |
| `-V`, `--version` | print the cargo-zirild version |

The `help` and `?` spellings must precede the Cargo command, like the other
Zirild options; quote `?` in shells that expand it as a wildcard.

The legacy `-zigpatch=<path>` and `-ZigOptimize=<mode>` spellings remain
accepted for compatibility.

The default link policy is Zig `ReleaseSafe`; Cargo `--release` maps the link
policy to `ReleaseFast`. Native C/C++ compilation normally keeps the
optimization flags selected by `cc-rs`, CMake, or another build helper from
Cargo's profile environment. An explicit `--zig-opt` or
`--zig-release-small` removes conflicting native `-O*` arguments and appends
the selected `-O0`, `-O2`, `-O3`, or `-Oz` flag last. Rust crates always retain
Cargo's normal profile semantics.

## Android NDK

Set `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT`, or legacy `Ndk_home` to either:

- one NDK installation; or
- the Android SDK `ndk` directory containing version subdirectories.

When multiple valid NDK versions exist, Zirild selects the newest version. It
prints the selected version and full path in every Android build log.

```powershell
$env:ANDROID_NDK_HOME = 'D:\Android\Sdk\ndk'

# Use the newest installed NDK.
cargo zirild -target=x86_64-linux-android check

# Select an exact installed version.
cargo zirild -target=x86_64-linux-android -ndkversion='27.2.12479018' check

# Select by newest-first index: 0 is newest, 1 is the next oldest.
cargo zirild -target=x86_64-linux-android -nindx=1 check
```

### Android options

| Option | Meaning |
| --- | --- |
| `--ndk-path=<path>` | select one NDK or an SDK `ndk` directory for this invocation |
| `-ndkversion=<version>` | select an exact version below the NDK search directory |
| `-nindx=<index>` | select from the newest-first version list |
| `-android-api=<level>` | select the Android API level |
| `-android-abi=<abi>` | validate the ABI, such as `arm64-v8a` or `x86_64` |
| `-android-stl=<name>` | export the requested NDK STL setting |
| `-android-cflag=<flag>` | pass one flag to the selected native compiler |
| `-android-link-arg=<flag>` | pass one flag to the selected native linker |
| `-ndkfallback` | explicitly use NDK Clang/LLD and LLVM ar instead of Zig |

CMake-style `-DANDROID_ABI=...`,
`-DANDROID_PLATFORM=android-<api>`, and `-DANDROID_STL=...` aliases are also
parsed in Android mode. Zirild exports their values for Cargo build scripts,
along with `ANDROID_NDK_ROOT` and `CMAKE_ANDROID_NDK`.

NDK lookup order is `--ndk-path`, `CARGO_ZIRILD_NDK_PATH`,
`ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT`, legacy `Ndk_home`, then the `ndk`
directory below `ANDROID_SDK_ROOT` or `ANDROID_HOME`.

Example native configuration:

```powershell
cargo zirild -target=x86_64-linux-android `
  -android-api=24 `
  -android-abi=x86_64 `
  -android-cflag=-fPIC `
  -android-link-arg=-Wl,--build-id `
  check
```

### Android fallback

The current Zig distribution cannot link Android libc even when an NDK sysroot
is available. Default Android mode therefore preserves the Zig-linker contract
and reports the limitation instead of silently producing a file with another
linker.

To produce an Android file with the installed NDK, opt in explicitly:

```powershell
cargo zirild -target=x86_64-linux-android -ndkfallback build
```

This switches C, C++, archive, and final-link steps to NDK Clang/LLD and LLVM
ar. The build log warns that the final output is not linked by Zig.

A suggested musl target is only a compatibility alternative: it produces a
self-contained Linux binary, not an Android application.

## Platform notes

### Linux GNU

Both target spellings below select the same Cargo target:

```powershell
rustup target add x86_64-unknown-linux-gnu
cargo zirild -target=x86_64-linux-gnu
cargo zirild -target=x86_64-unknown-linux-gnu
```

Zig supplies the GNU libc toolchain for ordinary Rust and native C/C++
dependencies. Applications using desktop libraries outside libc, such as DBus,
GTK, X11, or WebKit2GTK, additionally need a target sysroot containing those
libraries and their pkg-config metadata.

### Linux musl

Rust's musl targets use a self-contained CRT: rustc passes its own startup
objects plus `-nostartfiles` and expects the linker driver to add none. Zig's
cc driver ignores `-nostartfiles` and injects its bundled musl `crt1.o`
anyway, and the explicit `-lc` on rustc's line defeats `-nostdlib`, so Zirild
strips rustc's self-contained CRT objects from final musl links instead.
Response files that rustc produces for long link lines are rewritten into
filtered private copies. Zig's musl CRT and libc then stand alone, matching
the GNU-target model. Only final links are affected; C and C++ compilation is
unchanged.

For aarch64 final links, Zirild also removes rustc's Cortex-A53 workaround
switch (`-Wl,--fix-cortex-a53-843419`), which Zig's cc driver rejects as an
unsupported linker argument.

### Windows

Windows GNU uses Zig for C, C++, archives, raw-dylib import libraries, and final
linking. Zirild removes Rust's ignored `--enable/--disable-auto-image-base`
switches before Zig LLD is invoked. Linker response files are rewritten into a
private wrapper directory, and empty module-definition files are copied there
before a fallback export is added. Zirild never edits rustc's original `.rsp`
or `.def` inputs in place.

The default `--windows-runtime=auto` policy replaces the known Rust MinGW
runtime arguments with Zig's bundled runtime and adds `-lunwind`. It warns when
custom startup or entry-point arguments are detected. Use `zig` to acknowledge
and force that policy, or `preserve` for `no_std`, a custom CRT, custom startup
objects, or manually controlled unwind libraries. `preserve` disables runtime
argument removal and the automatic `-lunwind` addition.

Windows MSVC currently retains the system compiler, librarian, and linker for
ABI compatibility. It is a documented exception to the unified Zig-linker
goal.

## Validation

The fixture used for validation is [examples/mixed](examples/mixed): a standalone
crate with `cc-rs`, one C translation unit, one C++17 translation unit, and Rust
`extern "C"` calls into both. It is excluded from the published package and is
built directly:

```text
cd examples/mixed
cargo zirild -target=x86_64-pc-windows-gnu run
cargo zirild -target=x86_64-unknown-linux-gnu build
cargo zirild -target=x86_64-unknown-linux-musl build
```

Build traces confirmed the expected C, C++, archive, and final-link wrapper
modes. The runnable binaries exit with code 0 and report the expected interop
result: C returns 42 and C++ returns 43.

The current source was validated on 2026-07-14 against the temporary standalone
fixture that this repository example replaces. The exact targets and results are
listed under [Target triple support](#target-triple-support).

On 2026-09-20 the repository fixture was rebuilt with Zig 0.16.0 on Windows for
`x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`, and
`x86_64-unknown-linux-musl`. The MSVC binary ran and reported the expected
interop result; the GNU target produced a PIE ELF and the musl target produced a
statically linked ELF with no `PT_INTERP`.

Android artifacts were inspected with the selected NDK's `llvm-readelf`.
Android device execution has not yet been validated.

On 2026-09-06, the musl final-link path was validated with a mixed Rust, C,
and C++17 fixture built for `x86_64-unknown-linux-musl` through `cc-rs`; the
statically linked PIE ran on Linux with the expected C and C++ interop
results.

On 2026-07-15, Zig 0.17.0-dev.1282 accepted native C object compilation for
each Zig target spelling listed in the theoretical table. This was a target
mapping probe, not a Rust/C/C++ final-link or runtime validation.

The latest reliability pass also verified that Windows linker source files
remain unchanged, temporary wrapper files are removed after a successful
build, Cargo nightly `-Z` arguments remain intact, and Android NDK discovery
works through `ANDROID_NDK_HOME`.

## Scope and current limits

Zirild is currently strongest as a Windows-hosted, native-dependency-aware
Cargo driver for Windows GNU and Linux targets. It is not yet a universal SDK
or build-system replacement.

- Windows MSVC is orchestrated by Zirild but compiled and linked by MSVC.
- Strict Zig Android mode can check code but cannot currently link Android libc;
  `-ndkfallback` is explicit NDK LLVM orchestration.
- Android device execution is not yet covered by the validation matrix.
- Minimum-glibc suffixes, Apple universal binaries, custom target JSON, CMake
  toolchain generation, bindgen sysroots, and pkg-config sysroot management are
  not yet first-class Zirild features.
- The mixed Rust/C/C++ fixture covers `cc-rs`, archives, and final linking, but
  does not claim exhaustive compatibility with OpenSSL, `ring`, large CMake
  projects, Tauri Android, or every custom-CRT/`no_std` layout.

## Output paths and execution

Artifacts use Cargo's normal locations:

- `target/<cargo-target>/debug/` by default;
- `target/<cargo-target>/release/` with `--release`.

`run` and `test` require the host to execute the selected target. Use `build`
or `check` for foreign targets unless a compatible runtime is available.

## License

This project is licensed under the BSD 2-Clause License.

Copyright (c) 2026 SorMaze.

Redistribution in source or binary form is permitted provided that the
copyright notice, conditions, and disclaimer in [LICENSE](LICENSE) are
retained.
