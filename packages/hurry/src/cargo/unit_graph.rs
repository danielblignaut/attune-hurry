use std::hash::{Hash, Hasher};

use color_eyre::{Result, eyre::Context as _};
use serde::Deserialize;

use crate::{
    cargo::{CargoCompileMode, RustcTarget, RustcTargetPlatform},
    path::AbsFilePath,
};

/// A UnitGraph represents the output of `cargo build --unit-graph`. This output
/// is documented here[^1] and defined in source code here[^2].
///
/// [^1]: https://doc.rust-lang.org/cargo/reference/unstable.html#unit-graph
/// [^2]: https://github.com/rust-lang/cargo/blob/c24e1064277fe51ab72011e2612e556ac56addf7/src/cargo/core/compiler/unit_graph.rs#L43-L48
#[derive(Clone, Debug, Deserialize)]
pub struct UnitGraph {
    pub version: u64,
    pub units: Vec<UnitGraphUnit>,
    pub roots: Vec<usize>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UnitGraphUnit {
    pub pkg_id: String,
    pub target: cargo_metadata::Target,
    pub profile: UnitGraphProfile,
    pub platform: Option<String>,
    pub mode: CargoCompileMode,
    pub features: Vec<String>,
    #[serde(skip)]
    pub is_std: bool,
    pub dependencies: Vec<UnitGraphDependency>,
}

impl UnitGraphUnit {
    pub fn package_name_version(&self) -> Result<(String, String)> {
        let id = self
            .pkg_id
            .rsplit_once('#')
            .map(|(_, id)| id)
            .unwrap_or(self.pkg_id.as_str());
        if let Some((name, version)) = id.rsplit_once('@') {
            return Ok((String::from(name), String::from(version)));
        }

        let version = id
            .rsplit_once('#')
            .map(|(_, version)| version)
            .unwrap_or(id);
        Ok((self.target.name.clone(), String::from(version)))
    }

    pub fn crate_name(&self) -> String {
        self.target.name.replace('-', "_")
    }

    pub fn src_path(&self) -> Result<AbsFilePath> {
        self.target
            .src_path
            .as_std_path()
            .try_into()
            .context("parse unit graph target src_path")
    }

    pub fn target_arch(&self) -> RustcTarget {
        self.platform
            .as_deref()
            .map(|platform| {
                RustcTarget::Specified(
                    RustcTargetPlatform::try_from_str(platform)
                        .unwrap_or(RustcTargetPlatform::Unsupported(String::from(platform))),
                )
            })
            .unwrap_or(RustcTarget::ImplicitHost)
    }

    pub fn is_build_script_compilation(&self) -> bool {
        self.target.kind == [cargo_metadata::TargetKind::CustomBuild]
            && self.mode == CargoCompileMode::Build
    }

    pub fn is_build_script_execution(&self) -> bool {
        self.target.kind == [cargo_metadata::TargetKind::CustomBuild]
            && self.mode == CargoCompileMode::RunCustomBuild
    }

    pub fn is_binary(&self) -> bool {
        self.target.kind == [cargo_metadata::TargetKind::Bin]
    }

    pub fn is_cacheable_library(&self) -> bool {
        self.mode == CargoCompileMode::Build
            && (self.target.kind.contains(&cargo_metadata::TargetKind::Lib)
                || self.target.kind.contains(&cargo_metadata::TargetKind::RLib)
                || self
                    .target
                    .kind
                    .contains(&cargo_metadata::TargetKind::CDyLib)
                || self
                    .target
                    .kind
                    .contains(&cargo_metadata::TargetKind::DyLib)
                || self
                    .target
                    .kind
                    .contains(&cargo_metadata::TargetKind::StaticLib)
                || self
                    .target
                    .kind
                    .contains(&cargo_metadata::TargetKind::ProcMacro))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct UnitGraphProfile {
    pub name: String,
    pub opt_level: String,
    pub lto: String,
    #[serde(default)]
    pub codegen_backend: Option<String>,
    pub codegen_units: Option<u64>,
    pub debuginfo: Option<u64>,
    #[serde(default)]
    pub split_debuginfo: Option<String>,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub rpath: bool,
    pub incremental: bool,
    pub panic: UnitGraphProfilePanicStrategy,
    #[serde(default)]
    pub strip: Option<serde_json::Value>,
}

impl Hash for UnitGraphProfile {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.opt_level.hash(state);
        self.lto.hash(state);
        self.codegen_backend.hash(state);
        self.codegen_units.hash(state);
        self.debuginfo.hash(state);
        self.split_debuginfo.hash(state);
        self.debug_assertions.hash(state);
        self.overflow_checks.hash(state);
        self.rpath.hash(state);
        self.incremental.hash(state);
        self.panic.hash(state);
        self.strip.hash(state);
    }
}

#[derive(Clone, Debug, Deserialize, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum UnitGraphProfilePanicStrategy {
    Unwind,
    Abort,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UnitGraphDependency {
    pub index: usize,
    pub extern_crate_name: String,
    pub public: bool,
    pub noprelude: bool,
}

impl Hash for UnitGraphUnit {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.pkg_id.hash(state);
        self.target.name.hash(state);
        self.target.kind.hash(state);
        self.profile.hash(state);
        self.platform.hash(state);
        self.mode.hash(state);
        self.features.hash(state);
        self.is_std.hash(state);
    }
}
