#![cfg(unix)]

use loop_wire::token::{create, read};
use std::os::unix::fs::{symlink, PermissionsExt};

#[test]
fn tokens_are_private_fresh_and_readable_only_from_safe_files() {
    let dir = tempfile::tempdir().unwrap();
    let (token, path) = create(dir.path()).unwrap();
    assert!(token.len() >= 43);
    assert_eq!(read(&path).unwrap(), token);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_ne!(create(dir.path()).unwrap().0, token);

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read(&path).is_err());
    assert!(create(dir.path()).is_err());
    std::fs::remove_file(&path).unwrap();
    let target = dir.path().join("keep");
    std::fs::write(&target, "keep me").unwrap();
    symlink(&target, &path).unwrap();
    assert!(read(&path).is_err());
    assert!(create(dir.path()).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");
}

#[test]
fn token_directory_symlinks_and_shared_files_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    symlink(other.path(), dir.path().join("run")).unwrap();
    assert!(create(dir.path()).is_err());
    std::fs::remove_file(dir.path().join("run")).unwrap();
    let (_, path) = create(dir.path()).unwrap();
    std::fs::hard_link(&path, dir.path().join("alias")).unwrap();
    assert!(read(&path).is_err());
    assert!(create(dir.path()).is_err());
}

#[test]
fn reading_a_token_requires_a_private_parent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let (_, path) = create(dir.path()).unwrap();
    std::fs::set_permissions(
        path.parent().unwrap(),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(read(&path).is_err());
}
