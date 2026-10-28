//! Secret file names and key files, by name and content: what `install`
//! never copies into a skill mirror or writes over. The launcher itself
//! scans nothing in 0.7.0: denying agents these files is a sandbox's job.

use std::fs;
use std::path::Path;

/// Code whose name merely holds `.env.` (`process.env.d.ts`): not a .env
/// file (orchestrator, 2026-10-21). A short allowlist, so anything new
/// counts as a secret. Codex can't honour it: its glob rules only deny, and
/// a deny glob beats an exact path, so its agents can't read these either.
pub const ENV_CODE_SUFFIXES: &[&str] = &[".env.d.ts", ".env.ts", ".env.js", ".env.mjs", ".env.cjs"];

/// Credential dotfiles, and their backups (`.npmrc.bak`, `.netrc.orig`).
const SECRET_DOTFILES: &[&str] = &[
    ".npmrc",
    ".pypirc",
    ".netrc",
    ".pgpass",
    ".git-credentials",
    ".dev.vars",
];

/// Secret files by their whole name.
const SECRET_NAMES: &[&str] = &[
    "credentials.json",
    "secrets.yaml",
    "secrets.yml",
    "terraform.tfvars",
    "terraform.tfvars.json",
];

/// Key stores and auto-loaded Terraform variables, by the end of the name.
const SECRET_SUFFIXES: &[&str] = &[
    ".p12",
    ".pfx",
    ".jks",
    ".keystore",
    ".ppk",
    ".auto.tfvars",
    ".auto.tfvars.json",
];

/// Credential files by their folder and name, at any depth.
const SECRET_IN_FOLDERS: &[&str] = &[
    ".aws/credentials",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".docker/config.json",
];

/// SSH private keys, by the start of the name.
const SSH_KEYS: &[&str] = &["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];

/// After an SSH key's name (`id_rsa…`), what makes it something else: the
/// public key, or code named for it (`id_rsa_parser.rs`). A short
/// allowlist, as for .env files.
const KEY_CODE_SUFFIXES: &[&str] = &[".pub", ".rs", ".go", ".py", ".js", ".ts", ".md"];

/// Key files by the end of the name, which certificates and CA bundles
/// share (`/etc/ssl/cert.pem`, certifi's `cacert.pem`, TLS test fixtures):
/// secrets only when they hold a private key ([`is_secret_at`]), never by
/// name alone (review of e4f833e). Also DER and PKCS#8 names (`.der`,
/// `.pk8`, Apple's `AuthKey_*.p8`), which a PICO-8 cart (`game.p8`) or a
/// certificate shares (review of 565f2a2).
const KEY_SUFFIXES: &[&str] = &[".pem", ".key", ".der", ".pk8", ".p8"];

/// How much of a key file is scanned for a private key: past any CA bundle
/// or Keynote deck worth checking; a longer file without one found is
/// judged a secret (review of 565f2a2).
const KEY_SCAN_LIMIT: u64 = 16 << 20;

/// Whether the file at `rel` under `root` is a secret the launcher never
/// commits and no agent reads: one [`is_secret_file`] names, or a `.pem`,
/// `.key`, `.der`, `.pk8` or `.p8` file holding a private key (`BEGIN PRIVATE KEY`, `BEGIN RSA
/// PRIVATE KEY`, `BEGIN OPENSSH PRIVATE KEY`, `BEGIN ENCRYPTED PRIVATE
/// KEY`…, or a DER one: [`der_private_key`]). A missing one (deleted)
/// isn't; one that can't be read, isn't a regular file, or is longer than
/// [`KEY_SCAN_LIMIT`] is (review of e4f833e and 7e6f4b7). The read never
/// blocks (a FIFO).
pub fn is_secret_at(root: &Path, rel: &str) -> bool {
    if is_secret_file(rel) {
        return true;
    }
    let lower = rel.to_ascii_lowercase();
    if !KEY_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        return false;
    }
    match key_file(&root.join(rel)) {
        Ok(secret) => secret,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Whether the file at `path` holds a private key ([`holds_private_key`]),
/// opened without blocking (a FIFO) and only if it's a regular file.
fn key_file(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    holds_private_key(file)
}

/// Whether what `r` reads holds a private key: a DER one from its first
/// bytes ([`der_private_key`], [`der_encrypted_key`]), or `PRIVATE KEY`
/// anywhere in the first
/// [`KEY_SCAN_LIMIT`] (`BEGIN PRIVATE KEY`, `BEGIN RSA/EC/OPENSSH/ENCRYPTED
/// PRIVATE KEY`), read in chunks. Longer than that without one: judged a
/// secret.
pub(crate) fn holds_private_key(r: impl std::io::Read) -> std::io::Result<bool> {
    use std::io::Read as _;
    const NEEDLE: &[u8] = b"PRIVATE KEY";
    let mut r = r.take(KEY_SCAN_LIMIT + 1);
    let mut chunk = vec![0; 64 << 10];
    let mut head: Vec<u8> = Vec::new();
    let mut window: Vec<u8> = Vec::new();
    let mut total = 0u64;
    loop {
        let n = match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        total += n as u64;
        if head.len() < 64 {
            head.extend(&chunk[..n.min(64 - head.len())]);
            if head.len() >= 16 && (der_private_key(&head) || der_encrypted_key(&head)) {
                return Ok(true);
            }
        }
        window.extend(&chunk[..n]);
        if window.windows(NEEDLE.len()).any(|w| w == NEEDLE) {
            return Ok(true);
        }
        let keep = window.len().saturating_sub(NEEDLE.len() - 1);
        window.drain(..keep);
    }
    Ok(der_private_key(&head) || der_encrypted_key(&head) || total > KEY_SCAN_LIMIT)
}

/// Whether bytes open like an encrypted PKCS#8 key in DER: a SEQUENCE
/// whose first element, the encryption AlgorithmIdentifier, names a
/// password-based scheme: PBES1 or PBES2 (1.2.840.113549.1.5.x) or PKCS#12
/// PBE (1.2.840.113549.1.12.1.x). A public key (SPKI) names its key's
/// algorithm there instead (review of 827a389).
fn der_encrypted_key(b: &[u8]) -> bool {
    const PKCS5: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05];
    const PKCS12_PBE: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x0c, 0x01];
    let Some((0x30, h)) = der_header(b) else {
        return false;
    };
    let Some(alg) = b.get(h..) else {
        return false;
    };
    let Some((0x30, h)) = der_header(alg) else {
        return false;
    };
    match alg.get(h..) {
        Some([0x06, _, oid @ ..]) => oid.starts_with(PKCS5) || oid.starts_with(PKCS12_PBE),
        _ => false,
    }
}

