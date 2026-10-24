//! cargo-zirild: build a Cargo project through Zig's C/C++ and linker tools.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = r#"cargo-zirild - run Cargo commands with the Zig toolchain

Usage:
  cargo zirild -target=<zig-target> [options] [cargo-command] [cargo options]

Required:
  -target=<zig-target>        Zig target, e.g. x86_64-linux-musl

Zig optimisation:
  (default/dev)               ReleaseSafe
  --release                   ReleaseFast
  --zig-release-small         ReleaseSmall
  --zig-opt=<mode>            Explicit: debug/safe/fast/small or a full Zig mode name

Other options:
  --zig-path=<path>           Use this Zig executable/directory before environment lookup
  --ndk-path=<path>           Android: use this NDK or SDK ndk directory
  -ndkversion=<version>       Android: select an exact version below the NDK search path
  -nindx=<index>              Android: select NDK by newest-first index (0 = newest)
  -android-api=<level>        Android: use an Android API level for Zig's target
  -android-abi=<abi>          Android: validate ABI (arm64-v8a, x86_64, etc.)
  -android-stl=<name>         Android: export the requested NDK STL setting
  -android-cflag=<flag>       Android: pass one flag to the selected native compiler
  -android-link-arg=<flag>    Android: pass one flag to the selected native linker
  -ndkfallback                Android: explicitly use NDK Clang/LLD instead of Zig
  --windows-runtime=<policy>  Windows GNU: auto, zig, or preserve
  --preserve-linker-args      Alias for --windows-runtime=preserve
  --trace                     Print native wrapper invocation details
  -h, --help, help, ?        Show this help
  -V, --version               Print the cargo-zirild version

Cargo command:
  build (default), check, run, test, bench, rustc, clippy, doc, or asm

A bare help, or ?, prints this text instead of building. Like Zirild's options it
must precede the Cargo command. Quote ? in shells that expand it as a wildcard.

The Cargo command, when present, must precede its Cargo options. Arguments after
that command are passed to Cargo; Zirild only observes --release and rejects a
second Cargo --target. With no command, the first non-Zirild option starts the
default build's Cargo arguments. Cargo's -Z and its following value are
preserved. Zirild's -target is the single source of truth.

The asm command dispatches to the installed cargo-show-asm plugin with Zirild's
wrapper environment, so cargo zirild -target=<target> asm <function> inspects the
cross-compiled code. Zirild's -target is forwarded as cargo-asm's --target unless
the asm arguments already provide one.

Zig location:
  Lookup order: --zig-path, CARGO_ZIRILD_ZIG_PATH, ZIG, ZIG_HOME, Zig_home,
  then zig on PATH. -zigpatch remains accepted as a compatibility alias.

Android NDK location:
  Lookup includes --ndk-path, CARGO_ZIRILD_NDK_PATH, ANDROID_NDK_HOME,
  ANDROID_NDK_ROOT, Ndk_home, and the ndk directory below ANDROID_SDK_ROOT or
  ANDROID_HOME. Zirild never silently replaces Zig with NDK clang.
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WrapperMode {
    Cc,
    Cxx,
    Linker,
    Ar,
    Dlltool,
}

#[derive(Debug)]
struct BuildOptions {
    zig_target: String,
    cargo_target: String,
    optimize: String,
    force_native_optimize: bool,
    zig_patch: Option<PathBuf>,
    windows_runtime: WindowsRuntimePolicy,
    trace: bool,
    cargo_command: String,
    cargo_args: Vec<OsString>,
    android: AndroidOptions,
}

#[derive(Debug, Default)]
struct AndroidOptions {
    ndk_path: Option<PathBuf>,
    ndk_version: Option<String>,
    ndk_index: Option<usize>,
    api_level: Option<u32>,
    abi: Option<String>,
    stl: Option<String>,
    cflags: Vec<String>,
    link_args: Vec<String>,
    ndk_fallback: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum WindowsRuntimePolicy {
    #[default]
    Auto,
    Zig,
    Preserve,
}

impl WindowsRuntimePolicy {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "zig" => Ok(Self::Zig),
            "preserve" => Ok(Self::Preserve),
            _ => Err(format!(
                "invalid --windows-runtime '{value}'; use auto, zig, or preserve"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Zig => "zig",
            Self::Preserve => "preserve",
        }
    }

    fn rewrites_runtime(self) -> bool {
        !matches!(self, Self::Preserve)
    }
}

#[derive(Debug)]
struct AndroidNdk {
    root: PathBuf,
    version: String,
    sysroot: PathBuf,
    cc: PathBuf,
    cxx: PathBuf,
    ar: PathBuf,
}

fn main() -> ExitCode {
    if let Some(mode) = wrapper_mode() {
        return report(run_wrapper(mode));
    }
    report(run_driver(env::args_os().skip(1)))
}

fn report(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("cargo-zirild: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run_driver(args: impl IntoIterator<Item = OsString>) -> Result<(), String> {
    let mut args: Vec<_> = args.into_iter().collect();
    // Cargo passes the external subcommand name as argv[1], so
    // cargo zirild ... arrives as cargo-zirild zirild ....
    if args.first().is_some_and(|argument| argument == "zirild") {
        args.remove(0);
    }
    match driver_request(&args) {
        Some(DriverRequest::Help) => {
            print!("{USAGE}");
            return Ok(());
        }
        Some(DriverRequest::Version) => {
            println!("cargo-zirild {VERSION}");
            return Ok(());
        }
        None => {}
    }

    let options = parse_args(args)?;
    let android_ndk = if is_android_target(&options.cargo_target) {
        Some(android_ndk(&options.android, &options.cargo_target)?)
    } else {
        if options.android.is_configured() {
            return Err("Android-only options require an Android -target".into());
        }
        None
    };
    let zig = if options.android.ndk_fallback {
        None
    } else {
        Some(zig_executable(options.zig_patch.as_deref())?)
    };
    warn_about_android_target(
        &options.zig_target,
        android_ndk.as_ref(),
        options.android.ndk_fallback,
    );
    let wrappers = make_wrappers()?;
    let result = run_cargo(&options, zig.as_ref(), &wrappers, android_ndk.as_ref());
    let _ = fs::remove_dir_all(&wrappers.directory);
    result
}

/// A request Zirild must answer itself, before any Cargo work happens. Both are
/// only recognized ahead of the Cargo command, so `cargo zirild run -- --version`
/// still forwards `--version` to the produced executable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DriverRequest {
    Help,
    Version,
}

fn driver_request(arguments: &[OsString]) -> Option<DriverRequest> {
    for argument in arguments {
        let text = argument.to_string_lossy();
        if argument == "zirild" {
            continue;
        }
        if matches!(text.as_ref(), "--help" | "-h" | "help" | "?") {
            return Some(DriverRequest::Help);
        }
        if argument == "--version" || argument == "-V" {
            return Some(DriverRequest::Version);
        }
        if cargo_command_name(&text).is_some() || !is_zirild_option_spelling(&text) {
            return None;
        }
    }
    None
}

fn is_zirild_option_spelling(argument: &str) -> bool {
    argument == "--release"
        || argument == "--zig-release-small"
        || argument == "-ndkfallback"
        || argument == "--preserve-linker-args"
        || argument == "--trace"
        || argument.starts_with("-target=")
        || argument.starts_with("--target=")
        || argument.starts_with("--zig-opt=")
        || argument.starts_with("-ZigOptimize=")
        || argument.starts_with("--zig-path=")
        || argument.starts_with("-zigpatch=")
        || argument.starts_with("--ndk-path=")
        || argument.starts_with("-ndkversion=")
        || argument.starts_with("-nindx=")
        || argument.starts_with("-android-api=")
        || argument.starts_with("-android-abi=")
        || argument.starts_with("-android-stl=")
        || argument.starts_with("-android-cflag=")
        || argument.starts_with("-android-link-arg=")
        || argument.starts_with("-DANDROID_ABI=")
        || argument.starts_with("-DANDROID_PLATFORM=")
        || argument.starts_with("-DANDROID_STL=")
        || argument.starts_with("--windows-runtime=")
}

fn parse_args(args: Vec<OsString>) -> Result<BuildOptions, String> {
    let mut target = None;
    let mut explicit_optimize = None;
    let mut zig_patch = None;
    let mut release = false;
    let mut cargo_command = None;
    let mut cargo_args = Vec::new();
    let mut android = AndroidOptions::default();
    let mut cargo_phase = false;
    let mut executable_arguments = false;
    let mut windows_runtime = WindowsRuntimePolicy::default();
    let mut trace = false;

    for argument in args {
        let text = argument.to_string_lossy();
        if cargo_phase {
            if !executable_arguments
                && (argument == "--target"
                    || argument == "-target"
                    || text.starts_with("--target=")
                    || text.starts_with("-target="))
            {
                return Err(
                    "use Zirild's -target=<zig-target>, not Cargo's --target after the command"
                        .into(),
                );
            }
            if !executable_arguments && argument == "--release" {
                release = true;
            }
            if argument == "--" {
                executable_arguments = true;
            }
            cargo_args.push(argument);
            continue;
        }
        // Cargo may inject the external subcommand name before or after user
        // options, depending on the invoking Cargo version and platform.
        if argument == "zirild" && cargo_command.is_none() {
            continue;
        } else if let Some(value) = text.strip_prefix("-target=") {
            target = Some(value.to_owned());
        } else if let Some(value) = text.strip_prefix("--target=") {
            target = Some(value.to_owned());
        } else if let Some(value) = text
            .strip_prefix("--zig-opt=")
            .or_else(|| text.strip_prefix("-ZigOptimize="))
        {
            explicit_optimize = Some(validate_optimize(value)?);
        } else if argument == "--zig-release-small" {
            explicit_optimize = Some("ReleaseSmall".into());
        } else if let Some(value) = text
            .strip_prefix("--zig-path=")
            .or_else(|| text.strip_prefix("-zigpatch="))
        {
            if value.is_empty() {
                return Err("--zig-path cannot be empty".into());
            }
            zig_patch = Some(PathBuf::from(value));
        } else if let Some(value) = text.strip_prefix("--ndk-path=") {
            if value.is_empty() {
                return Err("--ndk-path cannot be empty".into());
            }
            android.ndk_path = Some(PathBuf::from(value));
        } else if let Some(value) = text.strip_prefix("-ndkversion=") {
            if value.is_empty() {
                return Err("-ndkversion cannot be empty".into());
            }
            android.ndk_version = Some(value.into());
        } else if let Some(value) = text.strip_prefix("-nindx=") {
            android.ndk_index = Some(value.parse().map_err(|_| {
                format!("invalid -nindx '{value}'; use a non-negative integer (0 = newest)")
            })?);
        } else if let Some(value) = text.strip_prefix("-android-api=") {
            set_android_api_level(&mut android, value)?;
        } else if let Some(value) = text.strip_prefix("-android-abi=") {
            if value.is_empty() {
                return Err("-android-abi cannot be empty".into());
            }
            android.abi = Some(value.into());
        } else if let Some(value) = text.strip_prefix("-android-stl=") {
            if value.is_empty() {
                return Err("-android-stl cannot be empty".into());
            }
            android.stl = Some(value.into());
        } else if let Some(value) = text.strip_prefix("-android-cflag=") {
            if value.is_empty() {
                return Err("-android-cflag cannot be empty".into());
            }
            android.cflags.push(value.into());
        } else if let Some(value) = text.strip_prefix("-android-link-arg=") {
            if value.is_empty() {
                return Err("-android-link-arg cannot be empty".into());
            }
            android.link_args.push(value.into());
        } else if argument == "-ndkfallback" {
            android.ndk_fallback = true;
        } else if let Some(value) = text.strip_prefix("--windows-runtime=") {
            windows_runtime = WindowsRuntimePolicy::parse(value)?;
        } else if argument == "--preserve-linker-args" {
            windows_runtime = WindowsRuntimePolicy::Preserve;
        } else if argument == "--trace" {
            trace = true;
        } else if let Some(value) = text.strip_prefix("-DANDROID_ABI=") {
            if value.is_empty() {
                return Err("-DANDROID_ABI cannot be empty".into());
            }
            android.abi = Some(value.into());
        } else if let Some(value) = text.strip_prefix("-DANDROID_PLATFORM=") {
            set_android_api_level(&mut android, value)?;
        } else if let Some(value) = text.strip_prefix("-DANDROID_STL=") {
            if value.is_empty() {
                return Err("-DANDROID_STL cannot be empty".into());
            }
            android.stl = Some(value.into());
        } else if argument == "--release" {
            release = true;
            cargo_args.push(argument);
        } else if argument == "--" {
            cargo_phase = true;
            executable_arguments = true;
            cargo_args.push(argument);
        } else if argument == "--target" || argument == "-target" {
            return Err("use -target=<zig-target>, not a separate Cargo --target".into());
        } else if cargo_command.is_none()
            && let Some(command) = cargo_command_name(&text)
        {
            cargo_command = Some(command.to_owned());
            cargo_phase = true;
        } else {
            // The first non-Zirild argument starts Cargo's argument phase for
            // the default build. Do not guess whether later values happen to
            // spell a Cargo command name.
            cargo_phase = true;
            cargo_args.push(argument);
        }
    }

    let target = target.ok_or_else(|| "missing required -target=<zig-target>".to_owned())?;
    if target.is_empty() {
        return Err("-target cannot be empty".into());
    }
    let force_native_optimize = explicit_optimize.is_some();
    let optimize = explicit_optimize.unwrap_or_else(|| {
        if release {
            "ReleaseFast".into()
        } else {
            "ReleaseSafe".into()
        }
    });
    let cargo_target = cargo_target_for(&target);
    let mut zig_target = zig_target_for(&target);
    if windows_runtime != WindowsRuntimePolicy::Auto && !cargo_target.ends_with("-pc-windows-gnu") {
        return Err("--windows-runtime is only valid for a Windows GNU -target".into());
    }
    if is_android_target(&cargo_target) {
        validate_android_options(&android, &cargo_target)?;
        if let Some(api_level) = android.api_level {
            zig_target.push('.');
            zig_target.push_str(&api_level.to_string());
        }
    }

    Ok(BuildOptions {
        zig_target,
        cargo_target,
        optimize,
        force_native_optimize,
        zig_patch,
        windows_runtime,
        trace,
        cargo_command: cargo_command.unwrap_or_else(|| "build".into()),
        cargo_args,
        android,
    })
}

impl AndroidOptions {
    fn is_configured(&self) -> bool {
        self.ndk_path.is_some()
            || self.ndk_version.is_some()
            || self.ndk_index.is_some()
            || self.api_level.is_some()
            || self.abi.is_some()
            || self.stl.is_some()
            || !self.cflags.is_empty()
            || !self.link_args.is_empty()
            || self.ndk_fallback
    }
}

fn set_android_api_level(android: &mut AndroidOptions, value: &str) -> Result<(), String> {
    let value = value.strip_prefix("android-").unwrap_or(value);
    let api_level = value
        .parse::<u32>()
        .map_err(|_| format!("invalid Android API level '{value}'"))?;
    if api_level == 0 {
        return Err("Android API level must be greater than zero".into());
    }
    if let Some(previous) = android.api_level
        && previous != api_level
    {
        return Err(format!(
            "conflicting Android API levels: android-{previous} and android-{api_level}"
        ));
    }
    android.api_level = Some(api_level);
    Ok(())
}

fn validate_android_options(android: &AndroidOptions, cargo_target: &str) -> Result<(), String> {
    if android.ndk_version.is_some() && android.ndk_index.is_some() {
        return Err("use either -ndkversion or -nindx, not both".into());
    }
    if let Some(abi) = &android.abi
        && let Some(expected) = android_abi_for_target(cargo_target)
        && abi != expected
    {
        return Err(format!(
            "Android ABI '{abi}' does not match target {cargo_target}; use {expected}"
        ));
    }
    Ok(())
}

fn cargo_command_name(argument: &str) -> Option<&'static str> {
    match argument {
        "build" => Some("build"),
        "check" => Some("check"),
        "run" => Some("run"),
        "test" => Some("test"),
        "bench" => Some("bench"),
        "rustc" => Some("rustc"),
        "clippy" => Some("clippy"),
        "doc" => Some("doc"),
        "asm" => Some("asm"),
        _ => None,
    }
}

