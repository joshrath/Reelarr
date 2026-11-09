use base64::{Engine, engine::general_purpose::STANDARD};
use reelarr_db::secrets::{SecretCipher, SecretError, XChaChaSecretCipher};

const SECRET: &str = "synthetic-api-key-not-for-diagnostics-5728";
const CONTEXT: &[u8] = b"reelarr:indexers:api_key:example";

#[test]
fn versioned_cipher_roundtrip_random_nonce_and_fail_closed() {
    let cipher = XChaChaSecretCipher::new([19; 32]);
    let first = cipher.encrypt(CONTEXT, SECRET).unwrap();
    let second = cipher.encrypt(CONTEXT, SECRET).unwrap();
    assert!(first.starts_with(b"RSC\x01"));
    assert_ne!(first, second);
    assert!(!first.windows(SECRET.len()).any(|v| v == SECRET.as_bytes()));
    assert_eq!(cipher.decrypt(CONTEXT, &first).unwrap().expose(), SECRET);
    let wrong = XChaChaSecretCipher::new([20; 32]);
    assert_eq!(
        wrong.decrypt(CONTEXT, &first).unwrap_err(),
        SecretError::Decrypt
    );
    assert_eq!(
        cipher.decrypt(b"another record", &first).unwrap_err(),
        SecretError::Decrypt
    );
    let mut corrupt = first.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    assert_eq!(
        cipher.decrypt(CONTEXT, &corrupt).unwrap_err(),
        SecretError::Decrypt
    );
    let mut unsupported = first.clone();
    unsupported[3] = 2;
    assert_eq!(
        cipher.decrypt(CONTEXT, &unsupported).unwrap_err(),
        SecretError::InvalidCiphertext
    );
    for len in 0..44 {
        assert_eq!(
            cipher.decrypt(CONTEXT, &first[..len]).unwrap_err(),
            SecretError::InvalidCiphertext
        );
    }
    assert!(
        !format!(
            "{cipher:?} {:?}",
            cipher.decrypt(CONTEXT, &corrupt).unwrap_err()
        )
        .contains(SECRET)
    );
}

#[test]
fn base64_key_requires_exactly_32_bytes_and_redacts_invalid_input() {
    let key = STANDARD.encode([87; 32]);
    let cipher = XChaChaSecretCipher::from_base64_key(&key).unwrap();
    let other = XChaChaSecretCipher::new([87; 32]);
    assert_eq!(
        other
            .decrypt(CONTEXT, &cipher.encrypt(CONTEXT, SECRET).unwrap())
            .unwrap()
            .expose(),
        SECRET
    );
    assert!(!format!("{cipher:?}").contains(&key));
    for bad in [
        String::new(),
        SECRET.into(),
        STANDARD.encode([87; 31]),
        STANDARD.encode([87; 33]),
    ] {
        let error = XChaChaSecretCipher::from_base64_key(&bad).unwrap_err();
        assert_eq!(error, SecretError::InvalidKey);
        assert!(error.to_string().contains("32 bytes"));
        assert!(!format!("{error} {error:?}").contains(SECRET));
    }
}

#[cfg(unix)]
#[test]
fn key_file_is_private_persistent_and_not_replaced() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let cipher = XChaChaSecretCipher::from_key_file(dir.path()).unwrap();
    let path = dir.path().join("secret.key");
    let metadata = std::fs::metadata(&path).unwrap();
    assert_eq!(metadata.mode() & 0o7777, 0o600);
    assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
    assert_eq!(metadata.len(), 32);
    let before = std::fs::read(&path).unwrap();
    let reopened = XChaChaSecretCipher::from_key_file(dir.path()).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), metadata.ino());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        reopened
            .decrypt(CONTEXT, &cipher.encrypt(CONTEXT, SECRET).unwrap())
            .unwrap()
            .expose(),
        SECRET
    );
}

