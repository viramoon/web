//! Seal credential payloads for the deployment's recipient without holding a private key.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openssl::{
    encrypt::Encrypter,
    hash::MessageDigest,
    pkey::{Id, PKey, Public},
    rand::rand_bytes,
    rsa::Padding,
    symm::{Cipher, encrypt_aead},
};
use rustix::{
    fs::{AtFlags, Mode, OFlags, RenameFlags, openat, renameat_with, unlinkat},
    process::geteuid,
};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::{File, OpenOptions, Permissions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;
use zeroize::Zeroizing;

pub const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const MAX_KEY_FILE_BYTES: u64 = 8192;

#[derive(Clone)]
pub struct CredentialStore(Arc<Store>);

struct Store {
    directory: File,
    recipient: PKey<Public>,
    key_id: String,
}

// Keep deployment paths, SQL errors, crypto errors and input values out of
// error responses and logs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialError {
    Directory,
    Recipient,
    Input,
    Sealing,
    Storage,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Directory => "Credential directory must be owned by web with mode 0700",
            Self::Recipient => {
                "Credential recipient must be a protected RSA public key in PEM format"
            }
            Self::Input => "Invalid credential size",
            Self::Sealing => "Could not seal credential",
            Self::Storage => "Could not store sealed credential",
        })
    }
}

impl std::error::Error for CredentialError {}

impl CredentialStore {
    pub fn open(root: &Path, recipient_public_key_file: &Path) -> Result<Self, CredentialError> {
        if !root.is_absolute() || root.components().any(|part| part == Component::ParentDir) {
            return Err(CredentialError::Directory);
        }
        // Dropping a trailing slash or "." keeps O_NOFOLLOW on the configured directory.
        let root: PathBuf = root.components().collect();
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(root)
            .map_err(|_| CredentialError::Directory)?;
        check_directory(&directory)?;
        let recipient = read_recipient(recipient_public_key_file)?;
        let der = recipient
            .public_key_to_der()
            .map_err(|_| CredentialError::Recipient)?;
        let key_id = URL_SAFE_NO_PAD.encode(Sha256::digest(der));
        Ok(Self(Arc::new(Store {
            directory,
            recipient,
            key_id,
        })))
    }

    /// Return only after the envelope and its directory entry are durable.
    pub async fn seal(&self, reference: Uuid, plaintext: Vec<u8>) -> Result<(), CredentialError> {
        let plaintext = Zeroizing::new(plaintext);
        if plaintext.is_empty() || plaintext.len() > MAX_CREDENTIAL_BYTES {
            return Err(CredentialError::Input);
        }
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let envelope = store.0.envelope(reference, &plaintext)?;
            drop(plaintext);
            store.0.persist(reference, envelope.as_bytes())
        })
        .await
        .map_err(|_| CredentialError::Sealing)?
    }
}

fn check_directory(directory: &File) -> Result<(), CredentialError> {
    let meta = directory
        .metadata()
        .map_err(|_| CredentialError::Directory)?;
    if meta.is_dir() && meta.mode() & 0o7777 == 0o700 && meta.uid() == geteuid().as_raw() {
        Ok(())
    } else {
        Err(CredentialError::Directory)
    }
}

fn read_recipient(path: &Path) -> Result<PKey<Public>, CredentialError> {
    if !path.is_absolute() {
        return Err(CredentialError::Recipient);
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| CredentialError::Recipient)?;
    let meta = file.metadata().map_err(|_| CredentialError::Recipient)?;
    if !meta.is_file()
        || meta.mode() & 0o022 != 0
        || (meta.uid() != 0 && meta.uid() != geteuid().as_raw())
    {
        return Err(CredentialError::Recipient);
    }
    let mut input = Zeroizing::new(String::new());
    file.take(MAX_KEY_FILE_BYTES + 1)
        .read_to_string(&mut input)
        .map_err(|_| CredentialError::Recipient)?;
    let body = input
        .trim()
        .strip_prefix("-----BEGIN PUBLIC KEY-----")
        .and_then(|pem| pem.strip_suffix("-----END PUBLIC KEY-----"))
        .ok_or(CredentialError::Recipient)?;
    if input.len() as u64 > MAX_KEY_FILE_BYTES
        || body.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || byte.is_ascii_whitespace() || b"+/=".contains(&byte))
        })
    {
        return Err(CredentialError::Recipient);
    }
    let key = PKey::public_key_from_pem(input.trim().as_bytes())
        .map_err(|_| CredentialError::Recipient)?;
    if key.id() != Id::RSA || !(2048..=8192).contains(&key.bits()) {
        return Err(CredentialError::Recipient);
    }
    Ok(key)
}