fn validate_optimize(value: &str) -> Result<String, String> {
    match value {
        "Debug" | "ReleaseSafe" | "ReleaseFast" | "ReleaseSmall" => Ok(value.into()),
        "debug" => Ok("Debug".into()),
        "safe" | "release-safe" => Ok("ReleaseSafe".into()),
        "fast" | "release-fast" => Ok("ReleaseFast".into()),
        "small" | "release-small" => Ok("ReleaseSmall".into()),
        _ => Err(format!(
            "invalid --zig-opt '{value}'; use debug, safe, fast, small, or a Zig mode name"
        )),
    }
}

/// A validated Zig installation. `verify_zig` already runs `zig version`, so the
/// reported version travels with the executable instead of being discarded: Zig
/// C/C++ and LLD argument handling changes between releases, and a build log
/// that omits the version cannot be compared against the documented validation
/// record.
#[derive(Debug)]
struct ZigToolchain {
    path: PathBuf,
    version: String,
}

fn zig_executable(zig_patch: Option<&Path>) -> Result<ZigToolchain, String> {
    if let Some(path) = zig_patch {
        return validate_zig_path(path, "--zig-path");
    }

    for name in ["CARGO_ZIRILD_ZIG_PATH", "ZIG", "ZIG_HOME", "Zig_home"] {
        if let Some(path) = env::var_os(name).filter(|value| !value.is_empty()) {
            return validate_zig_path(&PathBuf::from(path), name);
        }
    }
    if let Some(path) = executable_on_path("zig") {
        return verify_zig(&path, "PATH");
    }
    Err(
        "Zig was not found; pass --zig-path, set CARGO_ZIRILD_ZIG_PATH/ZIG/ZIG_HOME, or add zig to PATH"
            .into(),
    )
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    let executable_names: Vec<OsString> = if cfg!(windows) {
        vec![format!("{name}.exe").into(), name.into()]
    } else {
        vec![name.into()]
    };
    env::split_paths(&path).find_map(|directory| {
        executable_names
            .iter()
            .map(|executable| directory.join(executable))
            .find(|candidate| candidate.is_file())
    })
}

fn validate_zig_path(path: &Path, source: &str) -> Result<ZigToolchain, String> {
    if path.is_file() {
        return verify_zig(path, source);
    }
    if !path.is_dir() {
        return Err(format!("{source} path does not exist: {}", path.display()));
    }

    let names: &[&str] = if cfg!(windows) {
        &["zig.exe", "zig"]
    } else {
        &["zig"]
    };
    let executable = names
        .iter()
        .map(|name| path.join(name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            format!(
                "could not find {} in {} directory '{}'",
                names[0],
                source,
                path.display()
            )
        })?;
    verify_zig(&executable, source)
}

fn verify_zig(executable: &Path, source: &str) -> Result<ZigToolchain, String> {
    let output = Command::new(executable)
        .arg("version")
        .output()
        .map_err(|err| format!("{source} does not point to an executable Zig: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "{source} points to '{}', but Zig version validation failed with {}",
            executable.display(),
            output.status
        ));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if version.is_empty() {
        return Err(format!(
            "{source} points to '{}', but 'zig version' reported no version",
            executable.display()
        ));
    }
    Ok(ZigToolchain {
        path: executable.into(),
        version,
    })
}

fn is_android_target(target: &str) -> bool {
    matches!(
        target,
        "aarch64-linux-android"
            | "armv7-linux-androideabi"
            | "i686-linux-android"
            | "x86_64-linux-android"
    )
}

fn android_abi_for_target(target: &str) -> Option<&'static str> {
    match target {
        "aarch64-linux-android" => Some("arm64-v8a"),
        "armv7-linux-androideabi" => Some("armeabi-v7a"),
        "i686-linux-android" => Some("x86"),
        "x86_64-linux-android" => Some("x86_64"),
        _ => None,
    }
}