#[cfg(unix)]
#[test]
fn concurrent_key_initialization_publishes_one_complete_key() {
    let dir = tempfile::tempdir().unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(12));
    let threads: Vec<_> = (0..12)
        .map(|_| {
            let path = dir.path().to_owned();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                XChaChaSecretCipher::from_key_file(&path).unwrap()
            })
        })
        .collect();
    let ciphers: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    let encrypted = ciphers[0].encrypt(CONTEXT, SECRET).unwrap();
    for cipher in ciphers {
        assert_eq!(
            cipher.decrypt(CONTEXT, &encrypted).unwrap().expose(),
            SECRET
        );
    }
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "temporary files are removed"
    );
}

#[cfg(unix)]
#[test]
fn unsafe_or_malformed_existing_files_fail_without_overwrite() {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secret.key");
    for contents in [vec![], vec![71; 31], vec![71; 33]] {
        fs::write(&path, &contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            XChaChaSecretCipher::from_key_file(dir.path()).unwrap_err(),
            SecretError::KeyFileLength
        );
        assert_eq!(fs::read(&path).unwrap(), contents);
    }
    fs::write(&path, [7; 32]).unwrap();
    for mode in [0o644, 0o640, 0o400, 0o660] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            XChaChaSecretCipher::from_key_file(dir.path()).unwrap_err(),
            SecretError::KeyFilePermissions
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            mode
        );
    }
    fs::remove_file(&path).unwrap();
    let target = dir.path().join("target");
    fs::write(&target, [8; 32]).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&target, &path).unwrap();
    assert_eq!(
        XChaChaSecretCipher::from_key_file(dir.path()).unwrap_err(),
        SecretError::KeyFile
    );
    assert!(
        fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&target).unwrap(), [8; 32]);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(
        XChaChaSecretCipher::from_key_file(dir.path()).unwrap_err(),
        SecretError::KeyFilePermissions
    );
}

// Use a child process rather than unsafely mutating the concurrent test runner's
// environment. All values below are synthetic test material, not deployment keys.
#[test]
fn env_initialization_precedence_and_no_silent_fallback() {
    let dir = tempfile::tempdir().unwrap();
    for scenario in ["valid", "invalid", "length", "absent"] {
        let key = match scenario {
            "valid" => Some(STANDARD.encode([91; 32])),
            "invalid" => Some("not-base64".into()),
            "length" => Some(STANDARD.encode([91; 31])),
            _ => None,
        };
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "env_initialization_child"])
            .env("REELARR_TEST_SECRET_SCENARIO", scenario)
            .env("REELARR_DATA_DIR", dir.path())
            .env_remove("REELARR_SECRET_KEY");
        if let Some(key) = key {
            child.env("REELARR_SECRET_KEY", key);
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "child failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(
            dir.path().join("secret.key").exists(),
            scenario == "absent" && cfg!(unix)
        );
    }
}

#[test]
fn env_initialization_child() {
    let Ok(scenario) = std::env::var("REELARR_TEST_SECRET_SCENARIO") else {
        return;
    };
    match scenario.as_str() {
        "valid" => {
            let cipher = XChaChaSecretCipher::from_env().unwrap();
            let direct = XChaChaSecretCipher::new([91; 32]);
            assert_eq!(
                direct
                    .decrypt(CONTEXT, &cipher.encrypt(CONTEXT, SECRET).unwrap())
                    .unwrap()
                    .expose(),
                SECRET
            );
        }
        "absent" if cfg!(unix) => {
            XChaChaSecretCipher::from_env().unwrap();
        }
        "absent" => {
            assert_eq!(
                XChaChaSecretCipher::from_env().unwrap_err(),
                SecretError::UnsupportedPlatform
            );
        }
        "invalid" | "length" => {
            assert_eq!(
                XChaChaSecretCipher::from_env().unwrap_err(),
                SecretError::InvalidKey
            );
        }
        _ => panic!("unknown test scenario"),
    }
}