/// A DER element's tag and the length of its header, for the definite
/// lengths a key uses.
fn der_header(b: &[u8]) -> Option<(u8, usize)> {
    match b {
        [tag, n, ..] if *n < 0x80 => Some((*tag, 2)),
        [tag, n @ 0x81..=0x84, ..] => Some((*tag, 2 + usize::from(n - 0x80))),
        _ => None,
    }
}

/// Whether bytes open like a DER private key: a SEQUENCE holding the
/// INTEGER version, then what each format puts next (review of 565f2a2):
/// - version 0 or 1, an INTEGER: PKCS#1 RSA (1 for multi-prime), DSA;
/// - version 0 or 1, an AlgorithmIdentifier SEQUENCE: PKCS#8;
/// - version 1, an OCTET STRING: SEC 1 EC.
///
/// Explicit EC parameters (version 1, then a field-type OID) aren't; a
/// certificate, CRL or request opens with a SEQUENCE instead. An encrypted
/// PKCS#8 key does too, so it isn't found.
fn der_private_key(b: &[u8]) -> bool {
    let Some((0x30, h)) = der_header(b) else {
        return false;
    };
    let Some([0x02, 0x01, version, rest @ ..]) = b.get(h..) else {
        return false;
    };
    match (version, rest.first()) {
        (0x00 | 0x01, Some(0x02)) | (0x01, Some(0x04)) => true,
        (0x00 | 0x01, Some(0x30)) => {
            // The AlgorithmIdentifier's OID; prime-field and
            // characteristic-two-field (1.2.840.10045.1.x) are EC
            // parameters' field types.
            const FIELD_TYPE: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x01];
            match der_header(rest).and_then(|(_, h)| rest.get(h..)) {
                Some([0x06, _, oid @ ..]) => !oid.starts_with(FIELD_TYPE),
                _ => false,
            }
        }
        _ => false,
    }
}