fn android_ndk(options: &AndroidOptions, cargo_target: &str) -> Result<AndroidNdk, String> {
    let (home, source) = android_ndk_search_path(options)?;
    let root = select_ndk_root(&home, options, source)?;
    let version = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("direct")
        .to_owned();
    let sysroot = root
        .join("toolchains")
        .join("llvm")
        .join("prebuilt")
        .join(ndk_host_tag())
        .join("sysroot");
    if !sysroot.is_dir() {
        return Err(format!(
            "Android NDK '{}' has no sysroot for host '{}': {}",
            root.display(),
            ndk_host_tag(),
            sysroot.display()
        ));
    }
    let bin = root
        .join("toolchains")
        .join("llvm")
        .join("prebuilt")
        .join(ndk_host_tag())
        .join("bin");
    let (compiler_prefix, default_api_level) = android_compiler_prefix(cargo_target)?;
    let api_level = options.api_level.unwrap_or(default_api_level);
    let executable_extension = if cfg!(windows) { ".cmd" } else { "" };
    let cc = bin.join(format!(
        "{compiler_prefix}{api_level}-clang{executable_extension}"
    ));
    let cxx = bin.join(format!(
        "{compiler_prefix}{api_level}-clang++{executable_extension}"
    ));
    let ar = bin.join(if cfg!(windows) {
        "llvm-ar.exe"
    } else {
        "llvm-ar"
    });
    if options.ndk_fallback {
        for (name, tool) in [
            ("C compiler", &cc),
            ("C++ compiler", &cxx),
            ("archiver", &ar),
        ] {
            if !tool.is_file() {
                return Err(format!(
                    "Android NDK '{}' does not provide the {name} needed for {cargo_target}: {}",
                    root.display(),
                    tool.display()
                ));
            }
        }
    }
    Ok(AndroidNdk {
        root,
        version,
        sysroot,
        cc,
        cxx,
        ar,
    })
}

fn android_ndk_search_path(options: &AndroidOptions) -> Result<(PathBuf, &'static str), String> {
    if let Some(path) = &options.ndk_path {
        return Ok((path.clone(), "--ndk-path"));
    }
    for name in [
        "CARGO_ZIRILD_NDK_PATH",
        "ANDROID_NDK_HOME",
        "ANDROID_NDK_ROOT",
        "Ndk_home",
    ] {
        if let Some(path) = env::var_os(name).filter(|value| !value.is_empty()) {
            return Ok((PathBuf::from(path), name));
        }
    }
    for name in ["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Some(path) = env::var_os(name).filter(|value| !value.is_empty()) {
            return Ok((PathBuf::from(path).join("ndk"), name));
        }
    }
    Err(
        "Android NDK was not found; pass --ndk-path, set CARGO_ZIRILD_NDK_PATH/ANDROID_NDK_HOME/ANDROID_NDK_ROOT, or configure an Android SDK root"
            .into(),
    )
}

fn android_compiler_prefix(target: &str) -> Result<(&'static str, u32), String> {
    match target {
        "aarch64-linux-android" => Ok(("aarch64-linux-android", 21)),
        "armv7-linux-androideabi" => Ok(("armv7a-linux-androideabi", 16)),
        "i686-linux-android" => Ok(("i686-linux-android", 16)),
        "x86_64-linux-android" => Ok(("x86_64-linux-android", 21)),
        _ => Err(format!("unsupported Android Rust target: {target}")),
    }
}

fn select_ndk_root(home: &Path, options: &AndroidOptions, source: &str) -> Result<PathBuf, String> {
    let is_ndk_root = |path: &Path| {
        path.join("toolchains")
            .join("llvm")
            .join("prebuilt")
            .is_dir()
    };
    if is_ndk_root(home) {
        if options.ndk_version.is_some() || options.ndk_index.is_some() {
            return Err(format!(
                "{source} '{}' already points to one NDK installation; remove -ndkversion/-nindx",
                home.display()
            ));
        }
        return Ok(home.into());
    }
    if !home.is_dir() {
        return Err(format!("{source} path does not exist: {}", home.display()));
    }

    let candidates = fs::read_dir(home)
        .map_err(|err| format!("cannot read {source} directory '{}': {err}", home.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && is_ndk_root(path))
        .collect::<Vec<_>>();
    let mut ordered_candidates = candidates.clone();
    ordered_candidates.sort_by_key(|path| std::cmp::Reverse(ndk_version_key(path)));
    let available = ordered_candidates
        .iter()
        .filter_map(|path| path.file_name()?.to_str())
        .collect::<Vec<_>>()
        .join(", ");
    select_ndk_candidate(candidates, options).ok_or_else(|| {
        if let Some(version) = &options.ndk_version {
            format!(
                "NDK version '{version}' was not found below {source} '{}'; available versions: {available}",
                home.display()
            )
        } else if let Some(index) = options.ndk_index {
            format!(
                "NDK index {index} is out of range below {source} '{}'; newest-first versions: {available}",
                home.display()
            )
        } else {
            format!(
                "{source} '{}' is not an NDK and contains no NDK version with toolchains/llvm/prebuilt",
                home.display()
            )
        }
    })
}

fn select_ndk_candidate(mut candidates: Vec<PathBuf>, options: &AndroidOptions) -> Option<PathBuf> {
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by_key(|path| std::cmp::Reverse(ndk_version_key(path)));
    if let Some(version) = &options.ndk_version {
        return candidates.into_iter().find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy() == version.as_str())
        });
    }
    if let Some(index) = options.ndk_index {
        return candidates.into_iter().nth(index);
    }
    candidates.into_iter().next()
}

fn ndk_version_key(path: &Path) -> Vec<u32> {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .split('.')
        .map(|piece| piece.parse().unwrap_or(0))
        .collect()
}

fn ndk_host_tag() -> &'static str {
    if cfg!(windows) {
        "windows-x86_64"
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        "darwin-arm64"
    } else if cfg!(target_os = "macos") {
        "darwin-x86_64"
    } else {
        "linux-x86_64"
    }
}

struct Wrappers {
    directory: PathBuf,
    cc: PathBuf,
    cxx: PathBuf,
    linker: PathBuf,
    ar: PathBuf,
}

fn make_wrappers() -> Result<Wrappers, String> {
    let current = env::current_exe().map_err(|err| format!("cannot locate cargo-zirild: {err}"))?;
    let target_directory = match env::var_os("CARGO_TARGET_DIR") {
        Some(directory) => PathBuf::from(directory),
        None => env::current_dir()
            .map_err(|err| format!("cannot determine project directory: {err}"))?
            .join("target"),
    };
    let directory = target_directory
        .join(".cargo-zirild")
        .join(format!("{}", std::process::id()));
    fs::create_dir_all(&directory).map_err(|err| {
        format!(
            "cannot create Zig wrapper directory '{}': {err}",
            directory.display()
        )
    })?;

    let extension = if cfg!(windows) { ".exe" } else { "" };
    let wrapper = |name: &str| directory.join(format!("cargo-zirild-{name}{extension}"));
    let cc = wrapper("cc");
    let cxx = wrapper("cxx");
    let linker = wrapper("linker");
    let ar = wrapper("ar");
    let dlltool = directory.join(if cfg!(windows) {
        "dlltool.exe"
    } else {
        "dlltool"
    });
    for path in [&cc, &cxx, &linker, &ar, &dlltool] {
        fs::copy(&current, path).map_err(|err| {
            format!(
                "cannot create Zig wrapper '{}' from '{}': {err}",
                path.display(),
                current.display()
            )
        })?;
    }
    Ok(Wrappers {
        directory,
        cc,
        cxx,
        linker,
        ar,
    })
}

fn run_cargo(
    options: &BuildOptions,
    zig: Option<&ZigToolchain>,
    wrappers: &Wrappers,
    android_ndk: Option<&AndroidNdk>,
) -> Result<(), String> {
    let mut command = cargo_command(options);
    apply_wrapper_environment(&mut command, options, zig, wrappers, android_ndk)?;

    let is_msvc = options.cargo_target.ends_with("-msvc");
    let zig_version = zig.map_or("unversioned", |toolchain| toolchain.version.as_str());
    if is_msvc {
        eprintln!(
            "+ cargo {} --target {} (MSVC toolchain / {})",
            options.cargo_command, options.cargo_target, options.optimize
        );
    } else if options.android.ndk_fallback {
        let ndk = android_ndk.expect("fallback is validated as Android-only");
        eprintln!(
            "cargo-zirild: WARNING: -ndkfallback enabled. This build uses Android NDK Clang/LLD directly; the final output is not linked by Zig."
        );
        eprintln!(
            "+ cargo {} --target {} (Android NDK Clang/LLD fallback {} at {} / {})",
            options.cargo_command,
            options.cargo_target,
            ndk.version,
            ndk.root.display(),
            options.optimize
        );
    } else if let Some(ndk) = android_ndk {
        eprintln!(
            "+ cargo {} --target {} (Zig {} / {} / Android NDK {} at {} / {})",
            options.cargo_command,
            options.cargo_target,
            zig_version,
            options.zig_target,
            ndk.version,
            ndk.root.display(),
            options.optimize
        );
    } else {
        eprintln!(
            "+ cargo {} --target {} (Zig {} / {} / {})",
            options.cargo_command,
            options.cargo_target,
            zig_version,
            options.zig_target,
            options.optimize
        );
    }
    let tool = if options.cargo_command == "asm" {
        "cargo-asm"
    } else {
        "cargo"
    };
    let status = command
        .status()
        .map_err(|err| format!("failed to start {tool}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "cargo {} failed with {status}",
            options.cargo_command
        ))
    }
}

/// The command to run for the requested Cargo command. The `asm` command is a
/// Cargo plugin, so dispatch `cargo-asm` the way Cargo would and forward
/// Zirild's `-target` as cargo-asm's `--target`; otherwise the viewer would
/// inspect the host build while the wrappers are configured for the target.
fn cargo_command(options: &BuildOptions) -> Command {
    if options.cargo_command == "asm" {
        let mut command = Command::new("cargo-asm");
        command.arg("asm");
        if !has_asm_target_argument(&options.cargo_args) {
            command.arg("--target").arg(&options.cargo_target);
        }
        command.args(&options.cargo_args);
        command
    } else {
        let mut command = Command::new("cargo");
        command
            .arg(&options.cargo_command)
            .arg("--target")
            .arg(&options.cargo_target)
            .args(&options.cargo_args);
        command
    }
}

/// Whether the user passed their own cargo-asm `--target`, which keeps the
/// viewer on the target they selected.
fn has_asm_target_argument(arguments: &[OsString]) -> bool {
    arguments.iter().any(|argument| {
        let argument = argument.to_string_lossy();
        argument == "--target" || argument.starts_with("--target=")
    })
}

