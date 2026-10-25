//! Built-in password manager: storage, exact-origin matching, generation, and Chrome import.
//!
//! Saved logins live in the login keychain as generic passwords under one service. Each
//! item's non-secret attributes hold the origin and username, so listing never reads a
//! secret. Test profiles (`ATRIUM_MOCK_KEYCHAIN=1`) use an encrypted file in the data
//! directory instead, so tests never touch the keychain.
use std::{
    collections::BTreeSet,
    ffi::c_void,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const SERVICE: &str = "Atrium Passwords";
const VAULT: &str = "passwords-test.vault";
const VAULT_KEY: &str = "passwords-test.key";
const NEVER: &str = "password-never.json";
const MAGIC: &[u8] = b"ASPV1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Login {
    pub id: String,
    pub origin: String,
    pub username: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
    pub created: u64,
    pub updated: u64,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Saved {
    New,
    Updated,
    Unchanged,
}

/// `scheme://host[:port]` for http(s) URLs; hosts are ASCII (punycode), so lookalikes stay distinct.
pub fn origin_of(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    let origin = url.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn random_bytes(out: &mut [u8]) -> Result<()> {
    anyhow::ensure!(
        unsafe { cc::CCRandomGenerateBytes(out.as_mut_ptr().cast(), out.len()) } == 0,
        "The system random number generator failed"
    );
    Ok(())
}

/// 20 characters with lower, upper, digit, and symbol, avoiding look-alike characters.
pub fn generate() -> Result<String> {
    const SETS: [&[u8]; 4] = [b"abcdefghijkmnopqrstuvwxyz", b"ABCDEFGHJKLMNPQRSTUVWXYZ", b"23456789", b"-_.!@#$%"];
    let all: Vec<u8> = SETS.concat();
    let pick = |set: &[u8]| -> Result<u8> {
        let limit = 256 - 256 % set.len();
        loop {
            let mut b = [0u8];
            random_bytes(&mut b)?;
            if (b[0] as usize) < limit {
                return Ok(set[b[0] as usize % set.len()]);
            }
        }
    };
    loop {
        let mut out = Vec::with_capacity(20);
        for _ in 0..20 {
            out.push(pick(&all)?);
        }
        if SETS.iter().all(|set| out.iter().any(|c| set.contains(c))) {
            return Ok(String::from_utf8(out).unwrap());
        }
    }
}

enum Backend {
    Keychain { service: String },
    Vault { path: PathBuf, key: [u8; 64] },
}

pub struct Store {
    backend: Backend,
    dir: PathBuf,
}

static STORE: OnceLock<Mutex<Store>> = OnceLock::new();

/// The process-wide store for the running profile. Calls are serialized.
pub fn with<T>(f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
    let store = match STORE.get() {
        Some(s) => s,
        None => {
            let dir = crate::data_dir()?;
            let mock = std::env::var_os("ATRIUM_MOCK_KEYCHAIN").is_some();
            let _ = STORE.set(Mutex::new(Store::open(&dir, mock)?));
            STORE.get().unwrap()
        }
    };
    f(&store.lock().unwrap_or_else(|e| e.into_inner()))
}

pub fn is_mock() -> bool {
    std::env::var_os("ATRIUM_MOCK_KEYCHAIN").is_some()
}

impl Store {
    pub fn open(dir: &Path, mock: bool) -> Result<Self> {
        let backend = if mock {
            Backend::Vault { path: dir.join(VAULT), key: vault_key(&dir.join(VAULT_KEY))? }
        } else {
            Backend::Keychain { service: SERVICE.into() }
        };
        Ok(Self { backend, dir: dir.to_path_buf() })
    }

    #[cfg(test)]
    fn keychain_for_test(dir: &Path, service: &str) -> Self {
        Self { backend: Backend::Keychain { service: service.into() }, dir: dir.to_path_buf() }
    }

    pub fn kind(&self) -> &'static str {
        match self.backend {
            Backend::Keychain { .. } => "keychain",
            Backend::Vault { .. } => "test-vault",
        }
    }

    /// Every saved login without its password.
    pub fn list(&self) -> Result<Vec<Login>> {
        let mut logins = match &self.backend {
            Backend::Keychain { service } => keychain::list(service)?,
            Backend::Vault { .. } => self.read_vault()?.into_iter().map(|l| Login { password: String::new(), ..l }).collect(),
        };
        logins.sort_by(|a, b| (&a.origin, &a.username).cmp(&(&b.origin, &b.username)));
        Ok(logins)
    }

    pub fn for_origin(&self, origin: &str) -> Result<Vec<Login>> {
        Ok(self.list()?.into_iter().filter(|l| l.origin == origin).collect())
    }

    /// One login including its password.
    pub fn get(&self, id: &str) -> Result<Login> {
        match &self.backend {
            Backend::Keychain { service } => keychain::get(service, id),
            Backend::Vault { .. } => self.read_vault()?.into_iter().find(|l| l.id == id).context("This saved password no longer exists"),
        }
    }

    fn put(&self, login: &Login) -> Result<()> {
        match &self.backend {
            Backend::Keychain { service } => keychain::put(service, login),
            Backend::Vault { .. } => {
                let mut all = self.read_vault()?;
                all.retain(|l| l.id != login.id);
                all.push(login.clone());
                self.write_vault(&all)
            }
        }
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        match &self.backend {
            Backend::Keychain { service } => keychain::delete(service, id),
            Backend::Vault { .. } => {
                let mut all = self.read_vault()?;
                let before = all.len();
                all.retain(|l| l.id != id);
                anyhow::ensure!(all.len() < before, "This saved password no longer exists");
                self.write_vault(&all)
            }
        }
    }

    /// What saving would do, without changing anything.
    pub fn check(&self, origin: &str, username: &str, password: &str) -> Result<Saved> {
        Ok(match self.for_origin(origin)?.into_iter().find(|l| l.username == username) {
            None => Saved::New,
            Some(l) if self.get(&l.id)?.password == password => Saved::Unchanged,
            Some(_) => Saved::Updated,
        })
    }

    /// Adds a login, or replaces the password of the same username on the same origin.
    pub fn save(&self, origin: &str, username: &str, password: &str) -> Result<(Login, Saved)> {
        anyhow::ensure!(origin_of(origin).as_deref() == Some(origin), "Passwords can only be saved for web origins");
        anyhow::ensure!(!password.is_empty() && password.len() <= 4096 && username.len() <= 1024, "This password cannot be saved");
        if let Some(existing) = self.for_origin(origin)?.into_iter().find(|l| l.username == username) {
            let mut login = self.get(&existing.id)?;
            if login.password == password {
                return Ok((login, Saved::Unchanged));
            }
            login.password = password.into();
            login.updated = now();
            self.put(&login)?;
            return Ok((login, Saved::Updated));
        }
        let t = now();
        let login = Login { id: uuid::Uuid::new_v4().to_string(), origin: origin.into(), username: username.into(), password: password.into(), created: t, updated: t };
        self.put(&login)?;
        Ok((login, Saved::New))
    }

    pub fn edit(&self, id: &str, username: Option<&str>, password: Option<&str>) -> Result<Login> {
        let mut login = self.get(id)?;
        if let Some(u) = username {
            anyhow::ensure!(u.len() <= 1024, "This username is too long");
            if u != login.username && self.for_origin(&login.origin)?.iter().any(|l| l.username == u) {
                bail!("A password for this username is already saved for this site");
            }
            login.username = u.into();
        }
        if let Some(p) = password {
            anyhow::ensure!(!p.is_empty() && p.len() <= 4096, "Enter a password");
            login.password = p.into();
        }
        login.updated = now();
        self.put(&login)?;
        Ok(Login { password: String::new(), ..login })
    }

    /// Chrome's CSV layout (name,url,username,password,note). Contains every secret.
    pub fn export_csv(&self) -> Result<String> {
        let field = |s: &str| {
            // Leading formula characters are neutralized so spreadsheets don't evaluate them.
            let s = if s.starts_with(['=', '+', '-', '@']) { format!("'{s}") } else { s.to_string() };
            format!("\"{}\"", s.replace('"', "\"\""))
        };
        let mut out = String::from("name,url,username,password,note\n");
        for meta in self.list()? {
            let login = self.get(&meta.id)?;
            let name = reqwest::Url::parse(&login.origin).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
            out.push_str(&[field(&name), field(&login.origin), field(&login.username), field(&login.password), field("")].join(","));
            out.push('\n');
        }
        Ok(out)
    }

    pub fn never(&self) -> BTreeSet<String> {
        std::fs::read(self.dir.join(NEVER)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn set_never(&self, origin: &str, never: bool) -> Result<()> {
        let mut all = self.never();
        if never {
            anyhow::ensure!(origin_of(origin).as_deref() == Some(origin), "Not a web origin");
            all.insert(origin.into());
        } else {
            all.remove(origin);
        }
        write_private(&self.dir.join(NEVER), &serde_json::to_vec(&all)?)
    }

    fn read_vault(&self) -> Result<Vec<Login>> {
        let Backend::Vault { path, key } = &self.backend else { unreachable!() };
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        Ok(serde_json::from_slice(&open_sealed(key, &data)?)?)
    }

    fn write_vault(&self, all: &[Login]) -> Result<()> {
        let Backend::Vault { path, key } = &self.backend else { unreachable!() };
        write_private(path, &seal(key, &serde_json::to_vec(all)?)?)
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let temp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temp, path)?;
    Ok(())
}

fn vault_key(path: &Path) -> Result<[u8; 64]> {
    if let Ok(bytes) = std::fs::read(path) {
        return bytes.try_into().map_err(|_| anyhow!("The test password vault key is damaged"));
    }
    let mut key = [0u8; 64];
    random_bytes(&mut key)?;
    write_private(path, &key)?;
    Ok(key)
}

/// AES-256-CBC, then HMAC-SHA256 over everything before the tag.
fn seal(key: &[u8; 64], plain: &[u8]) -> Result<Vec<u8>> {
    let mut iv = [0u8; 16];
    random_bytes(&mut iv)?;
    let mut out = [MAGIC, &iv].concat();
    out.extend(cc::aes_cbc(true, &key[..32], &iv, plain)?);
    let tag = cc::hmac_sha256(&key[32..], &out);
    out.extend(tag);
    Ok(out)
}

fn open_sealed(key: &[u8; 64], data: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(data.len() >= MAGIC.len() + 16 + 16 + 32 && data.starts_with(MAGIC), "The test password vault is damaged");
    let (body, tag) = data.split_at(data.len() - 32);
    let expected = cc::hmac_sha256(&key[32..], body);
    anyhow::ensure!(expected.iter().zip(tag).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0, "The test password vault is damaged");
    let iv = &body[MAGIC.len()..MAGIC.len() + 16];
    cc::aes_cbc(false, &key[..32], iv, &body[MAGIC.len() + 16..])
}

/// CommonCrypto from libSystem.
mod cc {
    use super::*;
    extern "C" {
        #[allow(clippy::too_many_arguments)]
        fn CCCrypt(op: u32, alg: u32, options: u32, key: *const u8, key_len: usize, iv: *const u8, data_in: *const u8, in_len: usize, data_out: *mut u8, out_avail: usize, out_moved: *mut usize) -> i32;
        #[allow(clippy::too_many_arguments)]
        fn CCKeyDerivationPBKDF(algorithm: u32, password: *const u8, password_len: usize, salt: *const u8, salt_len: usize, prf: u32, rounds: u32, derived: *mut u8, derived_len: usize) -> i32;
        fn CCHmac(alg: u32, key: *const c_void, key_len: usize, data: *const c_void, data_len: usize, mac_out: *mut c_void);
        pub fn CCRandomGenerateBytes(bytes: *mut c_void, count: usize) -> i32;
    }
    pub fn aes_cbc(encrypt: bool, key: &[u8], iv: &[u8], input: &[u8]) -> Result<Vec<u8>> {
        let mut out = vec![0u8; input.len() + 16];
        let mut moved = 0usize;
        let status = unsafe {
            CCCrypt(if encrypt { 0 } else { 1 }, 0, 1, key.as_ptr(), key.len(), iv.as_ptr(), input.as_ptr(), input.len(), out.as_mut_ptr(), out.len(), &mut moved)
        };
        anyhow::ensure!(status == 0 && (encrypt || input.len() % 16 == 0), "Decryption failed");
        out.truncate(moved);
        Ok(out)
    }
    pub fn pbkdf2_sha1(password: &[u8], salt: &[u8], rounds: u32, out: &mut [u8]) -> Result<()> {
        let status = unsafe { CCKeyDerivationPBKDF(2, password.as_ptr(), password.len(), salt.as_ptr(), salt.len(), 1, rounds, out.as_mut_ptr(), out.len()) };
        anyhow::ensure!(status == 0, "Key derivation failed");
        Ok(())
    }
    pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
        let mut out = [0u8; 32];
        unsafe { CCHmac(2, key.as_ptr().cast(), key.len(), data.as_ptr().cast(), data.len(), out.as_mut_ptr().cast()) };
        out
    }
}

/// Security framework generic passwords in the login keychain.
mod keychain {
    use super::*;
    type Ref = *const c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
        static kCFBooleanTrue: Ref;
        fn CFDictionaryCreate(alloc: Ref, keys: *const Ref, values: *const Ref, count: isize, key_cb: *const c_void, value_cb: *const c_void) -> Ref;
        fn CFDictionaryGetValue(dict: Ref, key: Ref) -> Ref;
        fn CFStringCreateWithBytes(alloc: Ref, bytes: *const u8, len: isize, encoding: u32, external: u8) -> Ref;
        fn CFDataCreate(alloc: Ref, bytes: *const u8, len: isize) -> Ref;
        fn CFDataGetBytePtr(data: Ref) -> *const u8;
        fn CFDataGetLength(data: Ref) -> isize;
        fn CFDataGetTypeID() -> usize;
        fn CFArrayGetCount(array: Ref) -> isize;
        fn CFArrayGetValueAtIndex(array: Ref, index: isize) -> Ref;
        fn CFGetTypeID(value: Ref) -> usize;
        fn CFRelease(value: Ref);
    }
    #[link(name = "Security", kind = "framework")]
    extern "C" {
        static kSecClass: Ref;
        static kSecClassGenericPassword: Ref;
        static kSecAttrService: Ref;
        static kSecAttrAccount: Ref;
        static kSecAttrGeneric: Ref;
        static kSecAttrLabel: Ref;
        static kSecValueData: Ref;
        static kSecReturnAttributes: Ref;
        static kSecReturnData: Ref;
        static kSecMatchLimit: Ref;
        static kSecMatchLimitAll: Ref;
        static kSecMatchLimitOne: Ref;
        fn SecItemAdd(attributes: Ref, result: *mut Ref) -> i32;
        fn SecItemCopyMatching(query: Ref, result: *mut Ref) -> i32;
        fn SecItemUpdate(query: Ref, attributes: Ref) -> i32;
        fn SecItemDelete(query: Ref) -> i32;
        #[cfg(test)]
        pub fn SecKeychainSetUserInteractionAllowed(allowed: u8) -> i32;
    }
    const NOT_FOUND: i32 = -25300;
    const DUPLICATE: i32 = -25299;

    struct Cf(Ref);
    impl Drop for Cf {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) }
            }
        }
    }
    fn string(s: &str) -> Cf {
        Cf(unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, 0x0800_0100, 0) })
    }
    fn data(b: &[u8]) -> Cf {
        Cf(unsafe { CFDataCreate(std::ptr::null(), b.as_ptr(), b.len() as isize) })
    }
    fn bytes(value: Ref) -> Option<Vec<u8>> {
        if value.is_null() || unsafe { CFGetTypeID(value) != CFDataGetTypeID() } {
            return None;
        }
        let len = unsafe { CFDataGetLength(value) } as usize;
        Some(unsafe { std::slice::from_raw_parts(CFDataGetBytePtr(value), len) }.to_vec())
    }
    fn dict(pairs: &[(Ref, Ref)]) -> Cf {
        let keys: Vec<Ref> = pairs.iter().map(|p| p.0).collect();
        let values: Vec<Ref> = pairs.iter().map(|p| p.1).collect();
        Cf(unsafe {
            CFDictionaryCreate(std::ptr::null(), keys.as_ptr(), values.as_ptr(), pairs.len() as isize, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks)
        })
    }
    fn check(status: i32, what: &str) -> Result<()> {
        match status {
            0 => Ok(()),
            -128 => bail!("Keychain access was denied ({what})"),
            -25308 => bail!("The keychain needs your permission but cannot ask right now ({what})"),
            s => bail!("The keychain could not {what} (error {s})"),
        }
    }

    #[derive(Serialize, Deserialize)]
    struct Meta {
        id: String,
        origin: String,
        username: String,
        created: u64,
        updated: u64,
    }

    pub fn list(service: &str) -> Result<Vec<Login>> {
        let service = string(service);
        let query = unsafe { dict(&[(kSecClass, kSecClassGenericPassword), (kSecAttrService, service.0), (kSecReturnAttributes, kCFBooleanTrue), (kSecMatchLimit, kSecMatchLimitAll)]) };
        let mut result: Ref = std::ptr::null();
        let status = unsafe { SecItemCopyMatching(query.0, &mut result) };
        if status == NOT_FOUND {
            return Ok(Vec::new());
        }
        check(status, "list saved passwords")?;
        let result = Cf(result);
        let mut out = Vec::new();
        for i in 0..unsafe { CFArrayGetCount(result.0) } {
            let item = unsafe { CFArrayGetValueAtIndex(result.0, i) };
            let generic = bytes(unsafe { CFDictionaryGetValue(item, kSecAttrGeneric) });
            if let Some(meta) = generic.and_then(|g| serde_json::from_slice::<Meta>(&g).ok()) {
                out.push(Login { id: meta.id, origin: meta.origin, username: meta.username, password: String::new(), created: meta.created, updated: meta.updated });
            }
        }
        Ok(out)
    }

    pub fn get(service: &str, id: &str) -> Result<Login> {
        let (service, account) = (string(service), string(id));
        let query = unsafe {
            dict(&[(kSecClass, kSecClassGenericPassword), (kSecAttrService, service.0), (kSecAttrAccount, account.0), (kSecReturnAttributes, kCFBooleanTrue), (kSecReturnData, kCFBooleanTrue), (kSecMatchLimit, kSecMatchLimitOne)])
        };
        let mut result: Ref = std::ptr::null();
        let status = unsafe { SecItemCopyMatching(query.0, &mut result) };
        if status == NOT_FOUND {
            bail!("This saved password no longer exists");
        }
        check(status, "read the saved password")?;
        let result = Cf(result);
        let meta: Meta = serde_json::from_slice(&bytes(unsafe { CFDictionaryGetValue(result.0, kSecAttrGeneric) }).context("Damaged keychain item")?)?;
        let password = String::from_utf8(bytes(unsafe { CFDictionaryGetValue(result.0, kSecValueData) }).context("Damaged keychain item")?)?;
        Ok(Login { id: meta.id, origin: meta.origin, username: meta.username, password, created: meta.created, updated: meta.updated })
    }

    pub fn put(service: &str, login: &Login) -> Result<()> {
        let meta = serde_json::to_vec(&Meta { id: login.id.clone(), origin: login.origin.clone(), username: login.username.clone(), created: login.created, updated: login.updated })?;
        let (service, account, label) = (string(service), string(&login.id), string(&format!("Atrium: {}", login.origin)));
        let (generic, secret) = (data(&meta), data(login.password.as_bytes()));
        let query = unsafe { dict(&[(kSecClass, kSecClassGenericPassword), (kSecAttrService, service.0), (kSecAttrAccount, account.0)]) };
        let changes = unsafe { dict(&[(kSecAttrLabel, label.0), (kSecAttrGeneric, generic.0), (kSecValueData, secret.0)]) };
        let status = unsafe { SecItemUpdate(query.0, changes.0) };
        if status != NOT_FOUND {
            return check(status, "update the saved password");
        }
        let item = unsafe {
            dict(&[(kSecClass, kSecClassGenericPassword), (kSecAttrService, service.0), (kSecAttrAccount, account.0), (kSecAttrLabel, label.0), (kSecAttrGeneric, generic.0), (kSecValueData, secret.0)])
        };
        let status = unsafe { SecItemAdd(item.0, std::ptr::null_mut()) };
        if status == DUPLICATE {
            bail!("The keychain already has this item");
        }
        check(status, "save the password")
    }

    pub fn delete(service: &str, id: &str) -> Result<()> {
        let (service, account) = (string(service), string(id));
        let query = unsafe { dict(&[(kSecClass, kSecClassGenericPassword), (kSecAttrService, service.0), (kSecAttrAccount, account.0)]) };
        let status = unsafe { SecItemDelete(query.0) };
        if status == NOT_FOUND {
            bail!("This saved password no longer exists");
        }
        check(status, "delete the saved password")
    }

    /// Reading another app's item shows the system "allow access" prompt; only call on an explicit user action.
    pub fn read_generic(service: &str, account: &str) -> Result<Vec<u8>> {
        let (service, account) = (string(service), string(account));
        let query = unsafe { dict(&[(kSecClass, kSecClassGenericPassword), (kSecAttrService, service.0), (kSecAttrAccount, account.0), (kSecReturnData, kCFBooleanTrue), (kSecMatchLimit, kSecMatchLimitOne)]) };
        let mut result: Ref = std::ptr::null();
        let status = unsafe { SecItemCopyMatching(query.0, &mut result) };
        if status == NOT_FOUND {
            bail!("Chrome's password key was not found in the keychain");
        }
        check(status, "read Chrome's password key")?;
        let result = Cf(result);
        bytes(result.0).context("Chrome's password key is empty")
    }
}