impl Store {
    fn envelope(&self, reference: Uuid, plaintext: &[u8]) -> Result<String, CredentialError> {
        // JWE Compact Serialization (RFC 7516). The protected header is also
        // the AAD, so consumers can verify the UUID against the database reference.
        let header = serde_json::json!({
            "alg": "RSA-OAEP-256",
            "enc": "A256GCM",
            "kid": self.key_id,
            "credential_id": reference.to_string(),
        });
        let protected = URL_SAFE_NO_PAD.encode(header.to_string());
        let mut key = Zeroizing::new([0; 32]);
        let mut iv = [0; 12];
        let mut tag = [0; 16];
        let sealed = (|| -> Result<_, openssl::error::ErrorStack> {
            rand_bytes(&mut *key)?;
            rand_bytes(&mut iv)?;
            let mut encrypter = Encrypter::new(&self.recipient)?;
            encrypter.set_rsa_padding(Padding::PKCS1_OAEP)?;
            encrypter.set_rsa_oaep_md(MessageDigest::sha256())?;
            encrypter.set_rsa_mgf1_md(MessageDigest::sha256())?;
            let mut wrapped_key = vec![0; encrypter.encrypt_len(&*key)?];
            let written = encrypter.encrypt(&*key, &mut wrapped_key)?;
            wrapped_key.truncate(written);
            let ciphertext = encrypt_aead(
                Cipher::aes_256_gcm(),
                &*key,
                Some(&iv),
                protected.as_bytes(),
                plaintext,
                &mut tag,
            )?;
            Ok((wrapped_key, ciphertext))
        })();
        let (wrapped_key, ciphertext) = sealed.map_err(|_| CredentialError::Sealing)?;
        Ok(format!(
            "{protected}.{}.{}.{}.{}",
            URL_SAFE_NO_PAD.encode(wrapped_key),
            URL_SAFE_NO_PAD.encode(iv),
            URL_SAFE_NO_PAD.encode(ciphertext),
            URL_SAFE_NO_PAD.encode(tag),
        ))
    }

    fn persist(&self, reference: Uuid, envelope: &[u8]) -> Result<(), CredentialError> {
        check_directory(&self.directory)?;
        let destination = format!("{reference}.jwe");
        let temporary = format!(".{}.tmp", Uuid::new_v4());
        let descriptor = openat(
            &self.directory,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| CredentialError::Storage)?;
        let mut file = File::from(descriptor);
        let result = (|| -> std::io::Result<()> {
            file.set_permissions(Permissions::from_mode(0o600))?;
            file.write_all(envelope)?;
            file.sync_all()?;
            renameat_with(
                &self.directory,
                temporary.as_str(),
                &self.directory,
                destination.as_str(),
                RenameFlags::NOREPLACE,
            )?;
            self.directory.sync_all()
        })();
        if result.is_err() {
            let _ = unlinkat(&self.directory, temporary.as_str(), AtFlags::empty());
        }
        result.map_err(|_| CredentialError::Storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::{encrypt::Decrypter, pkey::Private, rsa::Rsa, symm::decrypt_aead};
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, symlink},
    };