/// Configure the environment for the delegate process the way Cargo itself
/// would receive it: wrapper tool locations, the Zig and Android parameters
/// the wrappers read, and the PATH order that lets the wrappers find
/// `zig dlltool` when Windows GNU needs it.
fn apply_wrapper_environment(
    command: &mut Command,
    options: &BuildOptions,
    zig: Option<&ZigToolchain>,
    wrappers: &Wrappers,
    android_ndk: Option<&AndroidNdk>,
) -> Result<(), String> {
    if let Some(zig) = zig {
        command.env("CARGO_ZIRILD_ZIG", &zig.path);
    }
    command.env("CARGO_ZIRILD_ZIG_TARGET", &options.zig_target);
    command.env("CARGO_ZIRILD_OPTIMIZE", &options.optimize);
    command.env(
        "CARGO_ZIRILD_WINDOWS_RUNTIME",
        options.windows_runtime.as_str(),
    );
    if options.force_native_optimize {
        command.env("CARGO_ZIRILD_FORCE_NATIVE_OPTIMIZE", "1");
    }
    if options.trace {
        command.env("CARGO_ZIRILD_TRACE", "1");
    }
    if let Some(ndk) = android_ndk {
        command.env("CARGO_ZIRILD_ANDROID_SYSROOT", &ndk.sysroot);
        command.env(
            "CARGO_ZIRILD_ANDROID_CFLAGS",
            encode_wrapper_arguments(&options.android.cflags),
        );
        command.env(
            "CARGO_ZIRILD_ANDROID_LINK_ARGS",
            encode_wrapper_arguments(&options.android.link_args),
        );
        command.env("ANDROID_NDK_ROOT", &ndk.root);
        command.env("CMAKE_ANDROID_NDK", &ndk.root);
        if options.android.ndk_fallback {
            command.env("CARGO_ZIRILD_NDK_FALLBACK", "1");
            command.env("CARGO_ZIRILD_NDK_CC", &ndk.cc);
            command.env("CARGO_ZIRILD_NDK_CXX", &ndk.cxx);
            command.env("CARGO_ZIRILD_NDK_AR", &ndk.ar);
        }
        if let Some(abi) = &options.android.abi {
            command.env("ANDROID_ABI", abi);
        }
        if let Some(api_level) = options.android.api_level {
            command.env("ANDROID_PLATFORM", format!("android-{api_level}"));
        }
        if let Some(stl) = &options.android.stl {
            command.env("ANDROID_STL", stl);
        }
    }

    let normalized = options.cargo_target.replace('-', "_").to_ascii_uppercase();
    let is_msvc = options.cargo_target.ends_with("-msvc");
    if !is_msvc {
        command.env(
            format!("CARGO_TARGET_{normalized}_LINKER"),
            &wrappers.linker,
        );
        command.env(format!("CARGO_TARGET_{normalized}_AR"), &wrappers.ar);

        // cc crate checks both spellings depending on its version and platform.
        for target in [
            options.cargo_target.as_str(),
            &options.cargo_target.replace('-', "_"),
        ] {
            command.env(format!("CC_{target}"), &wrappers.cc);
            command.env(format!("CXX_{target}"), &wrappers.cxx);
            command.env(format!("AR_{target}"), &wrappers.ar);
        }

        let mut paths = vec![wrappers.directory.clone()];
        if let Some(current_path) = env::var_os("PATH") {
            paths.extend(env::split_paths(&current_path));
        }
        let path = env::join_paths(paths)
            .map_err(|err| format!("cannot construct PATH for Zig dlltool: {err}"))?;
        command.env("PATH", path);
    }
    Ok(())
}

fn wrapper_mode() -> Option<WrapperMode> {
    let executable = env::current_exe().ok()?;
    let stem = executable.file_stem()?.to_string_lossy();
    if stem.ends_with("-cc") {
        Some(WrapperMode::Cc)
    } else if stem.ends_with("-cxx") {
        Some(WrapperMode::Cxx)
    } else if stem.ends_with("-linker") {
        Some(WrapperMode::Linker)
    } else if stem.ends_with("-ar") {
        Some(WrapperMode::Ar)
    } else if stem == "dlltool" || stem.ends_with("-dlltool") {
        Some(WrapperMode::Dlltool)
    } else {
        None
    }
}

fn run_wrapper(mode: WrapperMode) -> Result<(), String> {
    let target = env::var_os("CARGO_ZIRILD_ZIG_TARGET")
        .ok_or_else(|| "CARGO_ZIRILD_ZIG_TARGET is missing from wrapper environment".to_owned())?;
    let optimize = env::var_os("CARGO_ZIRILD_OPTIMIZE")
        .ok_or_else(|| "CARGO_ZIRILD_OPTIMIZE is missing from wrapper environment".to_owned())?;
    let compiler_optimize = compiler_optimization_flag(&optimize)?;
    let force_native_optimize = env::var_os("CARGO_ZIRILD_FORCE_NATIVE_OPTIMIZE").is_some();
    let windows_runtime = WindowsRuntimePolicy::parse(
        &env::var("CARGO_ZIRILD_WINDOWS_RUNTIME").unwrap_or_else(|_| "auto".into()),
    )?;

    let ndk_fallback = env::var_os("CARGO_ZIRILD_NDK_FALLBACK").is_some();
    let mut command = if ndk_fallback {
        let tool = match mode {
            WrapperMode::Cc | WrapperMode::Linker => env::var_os("CARGO_ZIRILD_NDK_CC"),
            WrapperMode::Cxx => env::var_os("CARGO_ZIRILD_NDK_CXX"),
            WrapperMode::Ar => env::var_os("CARGO_ZIRILD_NDK_AR"),
            WrapperMode::Dlltool => {
                return Err(
                    "NDK fallback does not provide dlltool; Android builds must not request it"
                        .into(),
                );
            }
        }
        .ok_or_else(|| "NDK fallback tool path is missing from wrapper environment".to_owned())?;
        let mut command = Command::new(tool);
        if mode == WrapperMode::Linker {
            command.arg(compiler_optimize);
        }
        command
    } else {
        let zig = env::var_os("CARGO_ZIRILD_ZIG")
            .ok_or_else(|| "CARGO_ZIRILD_ZIG is missing from wrapper environment".to_owned())?;
        let mut command = Command::new(zig);
        match mode {
            WrapperMode::Cc => {
                command.arg("cc").arg("-target").arg(&target);
            }
            WrapperMode::Cxx => {
                command.arg("c++").arg("-target").arg(&target);
            }
            WrapperMode::Linker => {
                command
                    .arg("cc")
                    .arg("-target")
                    .arg(&target)
                    .arg(compiler_optimize);
            }
            WrapperMode::Ar => {
                command.arg("ar");
            }
            WrapperMode::Dlltool => {
                command.arg("dlltool");
            }
        }
        command
    };
    // Zig's C/C++ driver may inject UBSan checks based on its safety defaults,
    // while rustc's final native link does not request the matching sanitizer
    // runtime. Zirild's optimization contract is the explicit Clang -O flag,
    // so keep native objects self-contained unless callers opt into sanitizers.
    if matches!(mode, WrapperMode::Cc | WrapperMode::Cxx) {
        command.arg("-fno-sanitize=undefined");
    }
    if matches!(
        mode,
        WrapperMode::Cc | WrapperMode::Cxx | WrapperMode::Linker
    ) {
        let native_arguments = if mode == WrapperMode::Linker {
            wrapper_arguments("CARGO_ZIRILD_ANDROID_LINK_ARGS")
        } else {
            wrapper_arguments("CARGO_ZIRILD_ANDROID_CFLAGS")
        };
        command.args(native_arguments);
    }
    let arguments: Vec<_> = env::args_os().skip(1).collect();
    let windows_link = mode == WrapperMode::Linker
        && target
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("-windows-");
    let strip_musl_crt = mode == WrapperMode::Linker
        && !ndk_fallback
        && musl_self_contained_link(&target, &arguments);
    let strip_cortex_a53_fix = mode == WrapperMode::Linker
        && !ndk_fallback
        && target
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("aarch64")
        && has_cortex_a53_fix_argument(&arguments);
    let linux_linker_rewrites = strip_musl_crt || strip_cortex_a53_fix;
    let mut stripped_arguments = 0usize;
    if windows_link
        && windows_runtime == WindowsRuntimePolicy::Auto
        && windows_runtime_may_be_custom(&arguments)
    {
        eprintln!(
            "cargo-zirild: WARNING: custom Windows startup/runtime linker arguments were detected. The default Zig runtime rewrite remains active; use --windows-runtime=preserve to keep the original runtime arguments, or --windows-runtime=zig to acknowledge the rewrite explicitly."
        );
    }
    let prepared_arguments = if windows_link {
        prepare_windows_linker_arguments(arguments, windows_runtime)?
    } else if linux_linker_rewrites {
        let (prepared, stripped) =
            prepare_linux_linker_arguments(arguments, strip_musl_crt, strip_cortex_a53_fix)?;
        stripped_arguments = stripped;
        prepared
    } else {
        PreparedArguments::unchanged(arguments)
    };
    let arguments = &prepared_arguments.arguments;
    let mut argument_index = 0;
    while argument_index < arguments.len() {
        let argument = &arguments[argument_index];
        if matches!(mode, WrapperMode::Cc | WrapperMode::Cxx) {
            if argument == "--target" || argument == "-target" {
                argument_index += 2;
                continue;
            }
            if is_embedded_target_argument(argument) {
                argument_index += 1;
                continue;
            }
        }
        if force_native_optimize
            && matches!(
                mode,
                WrapperMode::Cc | WrapperMode::Cxx | WrapperMode::Linker
            )
            && is_native_optimization_argument(argument)
        {
            argument_index += 1;
            continue;
        }
        if mode == WrapperMode::Dlltool && argument == "--temp-prefix" {
            argument_index += 2;
            continue;
        }
        if windows_link {
            if env::var_os("CARGO_ZIRILD_TRACE").is_some()
                && Path::new(argument)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("def"))
            {
                match fs::read_to_string(argument) {
                    Ok(contents) => eprintln!(
                        "cargo-zirild trace: definition file {}:\n{}",
                        Path::new(argument).display(),
                        contents
                    ),
                    Err(err) => eprintln!(
                        "cargo-zirild trace: cannot read definition file {}: {err}",
                        Path::new(argument).display()
                    ),
                }
            }
            command.arg(argument);
        } else {
            command.arg(argument);
        }
        argument_index += 1;
    }
    if windows_link && windows_runtime.rewrites_runtime() {
        command.arg("-lunwind");
    }
    if force_native_optimize
        && matches!(
            mode,
            WrapperMode::Cc | WrapperMode::Cxx | WrapperMode::Linker
        )
    {
        command.arg(compiler_optimize);
    }

    if env::var_os("CARGO_ZIRILD_TRACE").is_some() && mode != WrapperMode::Dlltool {
        eprintln!(
            "cargo-zirild trace: invoking {} in {mode:?} mode with {} arguments",
            if ndk_fallback { "NDK LLVM" } else { "Zig" },
            arguments.len(),
        );
    }
    if strip_musl_crt && env::var_os("CARGO_ZIRILD_TRACE").is_some() {
        eprintln!(
            "cargo-zirild trace: stripped {stripped_arguments} rustc self-contained CRT arguments so Zig supplies the only musl CRT"
        );
    }
    if strip_cortex_a53_fix && env::var_os("CARGO_ZIRILD_TRACE").is_some() {
        eprintln!(
            "cargo-zirild trace: stripped rustc's Cortex-A53 workaround linker switch, which Zig's cc driver rejects"
        );
    }
    let status = command.status();
    prepared_arguments.cleanup();
    let tool = if ndk_fallback { "NDK LLVM" } else { "Zig" };
    let status = status.map_err(|err| format!("failed to start {tool}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{tool} {mode:?} failed with {status}"))
    }
}

