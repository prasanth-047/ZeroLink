//! Current-user protected storage for the shared secret established during pairing.
use std::path::{Path, PathBuf};
use windows::core::w;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

fn pairing_path() -> Result<PathBuf, String> {
    let app_data = std::env::var_os("APPDATA")
        .ok_or_else(|| "APPDATA is not set; cannot store the paired-device secret".to_string())?;
    Ok(PathBuf::from(app_data)
        .join("ZeroLink")
        .join("paired-peer.dat"))
}

fn protect(secret: &[u8]) -> Result<Vec<u8>, String> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: secret.len() as u32,
        pbData: secret.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            w!("ZeroLink paired-device secret"),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|e| format!("DPAPI protect failed: {e}"))?;
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData.cast()));
        Ok(bytes)
    }
}

fn unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: ciphertext.len() as u32,
        pbData: ciphertext.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|e| format!("DPAPI unprotect failed: {e}"))?;
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData.cast()));
        Ok(bytes)
    }
}

pub fn load_pairing_secret() -> Result<Option<Vec<u8>>, String> {
    let path = pairing_path()?;
    load_pairing_secret_at(&path)
}

fn load_pairing_secret_at(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let encrypted = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not read paired-device secret: {error}")),
    };
    let secret = unprotect(&encrypted)?;
    if secret.len() != 32 {
        return Err("Stored paired-device secret has an invalid length".into());
    }
    Ok(Some(secret))
}

pub fn save_pairing_secret(secret: &[u8]) -> Result<(), String> {
    save_pairing_secret_at(&pairing_path()?, secret)
}

fn save_pairing_secret_at(path: &Path, secret: &[u8]) -> Result<(), String> {
    if secret.len() != 32 {
        return Err("Paired-device secret must be 32 bytes".into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "Invalid pairing path".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("Could not create pairing directory: {e}"))?;
    let encrypted = protect(secret)?;
    let temp = path.with_extension("dat.tmp");
    std::fs::write(&temp, encrypted)
        .map_err(|e| format!("Could not write paired-device secret: {e}"))?;
    std::fs::rename(&temp, path).map_err(|e| format!("Could not commit paired-device secret: {e}"))
}

pub fn clear_pairing_secret() -> Result<(), String> {
    clear_pairing_secret_at(&pairing_path()?)
}

fn clear_pairing_secret_at(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not remove paired-device secret: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{clear_pairing_secret_at, load_pairing_secret_at, save_pairing_secret_at};
    use std::path::PathBuf;

    struct TestDirectory(PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_secret_path() -> (TestDirectory, PathBuf) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "zerolink-pairing-store-{}-{unique}",
            std::process::id()
        ));
        let path = root.join("ZeroLink").join("paired-peer.dat");
        (TestDirectory(root), path)
    }

    #[test]
    fn dpapi_pairing_secret_round_trips_and_clears() {
        let (_directory, path) = test_secret_path();
        let secret = [0xA5; 32];

        save_pairing_secret_at(&path, &secret).unwrap();
        let protected = std::fs::read(&path).unwrap();
        assert_ne!(protected, secret);
        assert_eq!(
            load_pairing_secret_at(&path).unwrap(),
            Some(secret.to_vec())
        );

        clear_pairing_secret_at(&path).unwrap();
        assert_eq!(load_pairing_secret_at(&path).unwrap(), None);
    }

    #[test]
    fn pairing_store_rejects_secrets_with_the_wrong_length() {
        let (_directory, path) = test_secret_path();
        assert!(save_pairing_secret_at(&path, &[0xA5; 31]).is_err());
    }
}