/// Whether a path names a file the launcher never commits, in any case: a
/// .env file; a credential dotfile or a backup of one (`.npmrc`, `.pypirc`,
/// `.netrc`, `.pgpass`, `.git-credentials`, `.dev.vars`); `credentials.json`,
/// `secrets.yaml`, `terraform.tfvars`; a key store (`*.p12`, `*.pfx`,
/// `*.jks`, `*.keystore`, `*.ppk`) or `*.auto.tfvars`; `.aws/credentials`,
/// `.cargo/credentials`, `.docker/config.json`; or an SSH private key
/// (audit, 2026-10-22). By name alone, so the [`KEY_SUFFIXES`] names aren't here:
/// [`is_secret_at`] judges those by content.
pub fn is_secret_file(path: &str) -> bool {
    if is_env_file(path) {
        return true;
    }
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let dotfile = SECRET_DOTFILES.iter().any(|d| {
        name.strip_prefix(d)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    });
    let in_folder = SECRET_IN_FOLDERS
        .iter()
        .any(|p| lower == *p || lower.ends_with(&format!("/{p}")));
    if dotfile
        || in_folder
        || SECRET_NAMES.contains(&name)
        || SECRET_SUFFIXES.iter().any(|s| name.ends_with(s))
    {
        return true;
    }
    SSH_KEYS.iter().any(|k| {
        name.strip_prefix(k)
            .is_some_and(|rest| !KEY_CODE_SUFFIXES.iter().any(|s| rest.ends_with(s)))
    })
}

