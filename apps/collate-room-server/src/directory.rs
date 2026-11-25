//! Filesystem authority for the directory selected by the room-server operator.
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use craft_atomic::{AtomicFs, NewFilePermissions, RealFs, WriteOptions};
use designcraft_doc::{Document, MAX_SHARED_ARTWORK_BYTES, build::NewDocument};

use crate::room_server::{self, RoomFile, RoomFiles, Server};

fn io_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn read_capped(path: impl AsRef<Path>, limit: u64) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len > limit {
        return Err(io::Error::other("File exceeds its size limit."));
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len as usize).map_err(io::Error::other)?;
    file.take(len + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != len {
        return Err(io::Error::other("File changed during read."));
    }
    Ok(bytes)
}

fn private_files() -> WriteOptions {
    WriteOptions::default().with_new_file_permissions(NewFilePermissions::Private)
}

/// A lock on the room directory serializes every command and survives replacement of individual files.
struct Directory {
    root: PathBuf,
    data: PathBuf,
    _lock: File,
}

impl Directory {
    fn lock(root: &Path) -> Result<Self, String> {
        let metadata = fs::symlink_metadata(root).map_err(io_error)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err("Use a private room directory (permissions 0700).".into());
        }
        let root = fs::canonicalize(root).map_err(io_error)?;
        let lock = File::open(&root).map_err(io_error)?;
        lock.try_lock().map_err(|error| format!("Could not exclusively lock the room directory: {error}"))?;
        Ok(Self { data: root.join("state"), root, _lock: lock })
    }

    fn path(&self, file: RoomFile) -> PathBuf {
        match file {
            RoomFile::Header => self.data.join("room.json"),
            RoomFile::Seed => self.data.join("seed"),
            RoomFile::Blob(digest) => self.data.join("blobs").join(URL_SAFE_NO_PAD.encode(digest)),
            RoomFile::Revision(number) => self.data.join("history").join(format!("{number}.json")),
            RoomFile::Invitation => self.root.join("invitation.txt"),
        }
    }
}

impl RoomFiles for Directory {
    fn read(&self, file: RoomFile, limit: u64) -> Result<Vec<u8>, String> {
        read_capped(self.path(file), limit).map_err(io_error)
    }

    fn write(&self, file: RoomFile, bytes: &[u8]) -> Result<(), String> {
        let path = self.path(file);
        match file {
            RoomFile::Revision(_) | RoomFile::Seed => private_files().write_new(&path, bytes),
            _ => private_files().atomic_write(&path, bytes),
        }
        .map_err(io_error)
    }

    fn revisions(&self, limit: usize) -> Result<Vec<u64>, String> {
        let mut revisions = BTreeSet::new();
        for entry in fs::read_dir(self.data.join("history")).map_err(io_error)? {
            let path = entry.map_err(io_error)?.path();
            // Atomic writes may leave an unpublished staging file after a crash.
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let revision = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok()).ok_or("Invalid journal revision name.")?;
            if path != self.path(RoomFile::Revision(revision)) || !revisions.insert(revision) || revisions.len() > limit {
                return Err("Unexpected room revision name or count.".into());
            }
        }
        Ok(revisions.into_iter().collect())
    }

    fn clear_invitation(&self) -> Result<(), String> {
        match fs::remove_file(self.path(RoomFile::Invitation)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(error)),
        }
    }
}

pub(crate) fn init(root: &Path, original: Option<&Path>) -> Result<(), String> {
    let doc = if let Some(original) = original {
        let input = read_capped(original, MAX_SHARED_ARTWORK_BYTES as u64 + 8 * 1024 * 1024).map_err(io_error)?;
        designcraft_format::load(&input).map_err(io_error)?
    } else {
        Document::new(&NewDocument::default())
    };
    match fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io_error(error)),
    }
    let mut directory = Directory::lock(root)?;
    for entry in fs::read_dir(&directory.root).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        if !entry.file_name().to_string_lossy().starts_with(".collate-init-") || !entry.file_type().map_err(io_error)?.is_dir() {
            return Err("Initialize in an empty directory. Use serve to resume an existing room.".into());
        }
    }
    let stage = tempfile::Builder::new().prefix(".collate-init-").tempdir_in(&directory.root).map_err(io_error)?;
    let state = std::mem::replace(&mut directory.data, stage.path().to_owned());
    fs::DirBuilder::new().mode(0o700).create(directory.data.join("history")).map_err(io_error)?;
    fs::DirBuilder::new().mode(0o700).create(directory.data.join("blobs")).map_err(io_error)?;
    let directory = Arc::new(directory);
    room_server::init(directory.clone(), doc)?;
    fs::rename(stage.path(), state).map_err(io_error)?;
    RealFs.sync_dir(&directory.root).map_err(io_error)
}

pub(crate) fn open(root: &Path, relay: bool, port: u16) -> Result<Server, String> {
    Server::open(Arc::new(Directory::lock(root)?), relay, port)
}

pub(crate) fn rotate_invite(root: &Path) -> Result<(), String> {
    room_server::rotate_invite(Arc::new(Directory::lock(root)?))
}

pub(crate) fn export(root: &Path, output: &Path) -> Result<(), String> {
    let directory = Arc::new(Directory::lock(root)?);
    let doc = room_server::snapshot(directory.clone())?;
    private_files().write_new_with(output, |file| designcraft_format::write(file, &doc, None).map_err(io::Error::other)).map_err(io_error)
}