fn is_embedded_target_argument(argument: &OsString) -> bool {
    let argument = argument.to_string_lossy();
    argument.starts_with("--target=") || argument.starts_with("-target=")
}

fn is_native_optimization_argument(argument: &OsString) -> bool {
    matches!(
        argument.to_string_lossy().as_ref(),
        "-O" | "-O0" | "-O1" | "-O2" | "-O3" | "-Og" | "-Os" | "-Oz" | "-Ofast"
    )
}

/// Whether rustc's link arguments represent a musl final link where rustc
/// supplies its own CRT startup objects. Rustc passes those objects plus
/// `-nostartfiles` to keep the linker driver from adding startup files of its
/// own, but Zig's cc driver ignores `-nostartfiles` and injects its bundled
/// musl `crt1.o` regardless, duplicating `_start` and `_start_c`. An explicit
/// `-lc` on rustc's line also defeats `-nostdlib`, so the only stable
/// composition is to strip rustc's CRT objects and let Zig's musl CRT stand
/// alone, matching the GNU-target model where Zig already supplies the CRT.
fn musl_self_contained_link(target: &OsStr, arguments: &[OsString]) -> bool {
    if !target
        .to_string_lossy()
        .to_ascii_lowercase()
        .ends_with("-musl")
    {
        return false;
    }
    arguments.iter().any(|argument| {
        if argument == "-nostartfiles" || is_self_contained_crt_object(argument) {
            return true;
        }
        let Some(path) = response_file_path(argument) else {
            return false;
        };
        fs::read_to_string(&path)
            .map(|contents| {
                contents.lines().any(|line| {
                    let argument = response_line_argument(line);
                    argument == "-nostartfiles"
                        || is_self_contained_crt_object(OsStr::new(&argument))
                })
            })
            .unwrap_or(false)
    })
}

/// The response file path of a rustc `@file` linker argument, if any. rustc
/// moves the whole link command into a response file once the command line
/// grows past the platform limit, so long musl links carry their arguments
/// there instead of in argv.
fn response_file_path(argument: &OsString) -> Option<PathBuf> {
    let text = argument.to_string_lossy();
    let path = text.strip_prefix('@')?;
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// The logical argument carried by one rustc response-file line, which wraps
/// each argument in optional double quotes.
fn response_line_argument(line: &str) -> String {
    line.trim().trim_matches('"').to_owned()
}

/// rustc passes a Cortex-A53 erratum workaround switch (`--fix-cortex-a53-843419`)
/// to the linker for aarch64 targets. Zig's cc driver rejects it as an
/// unsupported linker argument, so Zirild drops it from the link command,
/// leaving the resulting switch-free link fully under Zig's control.
fn has_cortex_a53_fix_argument(arguments: &[OsString]) -> bool {
    arguments.iter().any(|argument| {
        if is_cortex_a53_fix_argument(argument) {
            return true;
        }
        let Some(path) = response_file_path(argument) else {
            return false;
        };
        fs::read_to_string(&path)
            .map(|contents| {
                contents
                    .lines()
                    .any(|line| response_line_argument(line) == "-Wl,--fix-cortex-a53-843419")
            })
            .unwrap_or(false)
    })
}

fn is_cortex_a53_fix_argument(argument: &OsString) -> bool {
    argument == "-Wl,--fix-cortex-a53-843419"
}

/// Strip rustc's self-contained musl CRT objects, `-nostartfiles`, and the
/// aarch64 Cortex-A53 workaround switch from the link arguments. Response
/// files are rewritten into filtered private copies beside the wrapper
/// executable; rustc's original inputs are never edited. Returns the prepared
/// arguments and the number of stripped arguments and response-file lines.
fn prepare_linux_linker_arguments(
    arguments: Vec<OsString>,
    strip_musl_crt: bool,
    strip_cortex_a53_fix: bool,
) -> Result<(PreparedArguments, usize), String> {
    let wrapper =
        env::current_exe().map_err(|err| format!("cannot locate cargo-zirild wrapper: {err}"))?;
    let parent = wrapper
        .parent()
        .ok_or_else(|| "cargo-zirild wrapper has no parent directory".to_owned())?;
    let directory = parent.join(format!("link-{}", std::process::id()));
    fs::create_dir_all(&directory).map_err(|err| {
        format!(
            "cannot create private linker directory '{}': {err}",
            directory.display()
        )
    })?;
    let (prepared, stripped) = prepare_linux_arguments_in_directory(
        arguments,
        &directory,
        strip_musl_crt,
        strip_cortex_a53_fix,
    )?;
    Ok((
        PreparedArguments {
            arguments: prepared,
            temporary_directory: Some(directory),
        },
        stripped,
    ))
}

fn prepare_linux_arguments_in_directory(
    arguments: Vec<OsString>,
    directory: &Path,
    strip_musl_crt: bool,
    strip_cortex_a53_fix: bool,
) -> Result<(Vec<OsString>, usize), String> {
    let mut prepared = Vec::with_capacity(arguments.len());
    let mut file_index = 0usize;
    let mut stripped = 0usize;
    for argument in arguments {
        if let Some(response_path) = response_file_path(&argument) {
            let (rewritten, dropped) = prepare_linux_response_file(
                &response_path,
                directory,
                &mut file_index,
                strip_musl_crt,
                strip_cortex_a53_fix,
            )?;
            stripped += dropped;
            prepared.push(format!("@{}", rewritten.display()).into());
            continue;
        }
        let value = argument.to_string_lossy();
        if drops_linux_linker_argument(&value, strip_musl_crt, strip_cortex_a53_fix) {
            stripped += 1;
            continue;
        }
        prepared.push(argument);
    }
    Ok((prepared, stripped))
}

fn prepare_linux_response_file(
    source: &Path,
    directory: &Path,
    file_index: &mut usize,
    strip_musl_crt: bool,
    strip_cortex_a53_fix: bool,
) -> Result<(PathBuf, usize), String> {
    let original = fs::read_to_string(source).map_err(|err| {
        format!(
            "cannot read linker response file '{}': {err}",
            source.display()
        )
    })?;
    let mut dropped = 0usize;
    let rewritten: Vec<_> = original
        .lines()
        .filter(|line| {
            let argument = response_line_argument(line);
            let drop = drops_linux_linker_argument(&argument, strip_musl_crt, strip_cortex_a53_fix);
            if drop {
                dropped += 1;
            }
            !drop
        })
        .collect();
    let output = directory.join(format!("response-{}.rsp", *file_index));
    *file_index += 1;
    fs::write(&output, format!("{}\n", rewritten.join("\n"))).map_err(|err| {
        format!(
            "cannot write private linker response file '{}': {err}",
            output.display()
        )
    })?;
    Ok((output, dropped))
}

/// Whether rustc's Linux linker argument is one Zirild must drop, either the
/// musl self-contained CRT markers or the aarch64 Cortex-A53 switch.
fn drops_linux_linker_argument(
    argument: &str,
    strip_musl_crt: bool,
    strip_cortex_a53_fix: bool,
) -> bool {
    (strip_musl_crt
        && (argument == "-nostartfiles" || is_self_contained_crt_object(OsStr::new(argument))))
        || (strip_cortex_a53_fix && argument == "-Wl,--fix-cortex-a53-843419")
}

/// Whether a link argument is one of rustc's self-contained CRT startup
/// objects, recognized by the `self-contained` sysroot path component and the
/// object base name so identically named objects from build scripts survive.
fn is_self_contained_crt_object(argument: &OsStr) -> bool {
    let path = Path::new(argument);
    let from_self_contained = path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("self-contained")
    });
    if !from_self_contained {
        return false;
    }
    let Some(name) = path.file_name() else {
        return false;
    };
    matches!(
        name.to_string_lossy().to_ascii_lowercase().as_str(),
        "crt1.o"
            | "scrt1.o"
            | "rcrt1.o"
            | "crti.o"
            | "crtn.o"
            | "crtbegin.o"
            | "crtbegins.o"
            | "crtend.o"
            | "crtends.o"
    )
}

fn encode_wrapper_arguments(arguments: &[String]) -> String {
    arguments.join("\n")
}

fn wrapper_arguments(name: &str) -> Vec<OsString> {
    env::var(name)
        .ok()
        .map(|arguments| {
            arguments
                .split('\n')
                .filter(|argument| !argument.is_empty())
                .map(OsString::from)
                .collect()
        })
        .unwrap_or_default()
}

fn is_windows_runtime_argument(argument: &OsString) -> bool {
    matches!(
        argument.to_string_lossy().as_ref(),
        "-nodefaultlibs"
            | "-lgcc_eh"
            | "-l:libpthread.a"
            | "-lmsvcrt"
            | "-lmingwex"
            | "-lmingw32"
            | "-lgcc"
    )
}

/// Zig's bundled LLD explicitly ignores MinGW's auto-image-base switches and
/// emits a warning for them. Removing them keeps the Windows GNU build clean
/// without changing Zig's image-base behavior.
fn is_windows_auto_image_base_argument(argument: &OsString) -> bool {
    matches!(
        argument.to_string_lossy().as_ref(),
        "-Wl,--enable-auto-image-base"
            | "-Wl,--disable-auto-image-base"
            | "--enable-auto-image-base"
            | "--disable-auto-image-base"
    )
}

fn windows_runtime_may_be_custom(arguments: &[OsString]) -> bool {
    arguments.iter().any(|argument| {
        let text = argument.to_string_lossy();
        if let Some(path) = text.strip_prefix('@') {
            return fs::read_to_string(path).is_ok_and(|contents| {
                contents
                    .lines()
                    .any(|line| is_custom_windows_runtime_argument(line.trim().trim_matches('"')))
            });
        }
        is_custom_windows_runtime_argument(&text)
    })
}

fn is_custom_windows_runtime_argument(argument: &str) -> bool {
    matches!(argument, "-nostdlib" | "-nostartfiles" | "--entry" | "-e")
        || argument.starts_with("--entry=")
        || argument.starts_with("-Wl,--entry")
        || argument.starts_with("-Wl,-e,")
}

