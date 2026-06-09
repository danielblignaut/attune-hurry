use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
};

use color_eyre::Result;
use rustc_stable_hash::StableSipHasher128;

use crate::cargo::{UnitGraphUnit, unit_graph::parse_package_id_source};

const METADATA_VERSION: u8 = 2;

#[derive(Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Metadata {
    pub unit_id: u64,
    pub c_metadata: u64,
}

impl Metadata {
    pub fn unit_hash(self) -> String {
        format!("{:016x}", self.unit_id)
    }
}

pub fn compute(
    unit: &UnitGraphUnit,
    deps: &[Metadata],
    rustc_verbose_version: &str,
    host_unit: bool,
    workspace_root: &str,
) -> Result<Metadata> {
    let mut shared = StableSipHasher128::new();

    METADATA_VERSION.hash(&mut shared);
    hash_package_id(&unit.pkg_id, workspace_root, &mut shared)?;
    unit.features.hash(&mut shared);
    unit.profile.hash(&mut shared);
    unit.mode.hash(&mut shared);

    // Cargo mixes LTO state here. Unit graph does not expose the resolved LTO
    // enum, so default builds use the profile lto representation.
    unit.profile.lto.hash(&mut shared);

    compile_kind_fingerprint_hash(unit.platform.as_deref()).hash(&mut shared);
    unit.target.name.hash(&mut shared);
    unit.target.kind.hash(&mut shared);
    hash_rustc_version(rustc_verbose_version, host_unit, &mut shared);
    unit.is_std.hash(&mut shared);

    let mut c_metadata_hasher = shared.clone();
    let mut dep_c_metadata = deps.iter().map(|dep| dep.c_metadata).collect::<Vec<_>>();
    dep_c_metadata.sort();
    dep_c_metadata.hash(&mut c_metadata_hasher);

    let mut unit_id_hasher = shared;
    let mut dep_unit_ids = deps.iter().map(|dep| dep.unit_id).collect::<Vec<_>>();
    dep_unit_ids.sort();
    dep_unit_ids.hash(&mut unit_id_hasher);

    // Cargo also hashes resolved extra rustc/rustdoc flags unless they include
    // remap-path-prefix. Unit graph does not expose those flags, so the default
    // empty set is used here.
    Vec::<String>::new().hash(&mut unit_id_hasher);

    Ok(Metadata {
        unit_id: Hasher::finish(&unit_id_hasher),
        c_metadata: Hasher::finish(&c_metadata_hasher),
    })
}

pub fn compute_all(
    units: &[UnitGraphUnit],
    rustc_verbose_version: &str,
    workspace_root: &str,
) -> Result<HashMap<usize, Metadata>> {
    let mut metadata = HashMap::new();
    for index in 0..units.len() {
        compute_one(
            index,
            units,
            rustc_verbose_version,
            workspace_root,
            &mut metadata,
        )?;
    }
    Ok(metadata)
}

fn compute_one(
    index: usize,
    units: &[UnitGraphUnit],
    rustc_verbose_version: &str,
    workspace_root: &str,
    metadata: &mut HashMap<usize, Metadata>,
) -> Result<Metadata> {
    if let Some(existing) = metadata.get(&index) {
        return Ok(*existing);
    }

    let unit = &units[index];
    let mut deps = Vec::new();
    for dep in &unit.dependencies {
        deps.push(compute_one(
            dep.index,
            units,
            rustc_verbose_version,
            workspace_root,
            metadata,
        )?);
    }

    let host_unit = unit.platform.is_none();
    let computed = compute(
        unit,
        &deps,
        rustc_verbose_version,
        host_unit,
        workspace_root,
    )?;
    metadata.insert(index, computed);
    Ok(computed)
}

fn hash_package_id<H: Hasher>(pkg_id: &str, workspace_root: &str, state: &mut H) -> Result<()> {
    let (source, package) = parse_package_id_source(pkg_id)?;
    let (name, version) = package
        .rsplit_once('@')
        .map(|(name, version)| (name, version))
        .unwrap_or((package, ""));

    name.hash(state);
    version.hash(state);
    hash_source_id(source, workspace_root, state);
    Ok(())
}

fn hash_source_id<H: Hasher>(source: &str, workspace_root: &str, state: &mut H) {
    let (kind, url) = source
        .split_once('+')
        .map(|(kind, url)| (kind, url))
        .unwrap_or((source, ""));
    kind.hash(state);

    if kind == "path" {
        let prefix = format!("file://{workspace_root}/");
        if let Some(relative) = url.strip_prefix(&prefix) {
            relative.hash(state);
            return;
        }
    }

    url.hash(state);
}

fn compile_kind_fingerprint_hash(platform: Option<&str>) -> u64 {
    let Some(platform) = platform else {
        return 0;
    };

    let mut hasher = StableSipHasher128::new();
    platform.hash(&mut hasher);
    Hasher::finish(&hasher)
}

fn hash_rustc_version<H: Hasher>(verbose_version: &str, host_unit: bool, state: &mut H) {
    for line in verbose_version.lines() {
        if host_unit || !line.starts_with("host: ") {
            line.hash(state);
        }
    }
}
