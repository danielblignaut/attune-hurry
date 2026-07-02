use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use color_eyre::{
    Report, Result,
    eyre::{OptionExt as _, bail},
};
use futures::stream;
use serde::{Deserialize, Serialize};
use tap::{Conv as _, Pipe as _};
use tracing::{debug, error, instrument, trace};

use crate::{
    cargo::{
        Fingerprint, QualifiedPath, Restored, RustcTarget, UnitPlan, Workspace, host_glibc_version,
    },
    cas::CourierCas,
    fs,
    path::{AbsDirPath, AbsFilePath, JoinWith as _},
};
use clients::{
    Courier,
    courier::v1::{
        self as courier, Key,
        cache::{CargoSaveRequest, CargoSaveUnitRequest},
    },
};

#[derive(Clone)]
struct FingerprintRewriteInput {
    target: RustcTarget,
    src_path: Option<AbsFilePath>,
    fingerprint: Fingerprint,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct SaveProgress {
    pub uploaded_units: u64,
    pub total_units: u64,
    pub uploaded_files: u64,
    pub uploaded_bytes: u64,
}

#[instrument(skip_all)]
pub async fn save_units(
    courier: &Courier,
    cas: &CourierCas,
    ws: Workspace,
    units: Vec<UnitPlan>,
    skip: Restored,
    mut on_progress: impl FnMut(&SaveProgress),
) -> Result<()> {
    trace!(?units, ?skip, "saving units");

    // TODO: This algorithm currently uploads units one at a time. Instead, we
    // should batch units together up to around 10MB in file size for optimal
    // upload speed. One way we could do this is have units present their
    // CAS-able contents, batch those contents up, and then issue save requests
    // for batches of units as their CAS contents are finished uploading.
    let mut save_requests = Vec::new();
    let mut fingerprint_inputs = HashMap::new();
    let mut units_to_save = Vec::new();
    for unit in units {
        if !has_fingerprint_files(&ws, &unit).await? {
            debug!(?unit, "skipping unit backup: fingerprint files are missing");
            continue;
        }

        let fingerprint = unit.read_fingerprint(&ws).await?;
        let old_hash = fingerprint.hash_u64();
        fingerprint_inputs.insert(
            old_hash,
            FingerprintRewriteInput {
                target: unit.info().target_arch.clone(),
                src_path: unit.src_path(),
                fingerprint,
            },
        );
        units_to_save.push(unit);
    }

    let mut progress = SaveProgress {
        uploaded_units: 0,
        total_units: units_to_save.len() as u64,
        uploaded_files: 0,
        uploaded_bytes: 0,
    };

    let mut dep_fingerprints = HashMap::new();
    for unit in units_to_save {
        debug!(?unit, "saving unit");
        if skip.units.contains(&unit.info().unit_hash) {
            debug!(?unit, "skipping unit backup: unit was restored from cache");
            progress.total_units -= 1;
            on_progress(&progress);

            // Even skipped units need to have their rewritten fingerprints
            // calculated, so that we have those values ready in case these
            // units are `dep`s of a downstream unit that is not skipped.
            let _ = rewrite_fingerprint_for_save(
                &ws,
                &unit.info().target_arch,
                unit.src_path(),
                &fingerprint_inputs,
                &mut dep_fingerprints,
                unit.read_fingerprint(&ws).await?,
            )
            .await?;

            continue;
        }

        // For units compiled against glibc, we need to know the glibc version
        // so we don't later restore the unit on a host machine that does not
        // have the needed glibc symbols.
        let unit_arch = match &unit.info().target_arch {
            RustcTarget::Specified(target_arch) => &target_arch.clone(),
            RustcTarget::ImplicitHost => &ws.host_arch,
        };
        let glibc_version = if unit_arch.uses_glibc() {
            if unit_arch != &ws.host_arch {
                // TODO: How do we determine the glibc version of a
                // cross-compiled unit? Maybe for `cross`, we can add
                // first-class support where we inspect the inside of the
                // container for its libc version? What about in general for
                // other cross-compilers? How do we know which libc the compiler
                // will link against?
                //
                // See also:
                // - https://stackoverflow.com/questions/61423973/how-to-find-which-libc-so-will-rustc-target-target-link-against
                // - https://github.com/rust-lang/rust/issues/71564
                // - https://users.rust-lang.org/t/clarifications-on-rusts-relationship-to-libc/56767/2
                //
                // Maybe we can directly ask the native compilers? `cc
                // --print-file-name=libc.so.6` and `aarch64-linux-gnu-gcc
                // --print-file-name=libc.so.6`? And from then we can open the
                // ELF and look at the verdef section? But how do we know which
                // linker Cargo will use for any particular build, and what flag
                // that linker accepts to query the libc file?
                error!("backing up cross-compiled units is not yet supported");
                progress.total_units -= 1;
                on_progress(&progress);
                continue;
            }
            // TODO: This isn't _technically_ correct. You could, in theory,
            // configure Cargo or your linker to link against against a version
            // of glibc different from your standard glibc. I'm not completely
            // sure how we would query that out of Cargo, rustc, or the linker,
            // (maybe `cc --print-filename=libc.so.6` when we can infer that the
            // linker is `cc`, or emulating `LD_LIBRARY_PATH` when it's `ld`?),
            // so for now such a configuration is unsupported.
            host_glibc_version()?
        } else {
            None
        };

        // Upload unit to CAS and cache.
        match unit {
            UnitPlan::LibraryCrate(plan) => {
                // Read unit files.
                let files = match plan.read(&ws).await {
                    Ok(files) => files,
                    Err(error) if is_missing_unit_file(&error) => {
                        debug!(?error, "skipping unit backup: unit files are missing");
                        progress.total_units -= 1;
                        on_progress(&progress);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let Some(fingerprint) = rewrite_fingerprint_for_save(
                    &ws,
                    &plan.info.target_arch,
                    Some(plan.src_path.clone()),
                    &fingerprint_inputs,
                    &mut dep_fingerprints,
                    files.fingerprint.clone(),
                )
                .await?
                else {
                    progress.total_units -= 1;
                    on_progress(&progress);
                    continue;
                };

                // Prepare CAS objects.
                let mut cas_uploads = Vec::new();

                let mut output_files = Vec::new();
                for output_file in files.output_files {
                    let object_key = Key::from_buffer(&output_file.contents);
                    output_files.push(
                        courier::SavedFile::builder()
                            .object_key(object_key.clone())
                            .executable(output_file.executable)
                            .path(serde_json::to_string(&output_file.path)?)
                            .build(),
                    );

                    if !skip.files.contains(&object_key) {
                        progress.uploaded_files += 1;
                        progress.uploaded_bytes += output_file.contents.len() as u64;
                        cas_uploads.push((object_key, output_file.contents));
                    }
                }

                let dep_info_file_contents = serde_json::to_vec(&files.dep_info_file)?;
                let dep_info_file = Key::from_buffer(&dep_info_file_contents);
                if !skip.files.contains(&dep_info_file) {
                    progress.uploaded_files += 1;
                    progress.uploaded_bytes += dep_info_file_contents.len() as u64;
                    cas_uploads.push((dep_info_file.clone(), dep_info_file_contents));
                }

                let encoded_dep_info_file = Key::from_buffer(&files.encoded_dep_info_file);
                if !skip.files.contains(&encoded_dep_info_file) {
                    progress.uploaded_files += 1;
                    progress.uploaded_bytes += files.encoded_dep_info_file.len() as u64;
                    cas_uploads.push((encoded_dep_info_file.clone(), files.encoded_dep_info_file));
                }

                // Save CAS objects.
                if !cas_uploads.is_empty() {
                    cas.store_bulk(stream::iter(cas_uploads)).await?;
                }

                // Prepare save request.
                let save_request = CargoSaveUnitRequest::builder()
                    .unit(courier::SavedUnit::LibraryCrate(
                        courier::LibraryFiles::builder()
                            .output_files(output_files)
                            .dep_info_file(dep_info_file)
                            .encoded_dep_info_file(encoded_dep_info_file)
                            .fingerprint(fingerprint)
                            .build(),
                        plan.try_into()?,
                    ))
                    .resolved_target(unit_arch.as_str().to_string())
                    .maybe_linux_glibc_version(glibc_version)
                    .build();

                save_requests.push(save_request);
            }
            UnitPlan::BuildScriptCompilation(plan) => {
                // Read unit files.
                let files = match plan.read(&ws).await {
                    Ok(files) => files,
                    Err(error) if is_missing_unit_file(&error) => {
                        debug!(?error, "skipping unit backup: unit files are missing");
                        progress.total_units -= 1;
                        on_progress(&progress);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let Some(fingerprint) = rewrite_fingerprint_for_save(
                    &ws,
                    &plan.info.target_arch,
                    Some(plan.src_path.clone()),
                    &fingerprint_inputs,
                    &mut dep_fingerprints,
                    files.fingerprint.clone(),
                )
                .await?
                else {
                    progress.total_units -= 1;
                    on_progress(&progress);
                    continue;
                };

                // Prepare CAS objects.
                let mut cas_uploads = Vec::new();

                let compiled_program = Key::from_buffer(&files.compiled_program);
                if !skip.files.contains(&compiled_program) {
                    progress.uploaded_files += 1;
                    progress.uploaded_bytes += files.compiled_program.len() as u64;
                    cas_uploads.push((compiled_program.clone(), files.compiled_program));
                }

                let dep_info_file_contents = serde_json::to_vec(&files.dep_info_file)?;
                let dep_info_file = Key::from_buffer(&dep_info_file_contents);
                if !skip.files.contains(&dep_info_file) {
                    progress.uploaded_files += 1;
                    progress.uploaded_bytes += dep_info_file_contents.len() as u64;
                    cas_uploads.push((dep_info_file.clone(), dep_info_file_contents));
                }

                let encoded_dep_info_file = Key::from_buffer(&files.encoded_dep_info_file);
                if !skip.files.contains(&encoded_dep_info_file) {
                    progress.uploaded_files += 1;
                    progress.uploaded_bytes += files.encoded_dep_info_file.len() as u64;
                    cas_uploads.push((encoded_dep_info_file.clone(), files.encoded_dep_info_file));
                }

                // Save CAS objects.
                if !cas_uploads.is_empty() {
                    cas.store_bulk(stream::iter(cas_uploads)).await?;
                }

                // Prepare save request.
                let save_request = CargoSaveUnitRequest::builder()
                    .unit(courier::SavedUnit::BuildScriptCompilation(
                        courier::BuildScriptCompiledFiles::builder()
                            .compiled_program(compiled_program)
                            .dep_info_file(dep_info_file)
                            .fingerprint(fingerprint)
                            .encoded_dep_info_file(encoded_dep_info_file)
                            .build(),
                        plan.try_into()?,
                    ))
                    .resolved_target(unit_arch.as_str().to_string())
                    .maybe_linux_glibc_version(glibc_version)
                    .build();

                save_requests.push(save_request);
            }
            UnitPlan::BuildScriptExecution(plan) => {
                // Read unit files.
                let files = match plan.read(&ws).await {
                    Ok(files) => files,
                    Err(error) if is_missing_unit_file(&error) => {
                        debug!(?error, "skipping unit backup: unit files are missing");
                        progress.total_units -= 1;
                        on_progress(&progress);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let Some(fingerprint) = rewrite_fingerprint_for_save(
                    &ws,
                    &plan.info.target_arch,
                    None,
                    &fingerprint_inputs,
                    &mut dep_fingerprints,
                    files.fingerprint.clone(),
                )
                .await?
                else {
                    progress.total_units -= 1;
                    on_progress(&progress);
                    continue;
                };

                // Prepare CAS objects.
                let mut cas_uploads = Vec::new();

                let mut out_dir_files = Vec::new();
                for out_dir_file in files.out_dir_files {
                    let object_key = Key::from_buffer(&out_dir_file.contents);
                    out_dir_files.push(
                        courier::SavedFile::builder()
                            .object_key(object_key.clone())
                            .executable(out_dir_file.executable)
                            .path(serde_json::to_string(&out_dir_file.path)?)
                            .build(),
                    );

                    if !skip.files.contains(&object_key) {
                        progress.uploaded_files += 1;
                        progress.uploaded_bytes += out_dir_file.contents.len() as u64;
                        cas_uploads.push((object_key, out_dir_file.contents));
                    }
                }

                let stdout_contents = serde_json::to_vec(&files.stdout)?;
                let stdout = Key::from_buffer(&stdout_contents);
                if !skip.files.contains(&stdout) {
                    progress.uploaded_files += 1;
                    progress.uploaded_bytes += stdout_contents.len() as u64;
                    cas_uploads.push((stdout.clone(), stdout_contents));
                }

                let stderr = Key::from_buffer(&files.stderr);
                if !skip.files.contains(&stderr) {
                    progress.uploaded_files += 1;
                    progress.uploaded_bytes += files.stderr.len() as u64;
                    cas_uploads.push((stderr.clone(), files.stderr));
                }

                // Save CAS objects.
                if !cas_uploads.is_empty() {
                    cas.store_bulk(stream::iter(cas_uploads)).await?;
                }

                // Prepare save request.
                let save_request = CargoSaveUnitRequest::builder()
                    .unit(courier::SavedUnit::BuildScriptExecution(
                        courier::BuildScriptOutputFiles::builder()
                            .out_dir_files(out_dir_files)
                            .stdout(stdout)
                            .stderr(stderr)
                            .fingerprint(fingerprint)
                            .build(),
                        plan.try_into()?,
                    ))
                    .resolved_target(unit_arch.as_str().to_string())
                    .maybe_linux_glibc_version(glibc_version)
                    .build();

                save_requests.push(save_request);
            }
        }
        progress.uploaded_units += 1;
        on_progress(&progress);
    }

    // Save units to remote cache.
    courier
        .cargo_cache_save(CargoSaveRequest::new(save_requests))
        .await?;

    Result::<_>::Ok(())
}

async fn has_fingerprint_files(ws: &Workspace, unit: &UnitPlan) -> Result<bool> {
    let profile_dir = ws.unit_profile_dir(unit.info());
    Ok(
        fs::exists(&profile_dir.join(&unit.fingerprint_json_file()?)).await
            && fs::exists(&profile_dir.join(&unit.fingerprint_hash_file()?)).await,
    )
}

async fn rewrite_fingerprint_for_save(
    ws: &Workspace,
    target: &RustcTarget,
    src_path: Option<AbsFilePath>,
    fingerprint_inputs: &HashMap<u64, FingerprintRewriteInput>,
    dep_fingerprints: &mut HashMap<u64, Fingerprint>,
    fingerprint: Fingerprint,
) -> Result<Option<courier::Fingerprint>> {
    match rewrite_fingerprint(
        ws,
        target,
        src_path,
        fingerprint_inputs,
        dep_fingerprints,
        fingerprint,
    )
    .await
    {
        Ok(fingerprint) => Ok(Some(fingerprint)),
        Err(error) if is_missing_dependency_fingerprint(&error) => {
            debug!(
                ?error,
                "skipping unit backup: dependency fingerprint is missing"
            );
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn is_missing_dependency_fingerprint(error: &Report) -> bool {
    format!("{error:?}").contains("dependency fingerprint hash not found")
}

fn is_missing_unit_file(error: &Report) -> bool {
    format!("{error:?}").contains("No such file or directory")
}

/// Rewrite fingerprint `src_path`s to be rooted at a static `$CARGO_HOME`.
///
/// This is necessary so that units compiled on host machines with different
/// `$CARGO_HOME`s still have the same `src_path` and therefore still have the
/// same fingerprint hash, so that our rewrite/restore algorithm (that depends
/// on being able to know old fingerprint hashes) works properly.
#[instrument(skip_all)]
async fn rewrite_fingerprint(
    ws: &Workspace,
    target: &RustcTarget,
    src_path: Option<AbsFilePath>,
    fingerprint_inputs: &HashMap<u64, FingerprintRewriteInput>,
    dep_fingerprints: &mut HashMap<u64, Fingerprint>,
    fingerprint: Fingerprint,
) -> Result<courier::Fingerprint> {
    let mut visiting = HashSet::new();
    let rewritten_fingerprint = rewrite_fingerprint_inner(
        ws,
        target,
        src_path,
        fingerprint_inputs,
        dep_fingerprints,
        &mut visiting,
        fingerprint,
    )?;
    serde_json::to_string(&rewritten_fingerprint)?
        .conv::<courier::Fingerprint>()
        .pipe(Ok)
}

fn rewrite_fingerprint_inner(
    ws: &Workspace,
    target: &RustcTarget,
    src_path: Option<AbsFilePath>,
    fingerprint_inputs: &HashMap<u64, FingerprintRewriteInput>,
    dep_fingerprints: &mut HashMap<u64, Fingerprint>,
    visiting: &mut HashSet<u64>,
    fingerprint: Fingerprint,
) -> Result<Fingerprint> {
    let old_hash = fingerprint.hash_u64();
    if let Some(rewritten) = dep_fingerprints.get(&old_hash) {
        return Ok(rewritten.clone());
    }
    if !visiting.insert(old_hash) {
        bail!("cycle in fingerprint dependencies");
    }

    for dep in &fingerprint.deps {
        let dep_hash = dep.fingerprint.hash_u64();
        if dep_fingerprints.contains_key(&dep_hash) {
            continue;
        }
        let input = fingerprint_inputs
            .get(&dep_hash)
            .cloned()
            .ok_or_eyre("dependency fingerprint hash not found")?;
        rewrite_fingerprint_inner(
            ws,
            &input.target,
            input.src_path,
            fingerprint_inputs,
            dep_fingerprints,
            visiting,
            input.fingerprint,
        )?;
    }

    let src_path = rewrite_src_path(ws, target, src_path)?;
    let rewritten = fingerprint.rewrite(src_path, dep_fingerprints)?;
    visiting.remove(&old_hash);
    Ok(rewritten)
}

fn rewrite_src_path(
    ws: &Workspace,
    target: &RustcTarget,
    src_path: Option<AbsFilePath>,
) -> Result<Option<PathBuf>> {
    let src_path = match src_path {
        Some(ref src_path) => {
            let qualified = QualifiedPath::parse_abs(ws, target, src_path);
            match qualified {
                QualifiedPath::Rootless(p) => {
                    bail!("impossible: fingerprint path is not absolute: {}", p)
                }
                QualifiedPath::RelativeTargetProfile(p) => {
                    bail!("unexpected fingerprint path root: {}", p)
                }
                QualifiedPath::Absolute(p) => bail!("unexpected fingerprint path root: {}", p),
                QualifiedPath::RelativeCargoHome(p) => AbsDirPath::try_from("/cargo_home")?
                    .join(p)
                    .conv::<PathBuf>()
                    .pipe(Some),
            }
        }
        None => None,
    };
    Ok(src_path)
}
