mod etc_tree;

use anyhow::{anyhow, Result};
use im::HashMap;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs::{DirBuilder, Permissions};
use std::os::unix::prelude::PermissionsExt;
use std::path;
use std::path::{Path, PathBuf};
use std::{fs, io};

use crate::{
    create_link, create_store_link, remove_dir, remove_file, remove_link, StorePath,
    ETC_STATE_FILE_NAME, SYSTEM_MANAGER_STATE_DIR,
};
use etc_tree::EtcTree;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EtcFile {
    source: StorePath,
    target: PathBuf,
    uid: u32,
    gid: u32,
    group: String,
    user: String,
    mode: String,
}

type EtcFiles = HashMap<String, EtcFile>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EtcFilesConfig {
    entries: EtcFiles,
    static_env: StorePath,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreatedEtcFile {
    path: PathBuf,
}

pub fn activate(store_path: &StorePath, ephemeral: bool) -> Result<()> {
    log::info!("Reading etc file definitions...");
    let file = fs::File::open(Path::new(&store_path.store_path).join("etcFiles/etcFiles.json"))?;
    let reader = io::BufReader::new(file);
    let config: EtcFilesConfig = serde_json::from_reader(reader)?;
    log::debug!("{:?}", config);

    let etc_dir = etc_dir(ephemeral);
    log::debug!("Storing /etc entries in {}", etc_dir.display());

    DirBuilder::new().recursive(true).create(&etc_dir)?;

    let old_state = read_created_files()?;

    // TODO: constant?
    let static_dir_name = ".system-manager-static";
    let (state, status) = create_etc_static_link(
        static_dir_name,
        &config.static_env,
        &etc_dir,
        EtcTree::new(PathBuf::new()),
    );
    status?;
    let new_state = create_etc_links(config.entries.values(), &etc_dir, state).update_state(
        old_state,
        &|path| {
            log::debug!("Deactivating: {}", path.display());
            false
        },
    );

    serialise_state(&new_state.unwrap_or_else(|| EtcTree::new(PathBuf::from("/"))))?;

    log::info!("Done");
    Ok(())
}

pub fn deactivate() -> Result<()> {
    let state = read_created_files()?;
    log::debug!("{:?}", state);

    serialise_state(&state.deactivate_managed_entry(
        Path::new("/"),
        &|path| match try_delete_path(path) {
            Ok(()) => true,
            Err(e) => {
                log::error!("Error deleting path: {}", path.display());
                log::error!("{e}");
                false
            }
        },
    ))?;

    log::info!("Done");
    Ok(())
}

fn try_delete_path(path: &Path) -> Result<()> {
    // exists() returns false for broken symlinks
    if path.exists() || path.is_symlink() {
        if path.is_symlink() {
            remove_link(path)
        } else if path.is_file() {
            remove_file(path)
        } else if path.is_dir() {
            remove_dir(path)
        } else {
            Err(anyhow!("Unsupported file type! {}", path.display()))
        }
    } else {
        Ok(())
    }
}

fn create_etc_links<'a, E>(entries: E, etc_dir: &Path, state: EtcTree) -> EtcTree
where
    E: Iterator<Item = &'a EtcFile>,
{
    entries.fold(state, |state, entry| {
        let (new_state, status) = create_etc_entry(entry, etc_dir, state);
        match status {
            Ok(_) => new_state,
            Err(e) => {
                log::error!("Error while creating file in {}: {e}", etc_dir.display());
                new_state
            }
        }
    })
}

fn create_etc_static_link(
    static_dir_name: &str,
    store_path: &StorePath,
    etc_dir: &Path,
    state: EtcTree,
) -> (EtcTree, Result<()>) {
    let static_path = etc_dir.join(static_dir_name);
    let (new_state, status) = create_dir_recursively(static_path.parent().unwrap(), state);
    match status.and_then(|_| create_store_link(store_path, static_path.as_path())) {
        Ok(_) => (new_state.register_managed_entry(&static_path), Ok(())),
        e => (new_state, e),
    }
}

fn create_etc_link(link_target: &OsStr, etc_dir: &Path, state: EtcTree) -> (EtcTree, Result<()>) {
    let link_path = etc_dir.join(link_target);
    let (new_state, status) = create_dir_recursively(link_path.parent().unwrap(), state);
    match status.and_then(|_| {
        create_link(
            Path::new(".")
                .join(".system-manager-static")
                .join("etc")
                .join(link_target)
                .as_path(),
            link_path.as_path(),
        )
    }) {
        Ok(_) => (new_state.register_managed_entry(&link_path), Ok(())),
        e => (new_state, e),
    }
}

// TODO split up this function, and treat symlinks and copied files the same in the state file (ie
// include the root for both).
fn create_etc_entry(entry: &EtcFile, etc_dir: &Path, state: EtcTree) -> (EtcTree, Result<()>) {
    if entry.mode == "symlink" {
        if let Some(path::Component::Normal(link_target)) = entry.target.components().next() {
            create_etc_link(link_target, etc_dir, state)
        } else {
            (
                state,
                Err(anyhow!("Cannot create link: {}", entry.target.display(),)),
            )
        }
    } else {
        let target_path = etc_dir.join(entry.target.as_path());
        let (new_state, status) = create_dir_recursively(target_path.parent().unwrap(), state);
        match status.and_then(|_| {
            copy_file(
                entry
                    .source
                    .store_path
                    .join("etc")
                    .join(&entry.target)
                    .as_path(),
                &target_path,
                &entry.mode,
            )
        }) {
            Ok(_) => (new_state.register_managed_entry(&target_path), Ok(())),
            e => (new_state, e),
        }
    }
}

fn create_dir_recursively(dir: &Path, state: EtcTree) -> (EtcTree, Result<()>) {
    use itertools::FoldWhile::{Continue, Done};
    use path::Component;

    let dirbuilder = DirBuilder::new();
    let (new_state, _, status) = dir
        .components()
        .fold_while(
            (state, PathBuf::from("/"), Ok(())),
            |(state, path, _), component| match component {
                Component::RootDir => Continue((state, path, Ok(()))),
                Component::Normal(dir) => {
                    let new_path = path.join(dir);
                    if !new_path.exists() {
                        log::debug!("Creating path: {}", new_path.display());
                        match dirbuilder.create(new_path.as_path()) {
                            Ok(_) => {
                                let new_state = state.register_managed_entry(&new_path);
                                Continue((new_state, new_path, Ok(())))
                            }
                            Err(e) => Done((state, path, Err(anyhow!(e)))),
                        }
                    } else {
                        Continue((state, new_path, Ok(())))
                    }
                }
                otherwise => Done((
                    state,
                    path,
                    Err(anyhow!(
                        "Unexpected path component encountered: {:?}",
                        otherwise
                    )),
                )),
            },
        )
        .into_inner();
    (new_state, status)
}

fn copy_file(source: &Path, target: &Path, mode: &str) -> Result<()> {
    fs::copy(source, target)?;
    let mode_int = u32::from_str_radix(mode, 8).map_err(anyhow::Error::from)?;
    fs::set_permissions(target, Permissions::from_mode(mode_int))?;
    Ok(())
}

fn etc_dir(ephemeral: bool) -> PathBuf {
    if ephemeral {
        Path::new("/run").join("etc")
    } else {
        Path::new("/etc").to_path_buf()
    }
}

fn serialise_state(created_files: &EtcTree) -> Result<()> {
    let state_file = Path::new(SYSTEM_MANAGER_STATE_DIR).join(ETC_STATE_FILE_NAME);
    DirBuilder::new()
        .recursive(true)
        .create(SYSTEM_MANAGER_STATE_DIR)?;

    log::info!("Writing state info into file: {}", state_file.display());
    let writer = io::BufWriter::new(fs::File::create(state_file)?);
    serde_json::to_writer(writer, created_files)?;
    Ok(())
}

fn read_created_files() -> Result<EtcTree> {
    let state_file = Path::new(SYSTEM_MANAGER_STATE_DIR).join(ETC_STATE_FILE_NAME);
    DirBuilder::new()
        .recursive(true)
        .create(SYSTEM_MANAGER_STATE_DIR)?;

    if Path::new(&state_file).is_file() {
        log::info!("Reading state info from {}", state_file.display());
        let reader = io::BufReader::new(fs::File::open(state_file)?);
        match serde_json::from_reader(reader) {
            Ok(created_files) => return Ok(created_files),
            Err(e) => {
                log::error!("Error reading the state file, ignoring.");
                log::error!("{:?}", e);
            }
        }
    }
    Ok(EtcTree::new(PathBuf::from("/")))
}