    fn test_recipient(dir: &Path) -> (PKey<Private>, PathBuf) {
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let private = dir.join("private.pem");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&private)
            .unwrap()
            .write_all(&key.private_key_to_pem_pkcs8().unwrap())
            .unwrap();
        let public = dir.join("public.pem");
        fs::write(&public, key.public_key_to_pem().unwrap()).unwrap();
        let private = PKey::private_key_from_pem(&fs::read(private).unwrap()).unwrap();
        (private, public)
    }

    #[tokio::test]
    async fn compact_envelope_round_trip_authenticates_metadata_and_never_overwrites() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let (private, public) = test_recipient(temp.path());
        let directory = temp.path().join("envelopes");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let store = CredentialStore::open(&directory, &public).unwrap();
        let reference = Uuid::new_v4();
        let plaintext = format!("test-only-credential-{}", Uuid::new_v4());
        store
            .seal(reference, plaintext.as_bytes().to_vec())
            .await
            .unwrap();
        let path = directory.join(format!("{reference}.jwe"));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
        let sealed = fs::read_to_string(&path).unwrap();
        assert!(!sealed.contains(&plaintext));

        let mut parts = sealed.split('.');
        let protected = parts.next().unwrap();
        let decoded = parts
            .map(|part| URL_SAFE_NO_PAD.decode(part).unwrap())
            .collect::<Vec<_>>();
        let [wrapped_key, iv, ciphertext, tag] = decoded.as_slice() else {
            panic!("expected four encoded segments after the protected header");
        };
        let mut decrypter = Decrypter::new(&private).unwrap();
        decrypter.set_rsa_padding(Padding::PKCS1_OAEP).unwrap();
        decrypter.set_rsa_oaep_md(MessageDigest::sha256()).unwrap();
        decrypter.set_rsa_mgf1_md(MessageDigest::sha256()).unwrap();
        let mut key = Zeroizing::new(vec![0; decrypter.decrypt_len(wrapped_key).unwrap()]);
        let size = decrypter.decrypt(wrapped_key, &mut key).unwrap();
        assert_eq!(size, 32);
        let open = |aad: &[u8]| {
            decrypt_aead(
                Cipher::aes_256_gcm(),
                &key[..size],
                Some(iv),
                aad,
                ciphertext,
                tag,
            )
        };
        let opened = Zeroizing::new(open(protected.as_bytes()).unwrap());
        assert!(opened.as_slice() == plaintext.as_bytes());
        let mut header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(protected).unwrap()).unwrap();
        assert_eq!(header["alg"], "RSA-OAEP-256");
        assert_eq!(header["enc"], "A256GCM");
        assert_eq!(header["credential_id"], reference.to_string());
        assert_eq!(
            header["kid"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(private.public_key_to_der().unwrap()))
        );
        header["credential_id"] = Uuid::new_v4().to_string().into();
        assert!(open(URL_SAFE_NO_PAD.encode(header.to_string()).as_bytes()).is_err());

        assert_eq!(
            store.seal(reference, b"second version".to_vec()).await,
            Err(CredentialError::Storage)
        );
        assert_eq!(fs::read_to_string(path).unwrap(), sealed);
        assert_eq!(fs::read_dir(directory).unwrap().count(), 1);
    }

    #[test]
    fn seal_store_requires_controlled_directory_and_public_key() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let (_, public) = test_recipient(temp.path());
        let directory = temp.path().join("envelopes");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(matches!(
            CredentialStore::open(&directory, &public),
            Err(CredentialError::Directory)
        ));
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let link = temp.path().join("linked");
        symlink(&directory, &link).unwrap();
        assert!(matches!(
            CredentialStore::open(&link.join("."), &public),
            Err(CredentialError::Directory)
        ));
        fs::set_permissions(&public, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(matches!(
            CredentialStore::open(&directory, &public),
            Err(CredentialError::Recipient)
        ));
        assert!(matches!(
            CredentialStore::open(&directory, &temp.path().join("private.pem")),
            Err(CredentialError::Recipient)
        ));
    }
}