fn transform_windows_linker_argument(argument: &OsString) -> OsString {
    let text = argument.to_string_lossy();
    if let Some(definition) = text.strip_prefix("-Wl,")
        && definition.to_ascii_lowercase().ends_with(".def")
    {
        // Zig's compiler driver accepts a module-definition file as an input,
        // then forwards it to its Windows linker in the required form.
        return definition.into();
    }
    argument.clone()
}

struct PreparedArguments {
    arguments: Vec<OsString>,
    temporary_directory: Option<PathBuf>,
}

impl PreparedArguments {
    fn unchanged(arguments: Vec<OsString>) -> Self {
        Self {
            arguments,
            temporary_directory: None,
        }
    }

    fn cleanup(&self) {
        if let Some(directory) = &self.temporary_directory {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

/// rustc uses response and module-definition files for Windows GNU links.
/// Never rewrite those source files in place: create private copies beside the
/// wrapper executable and pass the copies to Zig instead. A terminated wrapper
/// may leave disposable files behind, but cannot corrupt rustc's link inputs.
fn prepare_windows_linker_arguments(
    arguments: Vec<OsString>,
    runtime_policy: WindowsRuntimePolicy,
) -> Result<PreparedArguments, String> {
    let wrapper =
        env::current_exe().map_err(|err| format!("cannot locate cargo-zirild wrapper: {err}"))?;
    let parent = wrapper
        .parent()
        .ok_or_else(|| "cargo-zirild wrapper has no parent directory".to_owned())?;
    let directory = parent.join(format!("link-{}", std::process::id()));
    fs::create_dir_all(&directory).map_err(|err| {
        format!(
            "cannot create private linker directory '{}': {err}",
            directory.display()
        )
    })?;

    let mut prepared = Vec::with_capacity(arguments.len());
    let mut file_index = 0usize;
    for argument in arguments {
        let text = argument.to_string_lossy();
        if let Some(response_path) = text.strip_prefix('@') {
            let response_path = PathBuf::from(response_path);
            let rewritten = prepare_windows_response_file(
                &response_path,
                &directory,
                &mut file_index,
                runtime_policy,
            )?;
            prepared.push(format!("@{}", rewritten.display()).into());
            continue;
        }
        if runtime_policy.rewrites_runtime()
            && (is_windows_runtime_argument(&argument)
                || is_windows_auto_image_base_argument(&argument))
        {
            continue;
        }
        let transformed = transform_windows_linker_argument(&argument);
        prepared.push(prepare_definition_argument(
            transformed,
            &directory,
            &mut file_index,
        )?);
    }

    Ok(PreparedArguments {
        arguments: prepared,
        temporary_directory: Some(directory),
    })
}

fn prepare_windows_response_file(
    source: &Path,
    directory: &Path,
    file_index: &mut usize,
    runtime_policy: WindowsRuntimePolicy,
) -> Result<PathBuf, String> {
    let original = fs::read_to_string(source).map_err(|err| {
        format!(
            "cannot read linker response file '{}': {err}",
            source.display()
        )
    })?;
    let lines: Vec<_> = original.lines().collect();
    let library_directories = response_library_directories(&lines);
    let mut rewritten = Vec::with_capacity(lines.len());
    for line in lines {
        if let Some(line) = prepare_windows_response_line(
            line,
            &library_directories,
            directory,
            file_index,
            runtime_policy,
        )? {
            rewritten.push(line);
        }
    }
    let output = directory.join(format!("response-{}.rsp", *file_index));
    *file_index += 1;
    fs::write(&output, format!("{}\n", rewritten.join("\n"))).map_err(|err| {
        format!(
            "cannot write private linker response file '{}': {err}",
            output.display()
        )
    })?;
    Ok(output)
}

fn prepare_windows_response_line(
    line: &str,
    library_directories: &[PathBuf],
    directory: &Path,
    file_index: &mut usize,
    runtime_policy: WindowsRuntimePolicy,
) -> Result<Option<String>, String> {
    let trimmed = line.trim();
    let quoted = trimmed.starts_with('"') && trimmed.ends_with('"');
    let value = trimmed.trim_matches('"');
    let argument = OsString::from(value);
    if runtime_policy.rewrites_runtime()
        && (is_windows_runtime_argument(&argument)
            || is_windows_auto_image_base_argument(&argument))
    {
        return Ok(None);
    }

    if let Some(name) = value.strip_prefix("-l")
        && let Some(library) = find_windows_import_library(name, library_directories)
    {
        return Ok(Some(format!("\"{}\"", library.display())));
    }

    let transformed = transform_windows_linker_argument(&argument);
    let transformed = prepare_definition_argument(transformed, directory, file_index)?;
    if transformed == value {
        Ok(Some(line.into()))
    } else if quoted {
        Ok(Some(format!("\"{}\"", transformed.to_string_lossy())))
    } else {
        Ok(Some(transformed.to_string_lossy().into_owned()))
    }
}

fn prepare_definition_argument(
    argument: OsString,
    directory: &Path,
    file_index: &mut usize,
) -> Result<OsString, String> {
    let path = PathBuf::from(&argument);
    if !path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("def"))
    {
        return Ok(argument);
    }
    let original = fs::read_to_string(&path).map_err(|err| {
        format!(
            "cannot read Windows definition file '{}': {err}",
            path.display()
        )
    })?;
    let Some(patched) = windows_definition_with_fallback_export(&original) else {
        return Ok(argument);
    };
    let output = directory.join(format!("definition-{}.def", *file_index));
    *file_index += 1;
    fs::write(&output, patched).map_err(|err| {
        format!(
            "cannot write private Windows definition file '{}': {err}",
            output.display()
        )
    })?;
    Ok(output.into_os_string())
}

fn windows_definition_with_fallback_export(contents: &str) -> Option<String> {
    let mut after_exports = false;
    for line in contents.lines() {
        let value = line.trim();
        if value.eq_ignore_ascii_case("EXPORTS") {
            after_exports = true;
            continue;
        }
        if after_exports && !value.is_empty() && !value.starts_with(';') {
            return None;
        }
    }
    after_exports.then(|| "EXPORTS\n    DllMainCRTStartup\n".into())
}

fn response_library_directories(lines: &[&str]) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let value = lines[index].trim().trim_matches('"');
        if value == "-L" && index + 1 < lines.len() {
            directories.push(PathBuf::from(lines[index + 1].trim().trim_matches('"')));
            index += 2;
            continue;
        }
        if let Some(directory) = value.strip_prefix("-L")
            && !directory.is_empty()
        {
            directories.push(PathBuf::from(directory));
        }
        index += 1;
    }
    directories
}

