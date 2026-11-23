use std::os::unix::fs::symlink;

use super::{Journal, JournalError, MAX_FILE_BYTES, MAX_RECORD_BYTES, RecordId, Role, TestKeySource, Zeroizing, fs};

fn journal() -> (tempfile::TempDir, Journal, RecordId) {
    let dir = tempfile::tempdir().unwrap();
    let mut key = Zeroizing::new([0; 32]);
    getrandom::fill(&mut *key).unwrap();
    let mut journal = Journal::new(dir.path().join("journal"));
    journal.test_key = Some(TestKeySource::Memory(key));
    (dir, journal, RecordId { room: [5; 32], role: Role::Host })
}

#[test]
fn records_are_private_authenticated_replaceable_and_bound_to_scope() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, journal, host) = journal();
    let guest = RecordId { role: Role::Guest, ..host };
    let other_room = RecordId { room: [7; 32], ..host };
    let payload = b"Private document notes survive application restart.";
    assert_eq!(journal.load(host).unwrap(), None);
    journal.store(host, payload).unwrap();
    let host_path = journal.root.join(host.filename());
    let guest_path = journal.root.join(guest.filename());
    let before = fs::read(&host_path).unwrap();
    assert!(!before.windows(payload.len()).any(|part| part == payload));
    assert_eq!(journal.root.metadata().unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(host_path.metadata().unwrap().permissions().mode() & 0o777, 0o600);

    let mut reopened = Journal::new(journal.root.clone());
    reopened.test_key = journal.test_key.clone();
    assert_eq!(reopened.load(host).unwrap(), Some(payload.to_vec()));
    assert_eq!(reopened.load(guest).unwrap(), None);
    fs::copy(&host_path, &guest_path).unwrap();
    assert!(matches!(reopened.load(guest), Err(JournalError::Authentication)));
    fs::copy(&host_path, journal.root.join(other_room.filename())).unwrap();
    assert!(matches!(reopened.load(other_room), Err(JournalError::Authentication)));

    reopened.store(host, payload).unwrap();
    let mut changed = fs::read(&host_path).unwrap();
    assert_ne!(before, changed);
    *changed.last_mut().unwrap() ^= 1;
    fs::write(&host_path, changed).unwrap();
    assert!(matches!(reopened.load(host), Err(JournalError::Authentication)));
    reopened.delete(host).unwrap();
    reopened.delete(host).unwrap();
    assert_eq!(reopened.load(host).unwrap(), None);
}

#[test]
fn record_size_and_symlinks_are_rejected() {
    let (_dir, journal, id) = journal();
    assert!(matches!(journal.store(id, &vec![0; MAX_RECORD_BYTES + 1]), Err(JournalError::TooLarge)));
    journal.store(id, b"first sealed write").unwrap();
    let path = journal.root.join(id.filename());
    fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(MAX_FILE_BYTES as u64 + 1).unwrap();
    assert!(matches!(journal.load(id), Err(JournalError::TooLarge)));
    let other = journal.root.join("unrelated");
    fs::rename(&path, &other).unwrap();
    symlink(other, &path).unwrap();
    assert!(journal.load(id).is_err());
    assert!(journal.store(id, b"do not write through the symlink").is_err());
}

#[test]
fn lifetime_lease_excludes_another_writer_without_lockfile_removal() {
    let (_dir, journal, host) = journal();
    let second_window = Journal::new(journal.root.clone());
    let first = journal.lock(host).unwrap();
    assert!(matches!(second_window.lock(host), Err(JournalError::Busy)));
    assert!(second_window.lock(RecordId { role: Role::Guest, ..host }).is_ok());
    journal.delete(host).unwrap();
    assert!(matches!(second_window.lock(host), Err(JournalError::Busy)));
    drop(first);
    assert!(second_window.lock(host).is_ok());
}

struct DisposableKeychain(std::path::PathBuf);

impl Drop for DisposableKeychain {
    fn drop(&mut self) {
        if !std::process::Command::new("/usr/bin/security").arg("delete-keychain").arg(&self.0).output().is_ok_and(|output| output.status.success()) {
            eprintln!("Could not remove the disposable journal test Keychain at {}", self.0.display());
        }
    }
}

#[test]
#[ignore = "requires a macOS user Keychain session"]
fn native_keychain_reopens_journal_in_another_process() {
    use security_framework::os::macos::keychain::{CreateOptions, SecKeychain};
    use std::{
        fmt::Write as _,
        io::{Read as _, Write as _},
        process::{Command, Stdio},
    };

    const CHILD_ROOT: &str = "CODESIGN_TEST_JOURNAL_CHILD_ROOT";
    const ID: RecordId = RecordId { room: [9; 32], role: Role::Guest };
    const PENDING: &[u8] = b"fixture shared document, confirmed revision 12, one pending proposal";
    const CONFIRMED: &[u8] = b"fixture shared document, confirmed revision 13";

    if let Some(root) = std::env::var_os(CHILD_ROOT).map(std::path::PathBuf::from) {
        let mut password = Zeroizing::new(String::new());
        std::io::stdin().read_to_string(&mut password).unwrap();
        let mut keychain = SecKeychain::open(root.join("recovery.keychain")).unwrap();
        keychain.unlock(Some(&password)).unwrap();
        let mut journal = Journal::new(root.join("journal"));
        journal.test_key = Some(TestKeySource::Keychain(keychain));
        let _lease = journal.lock(ID).unwrap();
        assert_eq!(journal.load(ID).unwrap().as_deref(), Some(PENDING));
        journal.store(ID, CONFIRMED).unwrap();
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let mut entropy = Zeroizing::new([0u8; 16]);
    getrandom::fill(&mut *entropy).unwrap();
    let mut password = Zeroizing::new(String::with_capacity(32));
    for byte in entropy.iter() {
        write!(password, "{byte:02x}").unwrap();
    }
    let path = temp.path().join("recovery.keychain");
    let mut keychain = CreateOptions::new().password(&password).create(&path).unwrap();
    let _cleanup = DisposableKeychain(path);
    keychain.unlock(Some(&password)).unwrap();

    let mut journal = Journal::new(temp.path().join("journal"));
    journal.test_key = Some(TestKeySource::Keychain(keychain.clone()));
    let lease = journal.lock(ID).unwrap();
    assert_eq!(journal.load(ID).unwrap(), None);
    journal.store(ID, PENDING).unwrap();
    drop(lease);
    let record = journal.root.join(ID.filename());
    let encrypted = fs::read(&record).unwrap();
    assert!(!encrypted.windows(PENDING.len()).any(|window| window == PENDING));

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "journal::tests::native_keychain_reopens_journal_in_another_process"])
        .env(CHILD_ROOT, temp.path())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(password.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(journal.load(ID).unwrap().as_deref(), Some(CONFIRMED));

    let (_, key) = keychain.find_generic_password(super::SERVICE, super::ACCOUNT).unwrap();
    key.delete();
    let before = fs::read(&record).unwrap();
    assert!(matches!(journal.load(ID), Err(JournalError::KeyMissing)));
    assert!(matches!(journal.store(ID, b"would replace an unreadable journal"), Err(JournalError::KeyMissing)));
    assert_eq!(fs::read(record).unwrap(), before);
}
