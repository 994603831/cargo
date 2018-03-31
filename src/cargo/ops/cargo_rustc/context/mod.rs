#![allow(deprecated)]

use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::str::{self, FromStr};
use std::sync::Arc;
use std::cell::RefCell;

use jobserver::Client;

use core::{Package, PackageId, PackageSet, Profile, Resolve, Target};
use core::{Dependency, Profiles, Workspace};
use util::{internal, profile, Cfg, CfgExpr, Config, ProcessBuilder};
use util::errors::{CargoResult, CargoResultExt};

use super::TargetConfig;
use super::custom_build::{BuildDeps, BuildScripts, BuildState};
use super::fingerprint::Fingerprint;
use super::layout::Layout;
use super::links::Links;
use super::{BuildConfig, Compilation, Kind};

mod unit_dependencies;
use self::unit_dependencies::build_unit_dependencies;

mod compilation_files;
use self::compilation_files::CompilationFiles;
pub use self::compilation_files::Metadata;

/// All information needed to define a Unit.
///
/// A unit is an object that has enough information so that cargo knows how to build it.
/// For example, if your project has dependencies, then every dependency will be built as a library
/// unit. If your project is a library, then it will be built as a library unit as well, or if it
/// is a binary with `main.rs`, then a binary will be output. There are also separate unit types
/// for `test`ing and `check`ing, amongst others.
///
/// The unit also holds information about all possible metadata about the package in `pkg`.
///
/// A unit needs to know extra information in addition to the type and root source file. For
/// example, it needs to know the target architecture (OS, chip arch etc.) and it needs to know
/// whether you want a debug or release build. There is enough information in this struct to figure
/// all that out.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct Unit<'a> {
    /// Information about available targets, which files to include/exclude, etc. Basically stuff in
    /// `Cargo.toml`.
    pub pkg: &'a Package,
    /// Information about the specific target to build, out of the possible targets in `pkg`. Not
    /// to be confused with *target-triple* (or *target architecture* ...), the target arch for a
    /// build.
    pub target: &'a Target,
    /// The profile contains information about *how* the build should be run, including debug
    /// level, extra args to pass to rustc, etc.
    pub profile: &'a Profile,
    /// Whether this compilation unit is for the host or target architecture.
    ///
    /// For example, when
    /// cross compiling and using a custom build script, the build script needs to be compiled for
    /// the host architecture so the host rustc can use it (when compiling to the target
    /// architecture).
    pub kind: Kind,
}

/// Type of each file generated by a Unit.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TargetFileType {
    /// Not a special file type.
    Normal,
    /// It is something you can link against (e.g. a library)
    Linkable,
    /// It is a piece of external debug information (e.g. *.dSYM and *.pdb)
    DebugInfo,
}

/// The build context, containing all information about a build task
pub struct Context<'a, 'cfg: 'a> {
    /// The workspace the build is for
    pub ws: &'a Workspace<'cfg>,
    /// The cargo configuration
    pub config: &'cfg Config,
    /// The dependency graph for our build
    pub resolve: &'a Resolve,
    /// Information on the compilation output
    pub compilation: Compilation<'cfg>,
    pub packages: &'a PackageSet<'cfg>,
    pub build_state: Arc<BuildState>,
    pub build_script_overridden: HashSet<(PackageId, Kind)>,
    pub build_explicit_deps: HashMap<Unit<'a>, BuildDeps>,
    pub fingerprints: HashMap<Unit<'a>, Arc<Fingerprint>>,
    pub compiled: HashSet<Unit<'a>>,
    pub build_config: BuildConfig,
    pub build_scripts: HashMap<Unit<'a>, Arc<BuildScripts>>,
    pub links: Links<'a>,
    pub used_in_plugin: HashSet<Unit<'a>>,
    pub jobserver: Client,

    target_info: TargetInfo,
    host_info: TargetInfo,
    profiles: &'a Profiles,
    incremental_env: Option<bool>,

    unit_dependencies: HashMap<Unit<'a>, Vec<Unit<'a>>>,
    files: Option<CompilationFiles<'a, 'cfg>>,
}

#[derive(Clone, Default)]
struct TargetInfo {
    crate_type_process: Option<ProcessBuilder>,
    crate_types: RefCell<HashMap<String, Option<(String, String)>>>,
    cfg: Option<Vec<Cfg>>,
    sysroot_libdir: Option<PathBuf>,
}