/// Chrome on macOS: "v10" + AES-128-CBC, key = PBKDF2-HMAC-SHA1("Chrome Safe Storage" secret, "saltysalt", 1003), IV of 16 spaces.
pub mod chrome {
    use super::*;

    pub fn derive_key(safe_storage: &[u8]) -> Result<[u8; 16]> {
        let mut key = [0u8; 16];
        cc::pbkdf2_sha1(safe_storage, b"saltysalt", 1003, &mut key)?;
        Ok(key)
    }

    pub fn decrypt(key: &[u8; 16], blob: &[u8]) -> Result<String> {
        let body = blob.strip_prefix(b"v10").context("Unsupported Chrome password encryption")?;
        anyhow::ensure!(!body.is_empty() && body.len() % 16 == 0, "Damaged Chrome password");
        Ok(String::from_utf8(cc::aes_cbc(false, key, &[b' '; 16], body)?)?)
    }

    #[cfg(test)]
    pub fn encrypt(key: &[u8; 16], plain: &str) -> Vec<u8> {
        [b"v10".as_slice(), &cc::aes_cbc(true, key, &[b' '; 16], plain.as_bytes()).unwrap()].concat()
    }

    /// Explicit user action: macOS asks whether Atrium may read Chrome's key.
    pub fn safe_storage_from_keychain() -> Result<Vec<u8>> {
        keychain::read_generic("Chrome Safe Storage", "Chrome")
    }