fn find_windows_import_library(name: &str, directories: &[PathBuf]) -> Option<PathBuf> {
    let candidates = [
        format!("lib{name}.a"),
        format!("{name}.lib"),
        format!("lib{name}.dll.a"),
    ];
    for directory in directories {
        for candidate in &candidates {
            let path = directory.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// Zig's C/C++ frontend accepts Clang optimization flags, not Zig Build's
/// -OReleaseSafe spelling. Rust optimization remains controlled by Cargo.
fn compiler_optimization_flag(optimize: &OsString) -> Result<&'static str, String> {
    match optimize.to_string_lossy().as_ref() {
        "Debug" => Ok("-O0"),
        "ReleaseSafe" => Ok("-O2"),
        "ReleaseFast" => Ok("-O3"),
        "ReleaseSmall" => Ok("-Oz"),
        invalid => Err(format!("invalid CARGO_ZIRILD_OPTIMIZE '{invalid}'")),
    }
}

/// Convert common Zig triples to the Rust triples required by cargo --target.
/// Fully qualified Rust triples are preserved.
fn cargo_target_for(zig_target: &str) -> String {
    let pieces: Vec<_> = zig_target.split('-').collect();
    match pieces.as_slice() {
        ["arm", "linux", "android"] | ["armv7", "linux", "android"] => {
            "armv7-linux-androideabi".into()
        }
        [architecture, "linux", "android"] => format!("{architecture}-linux-android"),
        [architecture, "linux", abi] => format!("{architecture}-unknown-linux-{abi}"),
        [architecture, "pc", "windows"] => format!("{architecture}-pc-windows-msvc"),
        [architecture, "windows", abi] => format!("{architecture}-pc-windows-{abi}"),
        [architecture, "macos", _] => format!("{architecture}-apple-darwin"),
        _ => zig_target.into(),
    }
}

/// Accept a Rust target triple while passing Zig's shorter platform spelling
/// to its C/C++ and linker frontends.
fn zig_target_for(target: &str) -> String {
    let pieces: Vec<_> = target.split('-').collect();
    match pieces.as_slice() {
        [architecture, "unknown", "linux", abi] => format!("{architecture}-linux-{abi}"),
        [architecture, "pc", "windows"] => format!("{architecture}-windows-msvc"),
        [architecture, "pc", "windows", abi] => format!("{architecture}-windows-{abi}"),
        [architecture, "apple", "darwin"] => format!("{architecture}-macos-none"),
        _ => target.into(),
    }
}

fn warn_about_android_target(zig_target: &str, ndk: Option<&AndroidNdk>, ndk_fallback: bool) {
    if let Some(ndk) = ndk {
        if ndk_fallback {
            eprintln!(
                "cargo-zirild: Android NDK {} selected at '{}'; -ndkfallback will use its Clang/LLD tools directly.",
                ndk.version,
                ndk.root.display()
            );
        } else {
            eprintln!(
                "cargo-zirild: Android NDK {} selected at '{}'; its root and sysroot are exported to build scripts while Zig remains the compiler and final linker.",
                ndk.version,
                ndk.root.display()
            );
        }
    }
    if zig_target.contains("-android") && !ndk_fallback {
        eprintln!(
            "cargo-zirild: note: this Zig distribution cannot currently link Android libc, including an NDK sysroot. \
             Zirild will not fall back to NDK clang because that would violate its single Zig linker contract. \
             For a musl-based Android-compatible native build, try -target={} instead; \
             this is a musl Linux binary and not an Android-NDK-linked binary.",
            android_musl_target(zig_target)
        );
    }
}

fn android_musl_target(zig_target: &str) -> String {
    zig_target
        .split_once("-android")
        .map(|(prefix, _)| format!("{prefix}-musl"))
        .unwrap_or_else(|| zig_target.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_optimization_modes() {
        let dev = parse_args(vec!["-target=x86_64-linux-musl".into()]).unwrap();
        let release =
            parse_args(vec!["-target=x86_64-linux-musl".into(), "--release".into()]).unwrap();
        let small = parse_args(vec![
            "-target=x86_64-linux-musl".into(),
            "--zig-release-small".into(),
        ])
        .unwrap();
        assert_eq!(dev.optimize, "ReleaseSafe");
        assert_eq!(release.optimize, "ReleaseFast");
        assert_eq!(small.optimize, "ReleaseSmall");
        assert!(!dev.force_native_optimize);
        assert!(small.force_native_optimize);
    }

    #[test]
    fn explicit_optimization_wins() {
        let options = parse_args(vec![
            "-target=x86_64-linux-musl".into(),
            "--release".into(),
            "--zig-opt=small".into(),
        ])
        .unwrap();
        assert_eq!(options.optimize, "ReleaseSmall");
        assert!(options.force_native_optimize);
    }

    #[test]
    fn maps_common_zig_triples_to_cargo_triples() {
        assert_eq!(
            cargo_target_for("aarch64-linux-musl"),
            "aarch64-unknown-linux-musl"
        );
        assert_eq!(
            cargo_target_for("x86_64-windows-gnu"),
            "x86_64-pc-windows-gnu"
        );
        assert_eq!(
            cargo_target_for("x86_64-linux-android"),
            "x86_64-linux-android"
        );
        assert_eq!(
            cargo_target_for("x86_64-pc-windows"),
            "x86_64-pc-windows-msvc"
        );
    }

    #[test]
    fn maps_zig_policies_to_c_compiler_flags() {
        assert_eq!(compiler_optimization_flag(&"Debug".into()).unwrap(), "-O0");
        assert_eq!(
            compiler_optimization_flag(&"ReleaseSafe".into()).unwrap(),
            "-O2"
        );
        assert_eq!(
            compiler_optimization_flag(&"ReleaseFast".into()).unwrap(),
            "-O3"
        );
        assert_eq!(
            compiler_optimization_flag(&"ReleaseSmall".into()).unwrap(),
            "-Oz"
        );
    }

    #[test]
    fn recognizes_native_compiler_target_overrides() {
        assert!(is_embedded_target_argument(&OsString::from(
            "--target=x86_64-unknown-linux-gnu"
        )));
        assert!(is_embedded_target_argument(&OsString::from(
            "-target=aarch64-linux-android"
        )));
        assert!(!is_embedded_target_argument(&OsString::from("-m64")));
        assert!(is_native_optimization_argument(&OsString::from("-O3")));
        assert!(!is_native_optimization_argument(&OsString::from("-g")));
    }

    #[test]
    fn detects_musl_self_contained_links_by_target_and_nostartfiles() {
        let arguments = vec![OsString::from("-nostartfiles")];
        assert!(musl_self_contained_link(
            OsStr::new("x86_64-linux-musl"),
            &arguments
        ));
        assert!(musl_self_contained_link(
            OsStr::new("aarch64-linux-musl"),
            &arguments
        ));
        assert!(!musl_self_contained_link(
            OsStr::new("x86_64-linux-gnu"),
            &arguments
        ));
        assert!(!musl_self_contained_link(
            OsStr::new("x86_64-linux-musl"),
            &[]
        ));
        assert!(!musl_self_contained_link(
            OsStr::new("x86_64-windows-gnu"),
            &arguments
        ));
    }

    /// Build a rustlib path from components so the separators follow the host.
    ///
    /// A hard-coded `C:\...\self-contained\rcrt1.o` literal is a single opaque
    /// path component on Unix, because `Path::components` only splits on the
    /// host platform's separator. Assertions built from that literal held on
    /// Windows and failed everywhere else.
    fn rustlib_musl_object(tail: &[&str]) -> OsString {
        let parts: Vec<&str> = ["root", "lib", "rustlib", "x86_64-unknown-linux-musl", "lib"]
            .into_iter()
            .chain(tail.iter().copied())
            .collect();
        PathBuf::from_iter(parts).into_os_string()
    }

    #[test]
    fn recognizes_self_contained_crt_objects_without_touching_other_inputs() {
        assert!(is_self_contained_crt_object(&rustlib_musl_object(&[
            "self-contained",
            "rcrt1.o"
        ])));
        // The self-contained component and the file name are matched without
        // regard to ASCII case, which is how Windows paths arrive.
        assert!(is_self_contained_crt_object(&rustlib_musl_object(&[
            "Self-Contained",
            "CRTBEGINS.O"
        ])));
        assert!(!is_self_contained_crt_object(&rustlib_musl_object(&[
            "self-contained",
            "libc.a"
        ])));
        assert!(!is_self_contained_crt_object(&rustlib_musl_object(&[
            "crt1.o"
        ])));
        assert!(!is_self_contained_crt_object(&OsString::from(
            "-nostartfiles"
        )));
    }

    #[test]
    fn detects_musl_self_contained_links_inside_response_files() {
        let root = env::temp_dir().join(format!("cargo-zirild-musl-detect-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let response = root.join("link.rsp");
        fs::write(&response, "\"-m64\"\n\"-nostartfiles\"\n").unwrap();
        let arguments = vec![OsString::from(format!("@{}", response.display()))];
        assert!(musl_self_contained_link(
            OsStr::new("x86_64-linux-musl"),
            &arguments
        ));
        let crt = rustlib_musl_object(&["self-contained", "rcrt1.o"]);
        fs::write(
            &response,
            format!("\"-m64\"\n\"{}\"\n", Path::new(&crt).display()),
        )
        .unwrap();
        assert!(musl_self_contained_link(
            OsStr::new("x86_64-linux-musl"),
            &arguments
        ));
        fs::write(&response, "\"-m64\"\n\"-static-pie\"\n").unwrap();
        assert!(!musl_self_contained_link(
            OsStr::new("x86_64-linux-musl"),
            &arguments
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn strips_musl_crt_from_arguments_and_private_response_copies() {
        let root =
            env::temp_dir().join(format!("cargo-zirild-musl-prepare-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let directory = root.join("private");
        fs::create_dir_all(&directory).unwrap();
        let response = root.join("link.rsp");
        let crt = rustlib_musl_object(&["self-contained", "rcrt1.o"]);
        let crt_text = Path::new(&crt).display().to_string();
        let original = format!("\"-m64\"\n\"{crt_text}\"\n\"-nostartfiles\"\n\"-static-pie\"\n");
        fs::write(&response, &original).unwrap();
        let arguments = vec![
            OsString::from(format!("@{}", response.display())),
            crt,
            OsString::from("-nostartfiles"),
            OsString::from("-lc"),
        ];
        let (prepared, stripped) =
            prepare_linux_arguments_in_directory(arguments, &directory, true, false).unwrap();
        assert_eq!(stripped, 4);
        assert_eq!(prepared.len(), 2);
        assert!(prepared[0].to_string_lossy().starts_with('@'));
        assert_eq!(prepared[1], OsString::from("-lc"));
        let rewritten = fs::read_to_string(PathBuf::from(
            prepared[0].to_string_lossy().strip_prefix('@').unwrap(),
        ))
        .unwrap();
        assert_eq!(rewritten, "\"-m64\"\n\"-static-pie\"\n");
        assert_eq!(fs::read_to_string(&response).unwrap(), original);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn strips_cortex_a53_switch_from_aarch64_links() {
        let root = env::temp_dir().join(format!("cargo-zirild-cortex-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let directory = root.join("private");
        fs::create_dir_all(&directory).unwrap();
        let response = root.join("link.rsp");
        let original = "\"-m64\"\n\"-Wl,--fix-cortex-a53-843419\"\n\"-static\"\n";
        fs::write(&response, original).unwrap();
        let arguments = vec![
            OsString::from(format!("@{}", response.display())),
            OsString::from("-Wl,--fix-cortex-a53-843419"),
            OsString::from("-lc"),
        ];
        let (prepared, stripped) =
            prepare_linux_arguments_in_directory(arguments, &directory, false, true).unwrap();
        assert_eq!(stripped, 2);
        assert_eq!(prepared.len(), 2);
        assert_eq!(prepared[1], OsString::from("-lc"));
        let rewritten = fs::read_to_string(PathBuf::from(
            prepared[0].to_string_lossy().strip_prefix('@').unwrap(),
        ))
        .unwrap();
        assert_eq!(rewritten, "\"-m64\"\n\"-static\"\n");
        assert_eq!(fs::read_to_string(&response).unwrap(), original);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn detects_cortex_a53_switch_in_arguments_and_response_files() {
        assert!(has_cortex_a53_fix_argument(&[OsString::from(
            "-Wl,--fix-cortex-a53-843419"
        )]));
        assert!(!has_cortex_a53_fix_argument(&[OsString::from("-lc")]));
    }

    #[test]
    fn accepts_rust_linux_triples_for_zig() {
        let options = parse_args(vec!["-target=x86_64-unknown-linux-gnu".into()]).unwrap();
        assert_eq!(options.cargo_target, "x86_64-unknown-linux-gnu");
        assert_eq!(options.zig_target, "x86_64-linux-gnu");
    }

    #[test]
    fn expands_short_windows_target_to_msvc() {
        let options = parse_args(vec!["-target=x86_64-pc-windows".into()]).unwrap();
        assert_eq!(options.cargo_target, "x86_64-pc-windows-msvc");
        assert_eq!(options.zig_target, "x86_64-windows-msvc");
    }

    #[test]
    fn validates_windows_runtime_policy_scope_and_custom_runtime_signals() {
        let options = parse_args(vec![
            "-target=x86_64-pc-windows-gnu".into(),
            "--windows-runtime=preserve".into(),
        ])
        .unwrap();
        assert_eq!(options.windows_runtime, WindowsRuntimePolicy::Preserve);
        assert!(
            parse_args(vec![
                "-target=x86_64-linux-gnu".into(),
                "--windows-runtime=preserve".into(),
            ])
            .is_err()
        );
        assert!(is_custom_windows_runtime_argument("-nostartfiles"));
        assert!(is_custom_windows_runtime_argument(
            "-Wl,--entry=custom_start"
        ));
        assert!(!is_custom_windows_runtime_argument("-nodefaultlibs"));
    }

    #[test]
    fn zigpatch_overrides_are_parsed() {
        let options = parse_args(vec![
            "-target=x86_64-linux-musl".into(),
            "-zigpatch=C:\\tools\\zig\\zig.exe".into(),
        ])
        .unwrap();
        assert_eq!(
            options.zig_patch,
            Some(PathBuf::from("C:\\tools\\zig\\zig.exe"))
        );
        let canonical = parse_args(vec![
            "-target=x86_64-linux-android".into(),
            "--zig-path=C:\\tools\\zig".into(),
            "--ndk-path=C:\\Android\\Sdk\\ndk".into(),
        ])
        .unwrap();
        assert_eq!(canonical.zig_patch, Some(PathBuf::from("C:\\tools\\zig")));
        assert_eq!(
            canonical.android.ndk_path,
            Some(PathBuf::from("C:\\Android\\Sdk\\ndk"))
        );
    }

    #[test]
    fn cargo_subcommand_name_is_not_forwarded_to_build() {
        let options =
            parse_args(vec!["-target=x86_64-linux-musl".into(), "zirild".into()]).unwrap();
        assert!(options.cargo_args.is_empty());
        assert_eq!(options.cargo_command, "build");
        assert_eq!(options.zig_target, "x86_64-linux-musl");
    }

    #[test]
    fn selects_cargo_run_after_zirild_options() {
        let options = parse_args(vec![
            "-target=x86_64-pc-windows-msvc".into(),
            "run".into(),
            "--release".into(),
        ])
        .unwrap();
        assert_eq!(options.cargo_command, "run");
        assert_eq!(options.cargo_args, vec![OsString::from("--release")]);
        assert_eq!(options.optimize, "ReleaseFast");
    }

    #[test]
    fn parses_asm_command_after_zirild_options() {
        let options = parse_args(vec![
            "-target=x86_64-unknown-linux-musl".into(),
            "asm".into(),
            "add".into(),
        ])
        .unwrap();
        assert_eq!(options.cargo_command, "asm");
        assert_eq!(options.cargo_args, vec![OsString::from("add")]);
        assert_eq!(options.zig_target, "x86_64-linux-musl");
    }

    #[test]
    fn forwards_asm_target_to_plugin_unless_user_chooses_one() {
        assert!(!has_asm_target_argument(&[OsString::from("add")]));
        assert!(has_asm_target_argument(&[OsString::from("--target")]));
        assert!(has_asm_target_argument(&[OsString::from(
            "--target=aarch64-unknown-linux-musl"
        )]));
        assert!(!has_asm_target_argument(&[OsString::from(
            "--targetname=add"
        )]));
    }

    #[test]
    fn preserves_separator_and_run_arguments() {
        let options = parse_args(vec![
            "-target=x86_64-pc-windows-msvc".into(),
            "run".into(),
            "--".into(),
            "--help".into(),
            "run".into(),
        ])
        .unwrap();
        assert_eq!(options.cargo_command, "run");
        assert_eq!(
            options.cargo_args,
            vec![
                OsString::from("--"),
                OsString::from("--help"),
                OsString::from("run")
            ]
        );
        assert_eq!(
            driver_request(&[
                OsString::from("-target=x86_64-pc-windows-msvc"),
                OsString::from("run"),
                OsString::from("--help")
            ]),
            None
        );
        assert_eq!(
            driver_request(&[
                OsString::from("-target=x86_64-pc-windows-msvc"),
                OsString::from("--help")
            ]),
            Some(DriverRequest::Help)
        );
    }

    #[test]
    fn recognizes_driver_version_requests_ahead_of_the_cargo_command() {
        assert_eq!(
            driver_request(&[OsString::from("-V")]),
            Some(DriverRequest::Version)
        );
        assert_eq!(
            driver_request(&[OsString::from("zirild"), OsString::from("--version")]),
            Some(DriverRequest::Version)
        );
        assert_eq!(
            driver_request(&[
                OsString::from("-target=x86_64-linux-musl"),
                OsString::from("--version")
            ]),
            Some(DriverRequest::Version)
        );
        assert_eq!(
            driver_request(&[
                OsString::from("run"),
                OsString::from("--"),
                OsString::from("--version")
            ]),
            None
        );
        let version = VERSION.split('.').collect::<Vec<_>>();
        assert!(version.len() >= 3, "unexpected VERSION: {VERSION}");
        assert!(version.iter().all(|piece| !piece.is_empty()));
    }

    #[test]
    fn recognizes_bare_help_spellings_ahead_of_the_cargo_command() {
        for spelling in ["help", "?"] {
            assert_eq!(
                driver_request(&[OsString::from(spelling)]),
                Some(DriverRequest::Help),
                "a bare {spelling} should request help"
            );
            assert_eq!(
                driver_request(&[
                    OsString::from("-target=x86_64-unknown-linux-musl"),
                    OsString::from(spelling)
                ]),
                Some(DriverRequest::Help),
                "{spelling} after a Zirild option should request help"
            );
            assert_eq!(
                driver_request(&[OsString::from("run"), OsString::from(spelling)]),
                None,
                "{spelling} after a Cargo command belongs to Cargo"
            );
        }
    }

    #[test]
    fn does_not_treat_cargo_option_value_as_command() {
        let options = parse_args(vec![
            "-target=x86_64-pc-windows-msvc".into(),
            "--bin".into(),
            "run".into(),
        ])
        .unwrap();
        assert_eq!(options.cargo_command, "build");
        assert_eq!(
            options.cargo_args,
            vec![OsString::from("--bin"), OsString::from("run")]
        );
    }

    #[test]
    fn preserves_cargo_nightly_z_option_and_stops_command_guessing() {
        let options = parse_args(vec![
            "-target=x86_64-linux-musl".into(),
            "-Z".into(),
            "unstable-options".into(),
            "run".into(),
        ])
        .unwrap();
        assert_eq!(options.cargo_command, "build");
        assert_eq!(
            options.cargo_args,
            vec![
                OsString::from("-Z"),
                OsString::from("unstable-options"),
                OsString::from("run")
            ]
        );
        assert_eq!(options.optimize, "ReleaseSafe");
    }

    #[test]
    fn converts_windows_definition_file_for_zig_linker() {
        let argument = OsString::from("-Wl,D:\\build\\exports.def");
        assert_eq!(
            transform_windows_linker_argument(&argument),
            OsString::from("D:\\build\\exports.def")
        );
    }

    #[test]
    fn rewrites_private_linker_inputs_without_mutating_sources() {
        let root =
            env::temp_dir().join(format!("cargo-zirild-response-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let private = root.join("private");
        fs::create_dir_all(&private).unwrap();
        let definition = root.join("exports.def");
        let response = root.join("link.rsp");
        fs::write(&definition, "EXPORTS\n").unwrap();
        fs::write(
            &response,
            format!("\"-nodefaultlibs\"\n\"-Wl,{}\"\n", definition.display()),
        )
        .unwrap();

        let mut index = 0;
        let rewritten = prepare_windows_response_file(
            &response,
            &private,
            &mut index,
            WindowsRuntimePolicy::Auto,
        )
        .unwrap();
        let rewritten = fs::read_to_string(rewritten).unwrap();

        assert_eq!(
            fs::read_to_string(&response).unwrap(),
            format!("\"-nodefaultlibs\"\n\"-Wl,{}\"\n", definition.display())
        );
        assert_eq!(fs::read_to_string(&definition).unwrap(), "EXPORTS\n");
        assert!(!rewritten.contains("-nodefaultlibs"));
        assert!(rewritten.contains("definition-0.def"));
        assert_eq!(
            fs::read_to_string(private.join("definition-0.def")).unwrap(),
            "EXPORTS\n    DllMainCRTStartup\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn adds_fallback_export_only_to_empty_definition_files() {
        assert_eq!(
            windows_definition_with_fallback_export("EXPORTS\n"),
            Some("EXPORTS\n    DllMainCRTStartup\n".into())
        );
        assert_eq!(
            windows_definition_with_fallback_export("EXPORTS\n    real_export\n"),
            None
        );
    }

    #[test]
    fn orders_ndk_version_directories_numerically() {
        assert!(
            ndk_version_key(Path::new("27.2.12479018"))
                < ndk_version_key(Path::new("30.0.14904198"))
        );
    }

    #[test]
    fn selects_ndk_versions_newest_first_or_by_exact_name() {
        let candidates = vec![
            PathBuf::from("27.2.12479018"),
            PathBuf::from("30.0.14904198"),
            PathBuf::from("26.3.11579264"),
        ];
        assert_eq!(
            select_ndk_candidate(candidates.clone(), &AndroidOptions::default()),
            Some(PathBuf::from("30.0.14904198"))
        );
        let indexed = AndroidOptions {
            ndk_index: Some(1),
            ..AndroidOptions::default()
        };
        assert_eq!(
            select_ndk_candidate(candidates.clone(), &indexed),
            Some(PathBuf::from("27.2.12479018"))
        );
        let exact = AndroidOptions {
            ndk_version: Some("26.3.11579264".into()),
            ..AndroidOptions::default()
        };
        assert_eq!(
            select_ndk_candidate(candidates, &exact),
            Some(PathBuf::from("26.3.11579264"))
        );
    }

    #[test]
    fn parses_android_ndk_and_toolchain_options_without_forwarding_to_cargo() {
        let options = parse_args(vec![
            "-target=x86_64-linux-android".into(),
            "-nindx=1".into(),
            "-android-api=24".into(),
            "-DANDROID_ABI=x86_64".into(),
            "-DANDROID_STL=c++_shared".into(),
            "-android-cflag=-fPIC".into(),
            "-android-link-arg=-Wl,--build-id".into(),
            "-ndkfallback".into(),
        ])
        .unwrap();
        assert_eq!(options.zig_target, "x86_64-linux-android.24");
        assert_eq!(options.android.ndk_index, Some(1));
        assert_eq!(options.android.api_level, Some(24));
        assert_eq!(options.android.abi.as_deref(), Some("x86_64"));
        assert_eq!(options.android.stl.as_deref(), Some("c++_shared"));
        assert_eq!(options.android.cflags, vec!["-fPIC"]);
        assert_eq!(options.android.link_args, vec!["-Wl,--build-id"]);
        assert!(options.android.ndk_fallback);
        assert!(options.cargo_args.is_empty());
    }

    #[test]
    fn rejects_conflicting_ndk_selectors_and_android_abi() {
        assert!(
            parse_args(vec![
                "-target=x86_64-linux-android".into(),
                "-ndkversion=30.0.14904198".into(),
                "-nindx=0".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(vec![
                "-target=x86_64-linux-android".into(),
                "-android-abi=arm64-v8a".into(),
            ])
            .is_err()
        );
    }

    #[test]
    fn removes_android_api_level_from_musl_compatibility_suggestion() {
        assert_eq!(
            android_musl_target("x86_64-linux-android.24"),
            "x86_64-linux-musl"
        );
    }
}
