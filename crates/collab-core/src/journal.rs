//! Bounded, authenticated local collaboration records. Document schemas belong to callers.

use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use security_framework::{base::Error as KeychainError, os::macos::keychain::SecKeychain};
use security_framework_sys::base::{errSecDuplicateItem, errSecItemNotFound};
use std::{
    fmt::Write as _,
    fs::{self, DirBuilder, File, OpenOptions, Permissions, TryLockError},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use thiserror::Error;
use zeroize::Zeroizing;

// A Base64 seed, 8 MiB confirmed history and one pending artwork transfer.
pub const MAX_RECORD_BYTES: usize = 20 * 1024 * 1024;
const SERVICE: &str = "com.larshoie.codesign.collaboration";
const ACCOUNT: &str = "journal-wrapping-key-v1";
const MAGIC: &[u8; 8] = b"CDJRNL01";
const NONCE_BYTES: usize = 24;
const TAG_BYTES: usize = 16;
const HEADER_BYTES: usize = MAGIC.len() + NONCE_BYTES;
const MAX_FILE_BYTES: usize = HEADER_BYTES + MAX_RECORD_BYTES + TAG_BYTES;
type Key = Zeroizing<[u8; 32]>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Host,
    Guest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordId {
    pub room: [u8; 32],
    pub role: Role,
}

impl RecordId {
    fn associated_data(self) -> [u8; 41] {
        let mut data = [0; 41];
        data[..MAGIC.len()].copy_from_slice(MAGIC);
        data[MAGIC.len()] = match self.role {
            Role::Host => b'H',
            Role::Guest => b'G',
        };
        data[MAGIC.len() + 1..].copy_from_slice(&self.room);
        data
    }

    fn filename(self) -> String {
        let prefix = match self.role {
            Role::Host => "host-",
            Role::Guest => "guest-",
        };
        let mut name = String::with_capacity(prefix.len() + self.room.len() * 2 + 4);
        name.push_str(prefix);
        for byte in self.room {
            let _ = write!(name, "{byte:02x}");
        }
        name.push_str(".enc");
        name
    }
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("Could not read or write the local collaboration journal: {0}")]
    Io(#[from] io::Error),
    #[error("The local collaboration journal must be in an owner-only directory and regular file")]
    Permissions,
    #[error("Another window already has this local collaboration journal open")]
    Busy,
    #[error("The local collaboration journal exceeds its size limit")]
    TooLarge,
    #[error("The local collaboration journal uses an invalid or unsupported format")]
    InvalidFormat,
    #[error("The local collaboration journal failed authentication")]
    Authentication,
    #[error("Could not generate cryptographically secure random bytes for the local collaboration journal")]
    Random,
    #[error("Could not access the collaboration journal key in Keychain: {0}")]
    Keychain(#[source] KeychainError),
    #[error("The wrapping key for the existing collaboration journal is missing from Keychain")]
    KeyMissing,
    #[error("The Keychain entry has an invalid collaboration journal key")]
    KeyInvalid,
}

/// Ciphertext storage in a dedicated application-owned directory. Access may prompt via Keychain.
pub struct Journal {
    root: PathBuf,
    #[cfg(any(test, feature = "test-journal"))]
    test_key: Option<TestKeySource>,
}

#[cfg(any(test, feature = "test-journal"))]
#[derive(Clone)]
enum TestKeySource {
    #[cfg(test)]
    Memory(Key),
    Keychain(SecKeychain),
}

#[cfg(feature = "test-journal")]
thread_local! {
    static TEST_KEYCHAIN: std::cell::RefCell<Option<SecKeychain>> = const { std::cell::RefCell::new(None) };
}

/// Use the supplied disposable Keychain for journals created on this thread during an integration test.
#[cfg(feature = "test-journal")]
#[doc(hidden)]
pub fn with_test_keychain<R>(keychain: SecKeychain, run: impl FnOnce() -> R) -> R {
    struct RestoreKeychain(Option<SecKeychain>);

    impl Drop for RestoreKeychain {
        fn drop(&mut self) {
            TEST_KEYCHAIN.set(self.0.take());
        }
    }

    let _restore = RestoreKeychain(TEST_KEYCHAIN.replace(Some(keychain)));
    run()
}

/// Keep this lease alive while the application owns the selected journal record.
pub struct RecordLock {
    _file: File,
}

impl Journal {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            #[cfg(feature = "test-journal")]
            test_key: TEST_KEYCHAIN.with_borrow(|keychain| keychain.clone().map(TestKeySource::Keychain)),
            #[cfg(all(test, not(feature = "test-journal")))]
            test_key: None,
        }
    }

    pub fn load(&self, id: RecordId) -> Result<Option<Vec<u8>>, JournalError> {
        if !private_directory(&self.root)? {
            return Ok(None);
        }
        let Some(file) = private_file(&self.root.join(id.filename()))? else {
            return Ok(None);
        };
        let length = file.metadata()?.len();
        if length > MAX_FILE_BYTES as u64 {
            return Err(JournalError::TooLarge);
        }
        let mut bytes = Vec::with_capacity(length as usize);
        file.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(JournalError::TooLarge);
        }
        let key = self.key(false)?;
        open(&key, id, &bytes).map(Some)
    }

    /// Acquire the exclusive writer lease for one record. The lock file stays in the directory.
    pub fn lock(&self, id: RecordId) -> Result<RecordLock, JournalError> {
        self.ensure_directory()?;
        let path = self.root.join(format!("{}.lock", id.filename()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        validate_file(&file)?;
        match file.try_lock() {
            Ok(()) => Ok(RecordLock { _file: file }),
            Err(TryLockError::WouldBlock) => Err(JournalError::Busy),
            Err(error) => Err(io::Error::from(error).into()),
        }
    }

    /// Durably replace the caller's record. A missing wrapping key preserves existing data.
    pub fn store(&self, id: RecordId, payload: &[u8]) -> Result<(), JournalError> {
        if payload.len() > MAX_RECORD_BYTES {
            return Err(JournalError::TooLarge);
        }
        self.ensure_directory()?;
        let target = self.root.join(id.filename());
        let allow_create = private_file(&target)?.is_none();
        let key = self.key(allow_create)?;
        let encrypted = seal(&key, id, payload)?;
        let mut stage = tempfile::Builder::new().permissions(Permissions::from_mode(0o600)).tempfile_in(&self.root)?;
        stage.write_all(&encrypted)?;
        stage.as_file().sync_all()?;
        stage.persist(target).map_err(|error| error.error)?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    pub fn delete(&self, id: RecordId) -> Result<(), JournalError> {
        if !private_directory(&self.root)? {
            return Ok(());
        }
        match fs::remove_file(self.root.join(id.filename())) {
            Ok(()) => Ok(File::open(&self.root)?.sync_all()?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn ensure_directory(&self) -> Result<(), JournalError> {
        DirBuilder::new().recursive(true).mode(0o700).create(&self.root)?;
        private_directory(&self.root)?.then_some(()).ok_or(JournalError::Permissions)
    }

    fn key(&self, allow_create: bool) -> Result<Key, JournalError> {
        #[cfg(any(test, feature = "test-journal"))]
        if let Some(source) = &self.test_key {
            return match source {
                #[cfg(test)]
                TestKeySource::Memory(key) => Ok(key.clone()),
                TestKeySource::Keychain(keychain) => key_in(keychain, allow_create),
            };
        }
        let keychain = SecKeychain::default().map_err(JournalError::Keychain)?;
        key_in(&keychain, allow_create)
    }
}

fn private_directory(path: &Path) -> Result<bool, JournalError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0 => Ok(true),
        Ok(_) => Err(JournalError::Permissions),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn private_file(path: &Path) -> Result<Option<File>, JournalError> {
    let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    validate_file(&file)?;
    Ok(Some(file))
}

fn validate_file(file: &File) -> Result<(), JournalError> {
    let metadata = file.metadata()?;
    if metadata.is_file() && metadata.permissions().mode() & 0o077 == 0 { Ok(()) } else { Err(JournalError::Permissions) }
}

fn key_in(keychain: &SecKeychain, allow_create: bool) -> Result<Key, JournalError> {
    match read_key(keychain) {
        Err(JournalError::KeyMissing) if allow_create => create_key(keychain),
        result => result,
    }
}

fn read_key(keychain: &SecKeychain) -> Result<Key, JournalError> {
    match keychain.find_generic_password(SERVICE, ACCOUNT) {
        Ok((secret, _)) => secret.as_ref().try_into().map(Zeroizing::new).map_err(|_| JournalError::KeyInvalid),
        Err(error) if error.code() == errSecItemNotFound => Err(JournalError::KeyMissing),
        Err(error) => Err(JournalError::Keychain(error)),
    }
}

fn create_key(keychain: &SecKeychain) -> Result<Key, JournalError> {
    let mut key = Zeroizing::new([0; 32]);
    getrandom::fill(&mut *key).map_err(|_| JournalError::Random)?;
    match keychain.add_generic_password(SERVICE, ACCOUNT, &*key) {
        Ok(()) => Ok(key),
        Err(error) if error.code() == errSecDuplicateItem => read_key(keychain),
        Err(error) => Err(JournalError::Keychain(error)),
    }
}

fn seal(key: &[u8; 32], id: RecordId, plaintext: &[u8]) -> Result<Vec<u8>, JournalError> {
    let mut nonce = [0; NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(|_| JournalError::Random)?;
    let ciphertext = XChaCha20Poly1305::new(key.into())
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plaintext, aad: &id.associated_data() })
        .map_err(|_| JournalError::Authentication)?;
    let mut record = Vec::with_capacity(HEADER_BYTES + ciphertext.len());
    record.extend_from_slice(MAGIC);
    record.extend_from_slice(&nonce);
    record.extend_from_slice(&ciphertext);
    Ok(record)
}

fn open(key: &[u8; 32], id: RecordId, record: &[u8]) -> Result<Vec<u8>, JournalError> {
    if record.len() < HEADER_BYTES + TAG_BYTES || !record.starts_with(MAGIC) {
        return Err(JournalError::InvalidFormat);
    }
    let nonce = XNonce::from_slice(&record[MAGIC.len()..HEADER_BYTES]);
    XChaCha20Poly1305::new(key.into())
        .decrypt(nonce, Payload { msg: &record[HEADER_BYTES..], aad: &id.associated_data() })
        .map_err(|_| JournalError::Authentication)
}

#[cfg(test)]
mod tests;