impl TargetInfo {
    fn discover_crate_type(&self, crate_type: &str) -> CargoResult<Option<(String, String)>> {
        let mut process = self.crate_type_process.clone().unwrap();

        process.arg("--crate-type").arg(crate_type);

        let output = process.exec_with_output().chain_err(|| {
            format!(
                "failed to run `rustc` to learn about \
                 crate-type {} information",
                crate_type
            )
        })?;

        let error = str::from_utf8(&output.stderr).unwrap();
        let output = str::from_utf8(&output.stdout).unwrap();
        Ok(parse_crate_type(crate_type, error, &mut output.lines())?)
    }
}

impl<'a, 'cfg> Context<'a, 'cfg> {
    pub fn new(
        ws: &'a Workspace<'cfg>,
        resolve: &'a Resolve,
        packages: &'a PackageSet<'cfg>,
        config: &'cfg Config,
        build_config: BuildConfig,
        profiles: &'a Profiles,
        units: &[Unit<'a>],
    ) -> CargoResult<Context<'a, 'cfg>> {
        let dest = if build_config.release {
            "release"
        } else {
            "debug"
        };
        let host_layout = Layout::new(ws, None, dest)?;
        let target_layout = match build_config.requested_target.as_ref() {
            Some(target) => Some(Layout::new(ws, Some(target), dest)?),
            None => None,
        };

        let incremental_env = match env::var("CARGO_INCREMENTAL") {
            Ok(v) => Some(v == "1"),
            Err(_) => None,
        };

        // Load up the jobserver that we'll use to manage our parallelism. This
        // is the same as the GNU make implementation of a jobserver, and
        // intentionally so! It's hoped that we can interact with GNU make and
        // all share the same jobserver.
        //
        // Note that if we don't have a jobserver in our environment then we
        // create our own, and we create it with `n-1` tokens because one token
        // is ourself, a running process.
        let jobserver = match config.jobserver_from_env() {
            Some(c) => c.clone(),
            None => Client::new(build_config.jobs as usize - 1)
                .chain_err(|| "failed to create jobserver")?,
        };
        let mut cx = Context {
            ws,
            resolve,
            packages,
            config,
            target_info: TargetInfo::default(),
            host_info: TargetInfo::default(),
            compilation: Compilation::new(config),
            build_state: Arc::new(BuildState::new(&build_config)),
            build_config,
            fingerprints: HashMap::new(),
            profiles,
            compiled: HashSet::new(),
            build_scripts: HashMap::new(),
            build_explicit_deps: HashMap::new(),
            links: Links::new(),
            used_in_plugin: HashSet::new(),
            incremental_env,
            jobserver,
            build_script_overridden: HashSet::new(),

            unit_dependencies: HashMap::new(),
            files: None,
        };

        cx.probe_target_info()?;
        let deps = build_unit_dependencies(units, &cx)?;
        cx.unit_dependencies = deps;
        let files = CompilationFiles::new(units, host_layout, target_layout, ws, &cx);
        cx.files = Some(files);
        Ok(cx)
    }

    /// Prepare this context, ensuring that all filesystem directories are in
    /// place.
    pub fn prepare(&mut self) -> CargoResult<()> {
        let _p = profile::start("preparing layout");

        self.files_mut()
            .host
            .prepare()
            .chain_err(|| internal("couldn't prepare build directories"))?;
        if let Some(ref mut target) = self.files.as_mut().unwrap().target {
            target
                .prepare()
                .chain_err(|| internal("couldn't prepare build directories"))?;
        }

        self.compilation.host_deps_output = self.files_mut().host.deps().to_path_buf();

        let files = self.files.as_ref().unwrap();
        let layout = files.target.as_ref().unwrap_or(&files.host);
        self.compilation.root_output = layout.dest().to_path_buf();
        self.compilation.deps_output = layout.deps().to_path_buf();
        Ok(())
    }

    /// Ensure that we've collected all target-specific information to compile
    /// all the units mentioned in `units`.
    fn probe_target_info(&mut self) -> CargoResult<()> {
        debug!("probe_target_info");
        let host_target_same = match self.requested_target() {
            Some(s) if s != self.config.rustc()?.host => false,
            _ => true,
        };

        if host_target_same {
            let info = self.probe_target_info_kind(Kind::Target)?;
            self.host_info = info.clone();
            self.target_info = info;
        } else {
            self.host_info = self.probe_target_info_kind(Kind::Host)?;
            self.target_info = self.probe_target_info_kind(Kind::Target)?;
        }
        self.compilation.host_dylib_path = self.host_info.sysroot_libdir.clone();
        self.compilation.target_dylib_path = self.target_info.sysroot_libdir.clone();
        Ok(())
    }

    fn probe_target_info_kind(&self, kind: Kind) -> CargoResult<TargetInfo> {
        let rustflags = env_args(
            self.config,
            &self.build_config,
            self.info(&kind),
            kind,
            "RUSTFLAGS",
        )?;
        let mut process = self.config.rustc()?.process();
        process
            .arg("-")
            .arg("--crate-name")
            .arg("___")
            .arg("--print=file-names")
            .args(&rustflags)
            .env_remove("RUST_LOG");

        if kind == Kind::Target {
            process.arg("--target").arg(&self.target_triple());
        }

        let crate_type_process = process.clone();
        const KNOWN_CRATE_TYPES: &[&str] =
            &["bin", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];
        for crate_type in KNOWN_CRATE_TYPES.iter() {
            process.arg("--crate-type").arg(crate_type);
        }

        let mut with_cfg = process.clone();
        with_cfg.arg("--print=sysroot");
        with_cfg.arg("--print=cfg");

        let mut has_cfg_and_sysroot = true;
        let output = with_cfg
            .exec_with_output()
            .or_else(|_| {
                has_cfg_and_sysroot = false;
                process.exec_with_output()
            })
            .chain_err(|| "failed to run `rustc` to learn about target-specific information")?;

        let error = str::from_utf8(&output.stderr).unwrap();
        let output = str::from_utf8(&output.stdout).unwrap();
        let mut lines = output.lines();
        let mut map = HashMap::new();
        for crate_type in KNOWN_CRATE_TYPES {
            let out = parse_crate_type(crate_type, error, &mut lines)?;
            map.insert(crate_type.to_string(), out);
        }

        let mut sysroot_libdir = None;
        if has_cfg_and_sysroot {
            let line = match lines.next() {
                Some(line) => line,
                None => bail!(
                    "output of --print=sysroot missing when learning about \
                     target-specific information from rustc"
                ),
            };
            let mut rustlib = PathBuf::from(line);
            if kind == Kind::Host {
                if cfg!(windows) {
                    rustlib.push("bin");
                } else {
                    rustlib.push("lib");
                }
                sysroot_libdir = Some(rustlib);
            } else {
                rustlib.push("lib");
                rustlib.push("rustlib");
                rustlib.push(self.target_triple());
                rustlib.push("lib");
                sysroot_libdir = Some(rustlib);
            }
        }

        let cfg = if has_cfg_and_sysroot {
            Some(lines.map(Cfg::from_str).collect::<CargoResult<_>>()?)
        } else {
            None
        };

        Ok(TargetInfo {
            crate_type_process: Some(crate_type_process),
            crate_types: RefCell::new(map),
            cfg,
            sysroot_libdir,
        })
    }

    /// Builds up the `used_in_plugin` internal to this context from the list of
    /// top-level units.
    ///
    /// This will recursively walk `units` and all of their dependencies to
    /// determine which crate are going to be used in plugins or not.
    pub fn build_used_in_plugin_map(&mut self, units: &[Unit<'a>]) -> CargoResult<()> {
        let mut visited = HashSet::new();
        for unit in units {
            self.walk_used_in_plugin_map(unit, unit.target.for_host(), &mut visited)?;
        }
        Ok(())
    }

    fn walk_used_in_plugin_map(
        &mut self,
        unit: &Unit<'a>,
        is_plugin: bool,
        visited: &mut HashSet<(Unit<'a>, bool)>,
    ) -> CargoResult<()> {
        if !visited.insert((*unit, is_plugin)) {
            return Ok(());
        }
        if is_plugin {
            self.used_in_plugin.insert(*unit);
        }
        for unit in self.dep_targets(unit) {
            self.walk_used_in_plugin_map(&unit, is_plugin || unit.target.for_host(), visited)?;
        }
        Ok(())
    }

    pub fn files(&self) -> &CompilationFiles<'a, 'cfg> {
        self.files.as_ref().unwrap()
    }

    fn files_mut(&mut self) -> &mut CompilationFiles<'a, 'cfg> {
        self.files.as_mut().unwrap()
    }

    /// Return the host triple for this context
    pub fn host_triple(&self) -> &str {
        &self.build_config.host_triple
    }

    /// Return the target triple which this context is targeting.
    pub fn target_triple(&self) -> &str {
        self.requested_target()
            .unwrap_or_else(|| self.host_triple())
    }

    /// Requested (not actual) target for the build
    pub fn requested_target(&self) -> Option<&str> {
        self.build_config.requested_target.as_ref().map(|s| &s[..])
    }

    /// Return the filenames that the given target for the given profile will
    /// generate as a list of 3-tuples (filename, link_dst, linkable)
    ///
    ///  - filename: filename rustc compiles to. (Often has metadata suffix).
    ///  - link_dst: Optional file to link/copy the result to (without metadata suffix)
    ///  - linkable: Whether possible to link against file (eg it's a library)
    pub fn target_filenames(
        &mut self,
        unit: &Unit<'a>,
    ) -> CargoResult<Arc<Vec<(PathBuf, Option<PathBuf>, TargetFileType)>>> {
        self.files.as_ref().unwrap().target_filenames(unit, self)
    }

    /// For a package, return all targets which are registered as dependencies
    /// for that package.
    // TODO: this ideally should be `-> &[Unit<'a>]`
    pub fn dep_targets(&self, unit: &Unit<'a>) -> Vec<Unit<'a>> {
        // If this build script's execution has been overridden then we don't
        // actually depend on anything, we've reached the end of the dependency
        // chain as we've got all the info we're gonna get.
        //
        // Note there's a subtlety about this piece of code! The
        // `build_script_overridden` map here is populated in
        // `custom_build::build_map` which you need to call before inspecting
        // dependencies. However, that code itself calls this method and
        // gets a full pre-filtered set of dependencies. This is not super
        // obvious, and clear, but it does work at the moment.
        if unit.profile.run_custom_build {
            let key = (unit.pkg.package_id().clone(), unit.kind);
            if self.build_script_overridden.contains(&key) {
                return Vec::new();
            }
        }
        self.unit_dependencies[unit].clone()
    }

    fn dep_platform_activated(&self, dep: &Dependency, kind: Kind) -> bool {
        // If this dependency is only available for certain platforms,
        // make sure we're only enabling it for that platform.
        let platform = match dep.platform() {
            Some(p) => p,
            None => return true,
        };
        let (name, info) = match kind {
            Kind::Host => (self.host_triple(), &self.host_info),
            Kind::Target => (self.target_triple(), &self.target_info),
        };
        platform.matches(name, info.cfg.as_ref().map(|cfg| &cfg[..]))
    }

    /// Gets a package for the given package id.
    pub fn get_package(&self, id: &PackageId) -> CargoResult<&'a Package> {
        self.packages.get(id)
    }

    /// Get the user-specified linker for a particular host or target
    pub fn linker(&self, kind: Kind) -> Option<&Path> {
        self.target_config(kind).linker.as_ref().map(|s| s.as_ref())
    }

    /// Get the user-specified `ar` program for a particular host or target
    pub fn ar(&self, kind: Kind) -> Option<&Path> {
        self.target_config(kind).ar.as_ref().map(|s| s.as_ref())
    }

    /// Get the list of cfg printed out from the compiler for the specified kind
    pub fn cfg(&self, kind: Kind) -> &[Cfg] {
        let info = match kind {
            Kind::Host => &self.host_info,
            Kind::Target => &self.target_info,
        };
        info.cfg.as_ref().map(|s| &s[..]).unwrap_or(&[])
    }

    /// Get the target configuration for a particular host or target
    fn target_config(&self, kind: Kind) -> &TargetConfig {
        match kind {
            Kind::Host => &self.build_config.host,
            Kind::Target => &self.build_config.target,
        }
    }

    /// Number of jobs specified for this build
    pub fn jobs(&self) -> u32 {
        self.build_config.jobs
    }

    pub fn lib_profile(&self) -> &'a Profile {
        let (normal, test) = if self.build_config.release {
            (&self.profiles.release, &self.profiles.bench_deps)
        } else {
            (&self.profiles.dev, &self.profiles.test_deps)
        };
        if self.build_config.test {
            test
        } else {
            normal
        }
    }

    pub fn build_script_profile(&self, _pkg: &PackageId) -> &'a Profile {
        // TODO: should build scripts always be built with the same library
        //       profile? How is this controlled at the CLI layer?
        self.lib_profile()
    }

    pub fn incremental_args(&self, unit: &Unit) -> CargoResult<Vec<String>> {
        // There's a number of ways to configure incremental compilation right
        // now. In order of descending priority (first is highest priority) we
        // have:
        //
        // * `CARGO_INCREMENTAL` - this is blanket used unconditionally to turn
        //   on/off incremental compilation for any cargo subcommand. We'll
        //   respect this if set.
        // * `build.incremental` - in `.cargo/config` this blanket key can
        //   globally for a system configure whether incremental compilation is
        //   enabled. Note that setting this to `true` will not actually affect
        //   all builds though. For example a `true` value doesn't enable
        //   release incremental builds, only dev incremental builds. This can
        //   be useful to globally disable incremental compilation like
        //   `CARGO_INCREMENTAL`.
        // * `profile.dev.incremental` - in `Cargo.toml` specific profiles can
        //   be configured to enable/disable incremental compilation. This can
        //   be primarily used to disable incremental when buggy for a project.
        // * Finally, each profile has a default for whether it will enable
        //   incremental compilation or not. Primarily development profiles
        //   have it enabled by default while release profiles have it disabled
        //   by default.
        let global_cfg = self.config.get_bool("build.incremental")?.map(|c| c.val);
        let incremental = match (self.incremental_env, global_cfg, unit.profile.incremental) {
            (Some(v), _, _) => v,
            (None, Some(false), _) => false,
            (None, _, other) => other,
        };

        if !incremental {
            return Ok(Vec::new());
        }

        // Only enable incremental compilation for sources the user can
        // modify (aka path sources). For things that change infrequently,
        // non-incremental builds yield better performance in the compiler
        // itself (aka crates.io / git dependencies)
        //
        // (see also https://github.com/rust-lang/cargo/issues/3972)
        if !unit.pkg.package_id().source_id().is_path() {
            return Ok(Vec::new());
        }

        let dir = self.files().layout(unit.kind).incremental().display();
        Ok(vec!["-C".to_string(), format!("incremental={}", dir)])
    }

    pub fn rustflags_args(&self, unit: &Unit) -> CargoResult<Vec<String>> {
        env_args(
            self.config,
            &self.build_config,
            self.info(&unit.kind),
            unit.kind,
            "RUSTFLAGS",
        )
    }

    pub fn rustdocflags_args(&self, unit: &Unit) -> CargoResult<Vec<String>> {
        env_args(
            self.config,
            &self.build_config,
            self.info(&unit.kind),
            unit.kind,
            "RUSTDOCFLAGS",
        )
    }

    pub fn show_warnings(&self, pkg: &PackageId) -> bool {
        pkg.source_id().is_path() || self.config.extra_verbose()
    }

    fn info(&self, kind: &Kind) -> &TargetInfo {
        match *kind {
            Kind::Host => &self.host_info,
            Kind::Target => &self.target_info,
        }
    }
}

/// Acquire extra flags to pass to the compiler from various locations.
///
/// The locations are:
///
///  - the `RUSTFLAGS` environment variable
///
/// then if this was not found
///
///  - `target.*.rustflags` from the manifest (Cargo.toml)
///  - `target.cfg(..).rustflags` from the manifest
///
/// then if neither of these were found
///
///  - `build.rustflags` from the manifest
///
/// Note that if a `target` is specified, no args will be passed to host code (plugins, build
/// scripts, ...), even if it is the same as the target.
fn env_args(
    config: &Config,
    build_config: &BuildConfig,
    target_info: &TargetInfo,
    kind: Kind,
    name: &str,
) -> CargoResult<Vec<String>> {
    // We *want* to apply RUSTFLAGS only to builds for the
    // requested target architecture, and not to things like build
    // scripts and plugins, which may be for an entirely different
    // architecture. Cargo's present architecture makes it quite
    // hard to only apply flags to things that are not build
    // scripts and plugins though, so we do something more hacky
    // instead to avoid applying the same RUSTFLAGS to multiple targets
    // arches:
    //
    // 1) If --target is not specified we just apply RUSTFLAGS to
    // all builds; they are all going to have the same target.
    //
    // 2) If --target *is* specified then we only apply RUSTFLAGS
    // to compilation units with the Target kind, which indicates
    // it was chosen by the --target flag.
    //
    // This means that, e.g. even if the specified --target is the
    // same as the host, build scripts in plugins won't get
    // RUSTFLAGS.
    let compiling_with_target = build_config.requested_target.is_some();
    let is_target_kind = kind == Kind::Target;

    if compiling_with_target && !is_target_kind {
        // This is probably a build script or plugin and we're
        // compiling with --target. In this scenario there are
        // no rustflags we can apply.
        return Ok(Vec::new());
    }

    // First try RUSTFLAGS from the environment
    if let Ok(a) = env::var(name) {
        let args = a.split(' ')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        return Ok(args.collect());
    }

    let mut rustflags = Vec::new();

    let name = name.chars()
        .flat_map(|c| c.to_lowercase())
        .collect::<String>();
    // Then the target.*.rustflags value...
    let target = build_config
        .requested_target
        .as_ref()
        .unwrap_or(&build_config.host_triple);
    let key = format!("target.{}.{}", target, name);
    if let Some(args) = config.get_list_or_split_string(&key)? {
        let args = args.val.into_iter();
        rustflags.extend(args);
    }
    // ...including target.'cfg(...)'.rustflags
    if let Some(ref target_cfg) = target_info.cfg {
        if let Some(table) = config.get_table("target")? {
            let cfgs = table.val.keys().filter_map(|t| {
                if t.starts_with("cfg(") && t.ends_with(')') {
                    let cfg = &t[4..t.len() - 1];
                    CfgExpr::from_str(cfg).ok().and_then(|c| {
                        if c.matches(target_cfg) {
                            Some(t)
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            });

            // Note that we may have multiple matching `[target]` sections and
            // because we're passing flags to the compiler this can affect
            // cargo's caching and whether it rebuilds. Ensure a deterministic
            // ordering through sorting for now. We may perhaps one day wish to
            // ensure a deterministic ordering via the order keys were defined
            // in files perhaps.
            let mut cfgs = cfgs.collect::<Vec<_>>();
            cfgs.sort();

            for n in cfgs {
                let key = format!("target.{}.{}", n, name);
                if let Some(args) = config.get_list_or_split_string(&key)? {
                    let args = args.val.into_iter();
                    rustflags.extend(args);
                }
            }
        }
    }

    if !rustflags.is_empty() {
        return Ok(rustflags);
    }

    // Then the build.rustflags value
    let key = format!("build.{}", name);
    if let Some(args) = config.get_list_or_split_string(&key)? {
        let args = args.val.into_iter();
        return Ok(args.collect());
    }

    Ok(Vec::new())
}

/// Takes rustc output (using specialized command line args), and calculates the file prefix and
/// suffix for the given crate type, or returns None if the type is not supported. (e.g. for a
/// rust library like libcargo.rlib, prefix = "lib", suffix = "rlib").
///
/// The caller needs to ensure that the lines object is at the correct line for the given crate
/// type: this is not checked.
// This function can not handle more than 1 file per type (with wasm32-unknown-emscripten, there
// are 2 files for bin (.wasm and .js))
fn parse_crate_type(
    crate_type: &str,
    error: &str,
    lines: &mut str::Lines,
) -> CargoResult<Option<(String, String)>> {
    let not_supported = error.lines().any(|line| {
        (line.contains("unsupported crate type") || line.contains("unknown crate type"))
            && line.contains(crate_type)
    });
    if not_supported {
        return Ok(None);
    }
    let line = match lines.next() {
        Some(line) => line,
        None => bail!(
            "malformed output when learning about \
             crate-type {} information",
            crate_type
        ),
    };
    let mut parts = line.trim().split("___");
    let prefix = parts.next().unwrap();
    let suffix = match parts.next() {
        Some(part) => part,
        None => bail!(
            "output of --print=file-names has changed in \
             the compiler, cannot parse"
        ),
    };

    Ok(Some((prefix.to_string(), suffix.to_string())))
}