    pub struct Imported {
        pub logins: Vec<(String, String, String)>,
        pub never: Vec<String>,
        pub skipped: usize,
    }

    /// Reads a copy of `Login Data` so a running Chrome's lock and files are untouched.
    pub async fn read(profile: &Path, safe_storage: &[u8]) -> Result<Imported> {
        let source = profile.join("Login Data");
        anyhow::ensure!(source.is_file(), "This Chrome profile has no saved passwords file");
        anyhow::ensure!(source.metadata()?.len() < 256 * 1024 * 1024, "Chrome's password file is too large to import");
        let temp = std::env::temp_dir().join(format!("atrium-login-data-{}", uuid::Uuid::new_v4().simple()));
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&temp)?;
        }
        let result = async {
            let copy = temp.join("Login Data");
            std::fs::copy(&source, &copy)?;
            for suffix in ["-journal", "-wal"] {
                let side = profile.join(format!("Login Data{suffix}"));
                if side.is_file() {
                    std::fs::copy(&side, temp.join(format!("Login Data{suffix}")))?;
                }
            }
            let output = tokio::time::timeout(
                Duration::from_secs(20),
                tokio::process::Command::new("/usr/bin/sqlite3")
                    .args(["-readonly", "-json"])
                    .arg(&copy)
                    .arg("SELECT origin_url AS url, username_value AS username, hex(password_value) AS pw, blacklisted_by_user AS never FROM logins")
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .context("Reading Chrome's passwords took too long")??;
            anyhow::ensure!(output.status.success(), "Chrome's password file could not be read");
            let rows: Vec<Value> = if output.stdout.is_empty() { Vec::new() } else { serde_json::from_slice(&output.stdout)? };
            parse(rows, safe_storage)
        }
        .await;
        let _ = std::fs::remove_dir_all(&temp);
        result
    }

    pub(super) fn parse(rows: Vec<Value>, safe_storage: &[u8]) -> Result<Imported> {
        let key = derive_key(safe_storage)?;
        let mut out = Imported { logins: Vec::new(), never: Vec::new(), skipped: 0 };
        for row in rows {
            let Some(origin) = row["url"].as_str().and_then(origin_of) else {
                out.skipped += 1;
                continue;
            };
            if row["never"].as_i64() == Some(1) {
                out.never.push(origin);
                continue;
            }
            let blob = row["pw"].as_str().and_then(unhex).unwrap_or_default();
            match decrypt(&key, &blob) {
                Ok(password) if !password.is_empty() => out.logins.push((origin, row["username"].as_str().unwrap_or("").into(), password)),
                _ => out.skipped += 1,
            }
        }
        Ok(out)
    }

    fn unhex(s: &str) -> Option<Vec<u8>> {
        (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect()
    }
}

