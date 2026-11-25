//! Optional original artwork storage in a locally configured folder.
//!
//! The room supplies only a content identity. Local setup supplies the root, and
//! the checked-in layout supplies every path component beneath it. Run these
//! operations on an artwork worker because files may be hydrated by a sync client.

use std::fmt::Write as _;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use designcraft_collab::artwork::BlobId;
use designcraft_engine::io as engine_io;
use sha2::{Digest, Sha256};

const DIRECTORY: &str = ".collate-artwork-v1";

pub(super) struct SharedArtworkFolder {
    root: PathBuf,
}

impl SharedArtworkFolder {
    /// An explicitly chosen, existing local directory. Configuring it does not write files.
    pub(super) fn open(root: &Path) -> io::Result<Self> {
        if !root.is_absolute() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Choose an absolute shared artwork folder."));
        }
        let root = engine_io::canonicalize(root)?;
        if !engine_io::Metadata::is_dir(&engine_io::metadata(&root)?) {
            return Err(io::Error::from(io::ErrorKind::NotADirectory));
        }
        Ok(Self { root })
    }

    /// A missing file, a sync placeholder, or different bytes do not supply the original.
    pub(super) fn load(&self, id: BlobId) -> io::Result<Option<Arc<Vec<u8>>>> {
        if !id.valid() {
            return Ok(None);
        }
        let Some(directory) = self.directory(false)? else { return Ok(None) };
        let Some(file) = open_blob(&directory.join(blob_name(id)), id)? else { return Ok(None) };
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(id.length as usize).map_err(io::Error::other)?;
        file.take(u64::from(id.length) + 1).read_to_end(&mut bytes)?;
        Ok(matches_blob(id, &bytes).then(|| Arc::new(bytes)))
    }

    /// Store the exact original. A concurrent successful identical publish is also success.
    pub(super) fn publish(&self, id: BlobId, bytes: &[u8]) -> io::Result<()> {
        if !id.valid() || !matches_blob(id, bytes) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Artwork does not match its expected content."));
        }
        let directory = self.directory(true)?.ok_or(io::ErrorKind::NotFound)?;
        let target = directory.join(blob_name(id));
        match engine_io::write_new_private(&target, bytes) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => check_existing(&target, id),
            result => result,
        }
    }

    fn directory(&self, create: bool) -> io::Result<Option<PathBuf>> {
        let directory = self.root.join(DIRECTORY);
        if create {
            if engine_io::canonicalize(&self.root)? != self.root {
                return Err(permission_error("The configured artwork folder has moved."));
            }
            match engine_io::create_private_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }

        match engine_io::symlink_metadata(&directory) {
            Ok(meta) if engine_io::Metadata::is_dir(&meta) && engine_io::canonicalize(&directory)? == directory => Ok(Some(directory)),
            Ok(_) => Err(permission_error("The Collate artwork folder must be a regular local directory.")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn blob_name(id: BlobId) -> String {
    let mut name = String::with_capacity(id.digest.len() * 2);
    for byte in id.digest {
        let _ = write!(name, "{byte:02x}");
    }
    name
}

fn permission_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}

fn matches_blob(id: BlobId, bytes: &[u8]) -> bool {
    bytes.len() == id.length as usize && <[u8; 32]>::from(Sha256::digest(bytes)) == id.digest
}

fn open_blob(path: &Path, id: BlobId) -> io::Result<Option<engine_io::File>> {
    match engine_io::open_canonical_file_with_length(path, u64::from(id.length)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        result => result,
    }
}

fn check_existing(path: &Path, id: BlobId) -> io::Result<()> {
    if let Some(file) = open_blob(path, id)? {
        let mut file = file.take(u64::from(id.length) + 1);
        let mut digest = Sha256::new();
        let mut size = 0;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            size += read as u64;
            digest.update(&buffer[..read]);
        }
        if size == u64::from(id.length) && <[u8; 32]>::from(digest.finalize()) == id.digest {
            return Ok(());
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "A different file occupies this shared artwork location."))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Barrier;

    use super::*;

    fn id(bytes: &[u8]) -> BlobId {
        BlobId { digest: Sha256::digest(bytes).into(), length: bytes.len().try_into().unwrap() }
    }

    #[test]
    fn opening_a_shared_folder_does_not_write_and_a_publish_is_content_addressed() {
        let root = tempfile::tempdir().unwrap();
        let folder = SharedArtworkFolder::open(root.path()).unwrap();
        let bytes = b"original image bytes";
        let blob = id(bytes);
        assert!(folder.load(blob).unwrap().is_none());
        assert!(!root.path().join(DIRECTORY).exists());
        folder.publish(blob, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let directory = root.path().join(DIRECTORY);
            assert_eq!(fs::metadata(&directory).unwrap().permissions().mode() & 0o077, 0);
            assert_eq!(fs::metadata(directory.join(blob_name(blob))).unwrap().permissions().mode() & 0o077, 0);
        }
        let another = SharedArtworkFolder::open(root.path()).unwrap();
        assert_eq!(another.load(blob).unwrap().unwrap().as_slice(), bytes);
        assert!(another.load(id(b"different image")).unwrap().is_none());
    }

    #[test]
    fn invalid_inputs_and_mismatched_files_are_never_published_or_returned() {
        let root = tempfile::tempdir().unwrap();
        let folder = SharedArtworkFolder::open(root.path()).unwrap();
        let bytes = b"original image bytes";
        let blob = id(bytes);
        assert_eq!(folder.publish(blob, b"invalid bytes").unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(!root.path().join(DIRECTORY).exists());
        folder.publish(blob, bytes).unwrap();

        let path = root.path().join(DIRECTORY).join(blob_name(blob));
        let corrupt = vec![b'X'; bytes.len()];
        fs::write(&path, &corrupt).unwrap();
        assert!(folder.load(blob).unwrap().is_none());
        assert_eq!(folder.publish(blob, bytes).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), corrupt);

        fs::write(&path, b"truncated").unwrap();
        assert!(folder.load(blob).unwrap().is_none());
    }

    #[test]
    fn concurrent_publishers_converge_on_one_complete_original() {
        let root = tempfile::tempdir().unwrap();
        let folder = SharedArtworkFolder::open(root.path()).unwrap();
        let bytes = vec![b'P'; 120_000];
        let blob = id(&bytes);
        let start = Barrier::new(2);
        std::thread::scope(|scope| {
            let publish = || {
                start.wait();
                folder.publish(blob, &bytes).unwrap();
            };
            scope.spawn(publish);
            scope.spawn(publish);
        });
        assert_eq!(folder.load(blob).unwrap().unwrap().as_slice(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_redirect_reads_or_writes_outside_the_folder() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let folder = SharedArtworkFolder::open(root.path()).unwrap();
        let bytes = b"original image bytes";
        let blob = id(bytes);
        let external = outside.path().join(blob_name(blob));
        fs::write(&external, bytes).unwrap();
        let directory = root.path().join(DIRECTORY);
        symlink(outside.path(), &directory).unwrap();
        assert!(folder.load(blob).is_err());
        assert!(folder.publish(blob, bytes).is_err());

        fs::remove_file(&directory).unwrap();
        fs::create_dir(&directory).unwrap();
        symlink(&external, directory.join(blob_name(blob))).unwrap();
        assert!(folder.load(blob).unwrap().is_none());
        assert!(folder.publish(blob, bytes).is_err());
        assert_eq!(fs::read(&external).unwrap(), bytes);
    }
}