/// Whether a path names a .env file: `.env`, `*.env`, `*.env.*` (`.env.local`,
/// `prod.env.local`, `app.env.example`), `.env-*`, `.env_*`, a file in a
/// `.env` folder, `.envrc` or `.envrc.*`, in any case. Code with a name
/// before an allowlisted suffix (`process.env.d.ts`) isn't; a bare
/// `.env.ts` is.
pub fn is_env_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let mut segments = lower.rsplit('/');
    let name = segments.next().unwrap_or_default();
    if segments.any(|s| s == ".env") {
        return true;
    }
    if name == ".envrc"
        || name.starts_with(".envrc.")
        || name.starts_with(".env-")
        || name.starts_with(".env_")
        || name.ends_with(".env")
    {
        return true;
    }
    if !name.contains(".env.") {
        return false;
    }
    let code = !name.starts_with(".env.") && ENV_CODE_SUFFIXES.iter().any(|s| name.ends_with(s));
    !code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Every kind of secret file install never copies.
    const SECRETS: &[&str] = &[
        ".env",
        "sub/.env",
        "Prod.ENV",
        ".env.local",
        "prod.env.local",
        "app.env.example",
        "docker.env.production",
        ".envrc",
        ".envrc.local",
        "x.env.json",
        ".env.ts",
        ".env.d.ts",
        ".env-local",
        "web/.ENV_prod",
        "cfg/.env/secrets",
        "app/.env/prod/db",
        "store.p12",
        "a.pfx",
        "release.jks",
        "android/app.keystore",
        "putty.PPK",
        "id_rsa",
        ".ssh/id_ed25519",
        "id_ecdsa.bak",
        "id_dsa",
        ".npmrc",
        ".npmrc.bak",
        "pkg/.pypirc",
        ".netrc",
        ".netrc.orig",
        ".pgpass",
        ".git-credentials",
        "worker/.dev.vars",
        ".dev.vars.production",
        "credentials.json",
        "config/Secrets.yaml",
        "secrets.yml",
        "infra/terraform.tfvars",
        "terraform.tfvars.json",
        "prod.auto.tfvars",
        "x.auto.tfvars.json",
        ".aws/credentials",
        "home/.aws/credentials",
        "sub/.aws/credentials",
        ".cargo/credentials",
        "home/.cargo/credentials.toml",
        ".docker/config.json",
    ];

    /// Key files the content check must find: one per private key header.
    const KEY_FILES: &[(&str, &str)] = &[
        ("server.pem", "-----BEGIN PRIVATE KEY-----\nMIIE\n"),
        ("certs/TLS.KEY", "-----BEGIN RSA PRIVATE KEY-----\nMIIE\n"),
        ("ec.key", "-----BEGIN EC PRIVATE KEY-----\nMHc\n"),
        (
            "deploy/id.pem",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Bl\n",
        ),
        ("enc.pem", "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIF\n"),
        (
            "chain.pem",
            "-----BEGIN CERTIFICATE-----\nMIID\n-----END CERTIFICATE-----\n\
             -----BEGIN PRIVATE KEY-----\nMIIE\n",
        ),
    ];

    #[test]
    fn env_files_are_denied_broadly_and_only_code_suffixes_pass() {
        for secret in [
            ".env",
            "sub/.env",
            "Prod.ENV",
            ".env.local",
            "prod.env.local",
            "app.env.example",
            "docker.env.production",
            ".envrc",
            ".envrc.local",
            "x.env.json",
            ".env.ts",
            ".env.d.ts",
            ".env-local",
            ".env_prod",
            ".env/secrets",
            "a/.ENV/b/c",
        ] {
            assert!(is_env_file(secret), "{secret} must count as a .env file");
        }
        for code in [
            "src/process.env.d.ts",
            "config.env.ts",
            "load.env.js",
            "load.env.mjs",
            "load.env.cjs",
            "environment.ts",
            "src/envoy.rs",
            "README.md",
        ] {
            assert!(!is_env_file(code), "{code} is code");
        }
    }

    #[test]
    fn key_and_credential_files_are_secrets_and_their_code_isnt() {
        for secret in SECRETS {
            assert!(is_secret_file(secret), "{secret} must count as a secret");
        }
        for code in [
            "id_rsa.pub",
            "id_ed25519.pub",
            "src/id_rsa_parser.rs",
            "docs/id_rsa.md",
            "keyboard.rs",
            "aws/credentials.rs",
            "src/credentials.rs",
            "src/secrets.rs",
            "docs/secrets.md",
            "docker/config.json",
            "cargo/credentials",
            "variables.tf",
            "npmrc.md",
            "README.md",
            // By content only (review of e4f833e).
            "server.pem",
            "certs/TLS.KEY",
        ] {
            assert!(!is_secret_file(code), "{code} isn't a secret");
        }
    }

    #[test]
    fn a_key_file_is_a_secret_only_when_it_holds_a_private_key() {
        let root = std::env::temp_dir().join(format!("mdkg-run-keys-{}", uuid::Uuid::now_v7()));
        for (name, text) in KEY_FILES {
            let path = root.join(name);
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            fs::write(&path, text).expect("w");
            assert!(is_secret_at(&root, name), "{name} holds a private key");
        }
        // A CA bundle the size of certifi's, and a certificate alone.
        let cert = "-----BEGIN CERTIFICATE-----\nMIIDdzCCAl+gAwIBAgIE\n-----END CERTIFICATE-----\n";
        fs::write(root.join("cacert.pem"), cert.repeat(300 << 10 >> 6)).expect("w");
        fs::write(root.join("cert.pem"), cert).expect("w");
        fs::write(root.join("slides.key"), "a Keynote file").expect("w");
        for name in ["cacert.pem", "cert.pem", "slides.key"] {
            assert!(!is_secret_at(&root, name), "{name} holds no key");
        }
        // A deleted key file is no secret change; an unambiguous name is
        // judged by name whether or not it's there.
        assert!(!is_secret_at(&root, "gone.pem"));
        assert!(is_secret_at(&root, "gone/.env"));
        // A FIFO is never read, and counts as a secret.
        let fifo = root.join("pipe.pem");
        let made = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
        assert!(made.success());
        assert!(is_secret_at(&root, "pipe.pem"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn der_keys_are_secrets_by_their_shape_and_der_certificates_and_ec_parameters_arent() {
        let root = std::env::temp_dir().join(format!("mdkg-run-der-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&root).expect("mkdir");
        let padded = |mut b: Vec<u8>| {
            b.resize(96, 0);
            b
        };
        // PKCS#8 (version 0, then an AlgorithmIdentifier: rsaEncryption),
        // PKCS#1 RSA (version 0, then the modulus), SEC 1 EC (version 1,
        // then an OCTET STRING).
        let rsa_oid = [
            0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01,
        ];
        let mut pkcs8 = vec![0x30, 0x82, 0x04, 0xbd, 0x02, 0x01, 0x00, 0x30, 0x0d];
        pkcs8.extend(rsa_oid);
        let pkcs1 = padded(vec![0x30, 0x81, 0x9f, 0x02, 0x01, 0x00, 0x02, 0x41]);
        let sec1 = padded(vec![0x30, 0x77, 0x02, 0x01, 0x01, 0x04, 0x20]);
        // Explicit EC parameters: version 1, then a prime-field FieldID.
        let params = padded(vec![
            0x30, 0x81, 0xf7, 0x02, 0x01, 0x01, 0x30, 0x2c, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce,
            0x3d, 0x01, 0x01,
        ]);
        let cert = padded(vec![
            0x30, 0x82, 0x03, 0x77, 0x30, 0x82, 0x02, 0x5f, 0xa0, 0x03,
        ]);
        // Every content-judged name (review of 565f2a2).
        for (name, bytes) in [
            ("k8.key", padded(pkcs8.clone())),
            ("release.pk8", padded(pkcs8.clone())),
            ("key.der", padded(pkcs8.clone())),
            ("AuthKey_ABC123.p8", padded(pkcs8)),
            ("rsa.pem", pkcs1),
            ("ec.key", sec1),
        ] {
            fs::write(root.join(name), bytes).expect("w");
            assert!(is_secret_at(&root, name), "{name} is a DER key");
        }
        // Encrypted PKCS#8 (PBES2 with AES, PKCS#12 PBE with 3DES), as
        // `openssl pkcs8 -topk8 -outform DER` writes them; a public key
        // (SPKI) names its key's algorithm instead (review of 827a389).
        let pbes2 = padded(vec![
            0x30, 0x81, 0xf4, 0x30, 0x5f, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01,
            0x05, 0x0d,
        ]);
        let pbe3des = padded(vec![
            0x30, 0x81, 0xb1, 0x30, 0x1c, 0x06, 0x0a, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01,
            0x0c, 0x01, 0x03,
        ]);
        let spki = padded(vec![
            0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01,
        ]);
        for (name, bytes) in [("enc.der", pbes2), ("enc.p8", pbe3des)] {
            fs::write(root.join(name), bytes).expect("w");
            assert!(is_secret_at(&root, name), "{name} is an encrypted key");
        }
        for (name, bytes) in [
            ("cert.der", cert),
            ("ecparams.pem", params),
            ("pub.der", spki),
        ] {
            fs::write(root.join(name), bytes).expect("w");
            assert!(!is_secret_at(&root, name), "{name} holds no key");
        }
        // A PICO-8 cart is judged by content, not its name.
        fs::write(
            root.join("game.p8"),
            "pico-8 cartridge // http://www.pico-8.com\n",
        )
        .expect("w");
        assert!(!is_secret_file("carts/game.p8"));
        assert!(!is_secret_at(&root, "game.p8"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_long_key_file_is_scanned_to_the_cap_and_judged_a_secret_past_it() {
        let root = std::env::temp_dir().join(format!("mdkg-run-long-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&root).expect("mkdir");
        // A Keynote deck (a zip) over the old 512 KiB: no key, no secret.
        let mut deck = b"PK\x03\x04".to_vec();
        deck.resize(600 << 10, 0);
        fs::write(root.join("talk.key"), &deck).expect("w");
        assert!(!is_secret_at(&root, "talk.key"));
        // A key after 1 MiB of certificates, split across read chunks.
        let mut chain = "-----BEGIN CERTIFICATE-----\n".repeat((1 << 20) / 28);
        // "PRIVATE KEY" starts 5 bytes before a 64 KiB boundary.
        let chunk = 64 << 10;
        chain.push_str(&"x".repeat((2 * chunk - 16 - chain.len() % chunk) % chunk));
        chain.push_str("-----BEGIN PRIVATE KEY-----\n");
        fs::write(root.join("chain.pem"), chain).expect("w");
        assert!(is_secret_at(&root, "chain.pem"));
        // Past the cap with none found: judged a secret.
        let file = fs::File::create(root.join("huge.pem")).expect("create");
        file.set_len(KEY_SCAN_LIMIT + 1).expect("len");
        assert!(is_secret_at(&root, "huge.pem"));
        let _ = fs::remove_dir_all(root);
    }
    /// A multi-prime (three-prime) PKCS#1 RSA key's opening, as OpenSSL
    /// writes it: version 1, then the modulus INTEGER; the rest is filler.
    fn multi_prime_rsa() -> Vec<u8> {
        let mut der = vec![
            0x30, 0x82, 0x02, 0x7e, 0x02, 0x01, 0x01, 0x02, 0x81, 0x81, 0x00,
        ];
        der.resize(200, 0xa7);
        der
    }

    #[test]
    fn a_multi_prime_rsa_key_is_a_secret() {
        assert!(der_private_key(&multi_prime_rsa()));
        let root = std::env::temp_dir().join(format!("mdkg-multiprime-{}", std::process::id()));
        fs::create_dir_all(&root).expect("mkdir");
        fs::write(root.join("server.key"), multi_prime_rsa()).expect("w");
        assert!(is_secret_at(&root, "server.key"));
        let _ = fs::remove_dir_all(root);
    }
}