/// Saves imported logins; an existing username on the same origin gets Chrome's password.
pub fn import(store: &Store, data: chrome::Imported) -> Result<Value> {
    let (mut added, mut updated, mut unchanged) = (0, 0, 0);
    for (origin, username, password) in &data.logins {
        match store.save(origin, username, password)?.1 {
            Saved::New => added += 1,
            Saved::Updated => updated += 1,
            Saved::Unchanged => unchanged += 1,
        }
    }
    for origin in &data.never {
        store.set_never(origin, true)?;
    }
    Ok(json!({ "added": added, "updated": updated, "unchanged": unchanged, "never": data.never.len(), "skipped": data.skipped }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atrium-passwords-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn origins_are_exact_and_reject_non_web_schemes() {
        assert_eq!(origin_of("https://Example.com/login?x=1").as_deref(), Some("https://example.com"));
        assert_eq!(origin_of("https://example.com:443/").as_deref(), Some("https://example.com"));
        assert_eq!(origin_of("http://127.0.0.1:7831/a").as_deref(), Some("http://127.0.0.1:7831"));
        assert_ne!(origin_of("http://127.0.0.1:7832/"), origin_of("http://127.0.0.1:7831/"));
        assert_ne!(origin_of("https://accounts.example.com/"), origin_of("https://example.com/"));
        assert_ne!(origin_of("http://example.com/"), origin_of("https://example.com/"));
        assert!(origin_of("https://exаmple.com/").unwrap().starts_with("https://xn--"));
        for url in ["file:///etc/passwd", "javascript:alert(1)", "data:text/html,x", "about:blank", "chrome://settings"] {
            assert_eq!(origin_of(url), None, "{url}");
        }
    }

    #[test]
    fn generated_passwords_are_strong_and_distinct() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_eq!(a.len(), 20);
        assert_ne!(a, b);
        assert!(a.bytes().any(|c| c.is_ascii_lowercase()) && a.bytes().any(|c| c.is_ascii_uppercase()) && a.bytes().any(|c| c.is_ascii_digit()));
        assert!(!a.contains(['0', 'O', 'l', '1', 'I']));
    }

    #[test]
    fn vault_round_trips_and_lists_without_secrets() {
        let dir = temp();
        let store = Store::open(&dir, true).unwrap();
        assert_eq!(store.kind(), "test-vault");
        let (login, how) = store.save("http://127.0.0.1:7831", "ada", "s3cret-one").unwrap();
        assert_eq!(how, Saved::New);
        assert_eq!(store.check("http://127.0.0.1:7831", "ada", "s3cret-one").unwrap(), Saved::Unchanged);
        assert_eq!(store.check("http://127.0.0.1:7831", "ada", "other").unwrap(), Saved::Updated);
        assert_eq!(store.check("http://127.0.0.1:7832", "ada", "s3cret-one").unwrap(), Saved::New);
        assert_eq!(store.save("http://127.0.0.1:7831", "ada", "s3cret-two").unwrap().1, Saved::Updated);
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].password.is_empty());
        assert_eq!(store.get(&login.id).unwrap().password, "s3cret-two");
        assert!(store.for_origin("http://127.0.0.1:7832").unwrap().is_empty());
        let raw = std::fs::read(dir.join(VAULT)).unwrap();
        assert!(!raw.windows(10).any(|w| w == b"s3cret-two"));
        assert_eq!(std::fs::metadata(dir.join(VAULT_KEY)).unwrap().permissions().mode() & 0o777, 0o600);
        // Reopening with the same key reads the vault; tampering is detected.
        assert_eq!(Store::open(&dir, true).unwrap().list().unwrap().len(), 1);
        let mut bad = raw.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 1;
        std::fs::write(dir.join(VAULT), bad).unwrap();
        assert!(store.list().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn edit_delete_never_and_export() {
        let dir = temp();
        let store = Store::open(&dir, true).unwrap();
        let (a, _) = store.save("https://example.com", "ada", "pw\"1").unwrap();
        store.save("https://example.com", "bob", "=cmd").unwrap();
        assert!(store.edit(&a.id, Some("bob"), None).is_err());
        let edited = store.edit(&a.id, Some("ada2"), Some("pw2")).unwrap();
        assert!(edited.password.is_empty());
        assert_eq!(store.get(&a.id).unwrap().password, "pw2");
        let csv = store.export_csv().unwrap();
        assert!(csv.starts_with("name,url,username,password,note\n"));
        assert!(csv.contains("\"example.com\",\"https://example.com\",\"ada2\",\"pw2\",\"\""));
        assert!(csv.contains("\"'=cmd\""));
        store.delete(&a.id).unwrap();
        assert!(store.get(&a.id).is_err());
        assert!(store.delete(&a.id).is_err());
        store.set_never("https://never.example", true).unwrap();
        assert!(store.never().contains("https://never.example"));
        assert!(store.set_never("javascript:x", true).is_err());
        store.set_never("https://never.example", false).unwrap();
        assert!(store.never().is_empty());
        assert!(store.save("file:///x", "a", "b").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn chrome_import_reads_a_synthetic_profile_without_changing_it() {
        let dir = temp();
        let profile = dir.join("Profile 1");
        std::fs::create_dir_all(&profile).unwrap();
        let secret = b"synthetic-safe-storage";
        let key = chrome::derive_key(secret).unwrap();
        let hex = |b: Vec<u8>| b.iter().map(|x| format!("{x:02X}")).collect::<String>();
        let sql = format!(
            "CREATE TABLE logins(origin_url TEXT, signon_realm TEXT, username_value TEXT, password_value BLOB, blacklisted_by_user INTEGER);
             INSERT INTO logins VALUES ('https://example.com/login','https://example.com/','ada',X'{}',0);
             INSERT INTO logins VALUES ('http://127.0.0.1:7831/login','http://127.0.0.1:7831/','bob',X'{}',0);
             INSERT INTO logins VALUES ('https://blocked.example/','https://blocked.example/','',X'',1);
             INSERT INTO logins VALUES ('android://hash@com.example/','android://hash@com.example/','x',X'{}',0);
             INSERT INTO logins VALUES ('https://bad.example/','https://bad.example/','x',X'763130DEADBEEF',0);",
            hex(chrome::encrypt(&key, "chrome-pass-1")),
            hex(chrome::encrypt(&key, "chrome-pass-2")),
            hex(chrome::encrypt(&key, "android")),
        );
        let db = profile.join("Login Data");
        assert!(std::process::Command::new("/usr/bin/sqlite3").arg(&db).arg(sql).status().unwrap().success());
        let before = std::fs::read(&db).unwrap();
        let data = chrome::read(&profile, secret).await.unwrap();
        assert_eq!(std::fs::read(&db).unwrap(), before);
        assert_eq!(data.logins, vec![
            ("https://example.com".into(), "ada".into(), "chrome-pass-1".into()),
            ("http://127.0.0.1:7831".into(), "bob".into(), "chrome-pass-2".into()),
        ]);
        assert_eq!(data.never, vec!["https://blocked.example".to_string()]);
        assert_eq!(data.skipped, 2);
        assert!(chrome::read(&profile, b"wrong").await.unwrap().logins.is_empty());
        let store = Store::open(&dir, true).unwrap();
        let summary = import(&store, data).unwrap();
        assert_eq!(summary["added"], 2);
        assert_eq!(summary["never"], 1);
        let again = import(&store, chrome::read(&profile, secret).await.unwrap()).unwrap();
        assert_eq!(again["unchanged"], 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Writes one item to the real login keychain with user interaction disabled, so it can
    /// never show a prompt, then deletes it. Opt in with ATRIUM_TEST_REAL_KEYCHAIN=1.
    #[test]
    fn keychain_round_trip_without_prompts() {
        if std::env::var_os("ATRIUM_TEST_REAL_KEYCHAIN").is_none() {
            return;
        }
        unsafe { keychain::SecKeychainSetUserInteractionAllowed(0) };
        let dir = temp();
        let service = format!("Atrium Passwords Test {}", uuid::Uuid::new_v4());
        let store = Store::keychain_for_test(&dir, &service);
        assert!(store.list().unwrap().is_empty());
        let (login, _) = store.save("https://example.com", "ada", "kc-secret").unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].password.is_empty());
        assert_eq!(store.get(&login.id).unwrap().password, "kc-secret");
        assert_eq!(store.save("https://example.com", "ada", "kc-secret-2").unwrap().1, Saved::Updated);
        assert_eq!(store.get(&login.id).unwrap().password, "kc-secret-2");
        store.delete(&login.id).unwrap();
        assert!(store.list().unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(feature = "chromium")]
pub use api::routes;
#[cfg(not(feature = "chromium"))]
pub fn routes() -> axum::Router<std::sync::Arc<crate::server::App>> {
    axum::Router::new()
}

/// Management API. Routes sit behind the same Host/Origin checks and per-launch token as the rest.
#[cfg(feature = "chromium")]
mod api {
    use super::*;
    use crate::{app::Op, server::App};
    use axum::{extract::State, routing::{get, post}, Json, Router};
    use std::sync::Arc;

    pub fn routes() -> Router<Arc<App>> {
        Router::new()
            .route("/api/passwords", get(list))
            .route("/api/passwords/reveal", post(reveal))
            .route("/api/passwords/edit", post(edit))
            .route("/api/passwords/delete", post(delete))
            .route("/api/passwords/never", post(never))
            .route("/api/passwords/export", post(export))
            .route("/api/passwords/import-chrome", post(import_chrome))
            .route("/api/passwords/offers", get(offers))
            .route("/api/debug/cdp", post(cdp))
    }

    fn reply(result: Result<Value>) -> Json<Value> {
        Json(result.unwrap_or_else(|e| json!({ "error": e.to_string() })))
    }
    async fn blocking(f: impl FnOnce(&Store) -> Result<Value> + Send + 'static) -> Json<Value> {
        reply(tokio::task::spawn_blocking(move || with(f)).await.unwrap_or_else(|e| Err(e.into())))
    }

    async fn list() -> Json<Value> {
        blocking(|s| Ok(json!({ "store": s.kind(), "logins": s.list()?, "never": s.never() }))).await
    }

    #[derive(Deserialize)]
    struct Id {
        id: String,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
    }
    async fn reveal(Json(req): Json<Id>) -> Json<Value> {
        blocking(move |s| Ok(json!({ "password": s.get(&req.id)?.password }))).await
    }
    async fn edit(Json(req): Json<Id>) -> Json<Value> {
        blocking(move |s| Ok(json!({ "login": s.edit(&req.id, req.username.as_deref(), req.password.as_deref())? }))).await
    }
    async fn delete(Json(req): Json<Id>) -> Json<Value> {
        blocking(move |s| s.delete(&req.id).map(|_| json!({ "ok": true }))).await
    }

    #[derive(Deserialize)]
    struct Never {
        origin: String,
        never: bool,
    }
    async fn never(Json(req): Json<Never>) -> Json<Value> {
        blocking(move |s| s.set_never(&req.origin, req.never).map(|_| json!({ "ok": true }))).await
    }
    async fn export() -> Json<Value> {
        blocking(|s| Ok(json!({ "csv": s.export_csv()? }))).await
    }

    #[derive(Deserialize)]
    struct ImportReq {
        #[serde(default)]
        profile: Option<String>,
        /// Test profiles only: a synthetic profile directory and its injected Safe Storage secret.
        #[serde(default)]
        profile_path: Option<PathBuf>,
        #[serde(default)]
        safe_storage: Option<String>,
    }
    async fn import_chrome(Json(req): Json<ImportReq>) -> Json<Value> {
        reply(
            async {
                let (path, secret) = match (req.profile, req.profile_path, req.safe_storage) {
                    (None, Some(path), Some(secret)) => {
                        anyhow::ensure!(is_mock(), "Synthetic Chrome profiles can only be imported into test profiles");
                        (path, secret.into_bytes())
                    }
                    (Some(id), None, None) => {
                        let path = crate::migration::profile_at(&crate::migration::chrome_root()?, &id)?;
                        let secret = tokio::task::spawn_blocking(chrome::safe_storage_from_keychain).await??;
                        (path, secret)
                    }
                    _ => bail!("Choose a Chrome profile to import"),
                };
                let data = chrome::read(&path, &secret).await?;
                tokio::task::spawn_blocking(move || with(|s| import(s, data))).await?
            }
            .await,
        )
    }

    async fn offers(State(app): State<Arc<App>>) -> Json<Value> {
        reply(app.agent.browser.op(Op::PasswordOffers).await)
    }

    /// For automated WebAuthn tests (virtual authenticators) and shell screenshots; nothing else.
    async fn cdp(State(app): State<Arc<App>>, Json(req): Json<Value>) -> Json<Value> {
        let method = req["method"].as_str().unwrap_or("").to_string();
        if !(method.starts_with("WebAuthn.") || method == "Page.captureScreenshot") {
            return Json(json!({ "error": "This DevTools method is not available" }));
        }
        let tab = if req["tab"] == "shell" { Some(crate::app::SHELL) } else { req["tab"].as_u64() };
        let params = if req["params"].is_object() { req["params"].clone() } else { json!({}) };
        reply(match app.agent.browser.op(|reply| Op::Cdp { tab, method, params, reply }).await {
            Ok(r) => r.map(|data| json!({ "result": data })).map_err(|e| anyhow!(e)),
            Err(e) => Err(e),
        })
    }
}
