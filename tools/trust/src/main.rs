//! `godspeed-trust`: the registry maintainer's offline tool for the registry's trust anchor.
//!
//! It writes and signs the two OFFLINE documents - `root` (which keys hold each role) and `namespaces` (each
//! publisher's keys: active, retired or revoked) - in a directory that mirrors the bucket's `trust/` prefix:
//!
//!     trust/root/<n>.toml, trust/root/<n>.sig      every root version, so clients can follow rotations
//!     trust/namespaces.toml, trust/namespaces.sig
//!
//! Run it on the machine that holds the offline keys, never on the registry. The registry holds only the
//! online key, which signs `snapshot` and `timestamp`; it never sees these keys, so it cannot rewrite who may
//! sign for whom. `scripts/admin.mjs upload-trust` puts the directory in the bucket.
//!
//! Each edit bumps the document's version and deletes its now-stale signatures; sign again, with as many
//! keys as the role's threshold, possibly on different machines - `sign` keeps every signature that is still
//! valid and adds its own.

#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use godspeed_package::trust::{
    self, ended, verify_by, verify_next_root, write_namespaces, write_root, KeyStatus, NamespaceKey, Namespaces, Role,
    Root,
};
use godspeed_package::{is_timestamp, PackageName, PublicKey};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "godspeed-trust",
    version,
    about = "Offline signing of the registry's root and namespaces metadata"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Make a new offline key: into FILE (never overwrites), or with --print shown once to paste into a
    /// password manager, and written nowhere.
    Keygen {
        file: Option<PathBuf>,
        /// Show the key to copy into a password manager instead of writing a file
        #[arg(long, conflicts_with = "file")]
        print: bool,
    },
    /// Print the public key of a key: in FILE, or pasted (--stdin).
    Pubkey {
        file: Option<PathBuf>,
        /// Read the key from a hidden prompt (or piped standard input) instead of a file
        #[arg(long, conflicts_with = "file")]
        stdin: bool,
    },
    /// Write the next root version, unsigned: <dir>/root/<n>.toml.
    Root {
        #[arg(long, default_value = "trust")]
        dir: PathBuf,
        /// When it stops being valid (YYYY-MM-DDTHH:MM:SSZ). Roots are long-lived: a year or more.
        #[arg(long)]
        expires: String,
        /// A role's threshold and keys: root=2:ed25519:..,ed25519:..  Give all four roles.
        #[arg(long = "role", required = true)]
        roles: Vec<String>,
    },
    /// Add, retire or revoke a publisher's key in <dir>/namespaces.toml.
    Namespaces {
        #[arg(long, default_value = "trust")]
        dir: PathBuf,
        /// New expiry (YYYY-MM-DDTHH:MM:SSZ). Default: a year from now.
        #[arg(long)]
        expires: Option<String>,
        #[command(subcommand)]
        action: NamespaceAction,
    },
    /// Sign DOCUMENT with each --key (and --key-stdin), keeping any existing signatures that still verify.
    Sign {
        document: PathBuf,
        /// A key file
        #[arg(long = "key")]
        keys: Vec<PathBuf>,
        /// Also sign with a key pasted at a hidden prompt (or piped): it never touches the disk. To sign with
        /// several pasted keys, run `sign` once per key - signatures accumulate.
        #[arg(long)]
        key_stdin: bool,
    },
    /// Check every root in the chain and the namespaces document, as a client would.
    Verify {
        #[arg(long, default_value = "trust")]
        dir: PathBuf,
        /// Check expiry against this time instead of now.
        #[arg(long)]
        now: Option<String>,
    },
}

#[derive(Subcommand)]
enum NamespaceAction {
    /// Allow KEY to sign releases under PUBLISHER.
    Add { publisher: String, key: String },
    /// Routine rotation: KEY signs nothing new; releases published before now (or --at) stay trusted.
    Retire {
        publisher: String,
        key: String,
        #[arg(long)]
        at: Option<String>,
    },
    /// Compromise: nothing KEY ever signed is trusted any more, whenever it was published.
    Revoke { publisher: String, key: String },
}

type Result<T> = std::result::Result<T, String>;

fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    rfc3339(secs)
}

/// `YYYY-MM-DDTHH:MM:SSZ` from seconds since the Unix epoch (Howard Hinnant's civil-from-days).
fn rfc3339(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let z = days + 719_468;
    let (era, doe) = (z / 146_097, z % 146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn a_year_from_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    rfc3339(secs + 365 * 86_400)
}

fn timestamp(s: &str, what: &str) -> Result<String> {
    if is_timestamp(s) {
        Ok(s.to_string())
    } else {
        Err(format!("{what} \"{s}\" is not YYYY-MM-DDTHH:MM:SSZ"))
    }
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn sig_path(document: &Path) -> PathBuf {
    document.with_extension("sig")
}

// ---- keys --------------------------------------------------------------------------------------------

fn decode_seed(hex: &str) -> Option<[u8; 32]> {
    let b = hex.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in b.as_chunks::<2>().0.iter().enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// Read a key from text: a whole key file (the format `godspeed key generate` also writes, so either tool's
/// keys work), just its `secret = "..."` line, or the 64 hex digits alone - whatever was pasted. `what` names
/// where it came from, for errors.
fn parse_key(text: &str, what: &str) -> Result<[u8; 32]> {
    let field = |name: &str| {
        text.lines().find_map(|l| {
            l.trim()
                .strip_prefix(name)
                .and_then(|r| r.strip_prefix(" = \""))
                .and_then(|r| r.strip_suffix('"'))
        })
    };
    let seed = field("secret")
        .or_else(|| Some(text.trim()))
        .and_then(decode_seed)
        .ok_or(format!(
            "{what} holds no valid key (expected the key text, or its 64 hex digits)"
        ))?;
    let public = godspeed_package::ManifestSignature::public_key(&seed).to_string();
    if let Some(stated) = field("public") {
        if stated != public {
            return Err(format!("{what}: the public key does not match the secret one"));
        }
    }
    Ok(seed)
}

fn load_key(path: &Path) -> Result<[u8; 32]> {
    parse_key(&read(path)?, &path.display().to_string())
}

/// A key pasted at a hidden prompt, or piped in. It is held in memory only.
fn stdin_key() -> Result<[u8; 32]> {
    use std::io::{IsTerminal, Read};
    let text = if std::io::stdin().is_terminal() {
        rpassword::prompt_password("Paste the key's secret (64 hex digits; input hidden), then Enter: ")
            .map_err(|e| format!("could not read the key: {e}"))?
    } else {
        let mut t = String::new();
        std::io::stdin()
            .read_to_string(&mut t)
            .map_err(|e| format!("could not read the key: {e}"))?;
        t
    };
    let seed = parse_key(&text, "the pasted key")?;
    eprintln!("Using {}", godspeed_package::ManifestSignature::public_key(&seed));
    Ok(seed)
}

fn keygen(file: Option<&Path>, print: bool) -> Result<()> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| format!("the random number source failed: {e}"))?;
    let public = godspeed_package::ManifestSignature::public_key(&seed);
    let secret: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    let text = format!(
        "# A Godspeed registry OFFLINE key. Anyone holding this file can sign for the role it is listed under.\n\
         # Keep it off the registry and off any networked machine you can avoid; back it up.\n\
         format = 1\npublic = \"{public}\"\nsecret = \"{secret}\"\n"
    );
    let file = match (file, print) {
        (_, true) => {
            println!("{text}");
            eprintln!("Copy ALL of the above into a password-manager secure note now. It was written nowhere: this");
            eprintln!("is the only copy. Then clear the terminal (`cls` on Windows, `clear` elsewhere).");
            return Ok(());
        }
        (Some(f), false) => f,
        (None, false) => return Err("give a FILE to write the key to, or --print".into()),
    };
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    use std::io::Write;
    open.open(file)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| format!("{}: {e} (an existing key is never overwritten)", file.display()))?;
    println!("{public}");
    eprintln!("written to {}", file.display());
    Ok(())
}

// ---- root --------------------------------------------------------------------------------------------

fn root_versions(dir: &Path) -> u64 {
    let mut n = 0;
    while dir.join("root").join(format!("{}.toml", n + 1)).exists() {
        n += 1;
    }
    n
}

fn parse_role(spec: &str) -> Result<(Role, u32, Vec<PublicKey>)> {
    let (name, rest) = spec
        .split_once('=')
        .ok_or(format!("--role {spec}: expected name=threshold:key,key"))?;
    let role = Role::from_name(name).ok_or(format!("--role {spec}: no role \"{name}\""))?;
    let (threshold, keys) = rest
        .split_once(':')
        .ok_or(format!("--role {spec}: expected name=threshold:key,key"))?;
    let threshold: u32 = threshold
        .parse()
        .map_err(|_| format!("--role {spec}: threshold is not a number"))?;
    // Keys contain ':' themselves (ed25519:...), so split on commas only.
    let keys: Vec<PublicKey> = keys
        .split(',')
        .map(|k| {
            k.parse()
                .map_err(|_| format!("--role {spec}: \"{k}\" is not ed25519:<64 hex digits>"))
        })
        .collect::<Result<_>>()?;
    if threshold == 0 || threshold as usize > keys.len() {
        return Err(format!(
            "--role {spec}: the threshold must be between 1 and the number of keys"
        ));
    }
    Ok((role, threshold, keys))
}

fn new_root(dir: &Path, expires: &str, specs: &[String]) -> Result<()> {
    let expires = timestamp(expires, "--expires")?;
    let roles: Vec<_> = specs.iter().map(|s| parse_role(s)).collect::<Result<_>>()?;
    for role in Role::ALL {
        if roles.iter().filter(|(r, ..)| *r == role).count() != 1 {
            return Err(format!("give --role {role}=... exactly once"));
        }
    }
    let version = root_versions(dir) + 1;
    let text = write_root(version, &expires, &roles);
    Root::parse(&text).map_err(|e| format!("the root this would write is invalid: {e}"))?;
    let path = dir.join("root").join(format!("{version}.toml"));
    write(&path, &text)?;
    println!("Wrote root version {version}: {}", path.display());
    if version > 1 {
        println!(
            "Sign it with root keys of version {} (enough for ITS threshold) and of this version.",
            version - 1
        );
    } else {
        println!("Sign it with enough of its root keys to meet the root threshold.");
    }
    Ok(())
}

// ---- namespaces --------------------------------------------------------------------------------------

fn edit_namespaces(dir: &Path, expires: Option<&str>, action: &NamespaceAction) -> Result<()> {
    let path = dir.join("namespaces.toml");
    let (mut version, mut keys) = (0u64, Vec::<NamespaceKey>::new());
    if path.exists() {
        let text = read(&path)?;
        let ns = Namespaces::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        version = ns.header.version;
        for (publisher, status, key) in ns.entries() {
            let ended = match status {
                KeyStatus::Active => None,
                KeyStatus::Retired(at) => ended("retired", at),
                KeyStatus::Revoked(at) => ended("revoked", at),
            };
            keys.push(NamespaceKey {
                publisher: publisher.to_string(),
                key,
                ended,
            });
        }
    }
    let (publisher, key) = match action {
        NamespaceAction::Add { publisher, key }
        | NamespaceAction::Retire { publisher, key, .. }
        | NamespaceAction::Revoke { publisher, key } => (publisher, key),
    };
    if !PackageName::is_valid_part(publisher) {
        return Err(format!("\"{publisher}\" is not a publisher name"));
    }
    let key: PublicKey = key
        .parse()
        .map_err(|_| format!("\"{key}\" is not ed25519:<64 hex digits>"))?;
    let existing = keys.iter_mut().find(|k| k.publisher == *publisher && k.key == key);
    let what = match (action, existing) {
        (NamespaceAction::Add { .. }, None) => {
            keys.push(NamespaceKey {
                publisher: publisher.clone(),
                key,
                ended: None,
            });
            format!("{key} may now sign releases under {publisher}/")
        }
        (NamespaceAction::Add { .. }, Some(_)) => {
            return Err(format!(
                "{key} is already listed for {publisher}/; a key is never re-added"
            ))
        }
        (NamespaceAction::Retire { at, .. }, Some(k)) if k.ended.is_none() => {
            let at = match at {
                Some(at) => timestamp(at, "--at")?,
                None => now(),
            };
            k.ended = ended("retired", &at);
            format!("{key} is retired for {publisher}/ from {at}: releases published before then stay trusted")
        }
        (NamespaceAction::Retire { .. }, Some(_)) => return Err(format!("{key} is not active for {publisher}/")),
        (NamespaceAction::Revoke { .. }, Some(k)) if !matches!(k.ended, Some(("revoked", _))) => {
            let at = now();
            k.ended = ended("revoked", &at);
            format!("{key} is revoked for {publisher}/: nothing it ever signed is trusted")
        }
        (NamespaceAction::Revoke { .. }, Some(_)) => return Err(format!("{key} is already revoked for {publisher}/")),
        (_, None) => return Err(format!("{key} is not listed for {publisher}/")),
    };
    let expires = match expires {
        Some(e) => timestamp(e, "--expires")?,
        None => a_year_from_now(),
    };
    let text = write_namespaces(version + 1, &expires, &keys);
    Namespaces::parse(&text).map_err(|e| format!("the document this would write is invalid: {e}"))?;
    write(&path, &text)?;
    let _ = std::fs::remove_file(sig_path(&path)); // its signatures covered the old text
    println!("{what}.");
    println!(
        "namespaces is now version {}, unsigned: sign it with enough namespaces keys, then upload.",
        version + 1
    );
    Ok(())
}

// ---- signing and verifying ---------------------------------------------------------------------------

fn sign(document: &Path, key_files: &[PathBuf], key_stdin: bool) -> Result<()> {
    if key_files.is_empty() && !key_stdin {
        return Err("give --key <file> or --key-stdin".into());
    }
    let text = read(document)?;
    let role = text
        .lines()
        .nth(1)
        .and_then(|l| l.strip_prefix("type = \"")?.strip_suffix('"'))
        .and_then(Role::from_name)
        .ok_or(format!("{} is not trust metadata (no type line)", document.display()))?;
    if role == Role::Snapshot || role == Role::Timestamp {
        return Err(format!("{role} is signed by the registry's online key, not offline"));
    }
    // Keep every existing signature that is still valid for exactly this text.
    let mut sigs = Vec::new();
    if let Ok(old) = std::fs::read_to_string(sig_path(document)) {
        for line in old.lines().skip(1) {
            let one = format!("format = 1\n{line}\n");
            if trust::verify_signatures(role, text.as_bytes(), &one, 1, |_| true).is_ok() {
                if let Some((k, s)) = line
                    .strip_prefix("signature = \"")
                    .and_then(|l| l.strip_suffix('"'))
                    .and_then(|l| l.split_once(' '))
                {
                    if let (Ok(k), Ok(s)) = (k.parse(), s.parse()) {
                        sigs.push((k, s));
                    }
                }
            }
        }
    }
    for file in key_files {
        let seed = load_key(file)?;
        sigs.push(trust::sign_metadata(role, text.as_bytes(), &seed));
    }
    if key_stdin {
        let seed = stdin_key()?;
        sigs.push(trust::sign_metadata(role, text.as_bytes(), &seed));
    }
    let out = trust::write_signatures(&sigs);
    write(&sig_path(document), &out)?;
    println!(
        "{} now has {} signature(s).",
        document.display(),
        out.lines().count() - 1
    );
    Ok(())
}

fn verify(dir: &Path, now: Option<&str>) -> Result<()> {
    let now = match now {
        Some(n) => timestamp(n, "--now")?,
        None => self::now(),
    };
    let latest = root_versions(dir);
    if latest == 0 {
        return Err(format!("no root in {}/root/", dir.display()));
    }
    let mut texts = Vec::new();
    for v in 1..=latest {
        let p = dir.join("root").join(format!("{v}.toml"));
        texts.push((
            read(&p)?,
            read(&sig_path(&p)).map_err(|_| format!("root {v} is not signed"))?,
        ));
    }
    let (t1, s1) = &texts[0];
    let mut root = Root::parse(t1).map_err(|e| format!("root 1: {e}"))?;
    verify_by(&root, Role::Root, t1.as_bytes(), s1).map_err(|e| format!("root 1: {e}"))?;
    println!("root 1: signed by its root keys");
    for (i, (t, s)) in texts.iter().enumerate().skip(1) {
        root = verify_next_root(&root, t, s).map_err(|e| format!("root {}: {e}", i + 1))?;
        println!(
            "root {}: signed by the root keys of version {} and of its own",
            i + 1,
            i
        );
    }
    root.header
        .check_fresh(&now)
        .map_err(|e| format!("root {latest}: {e}"))?;
    for role in Role::ALL {
        println!("  {role}: threshold {}", root.threshold(role));
    }

    let path = dir.join("namespaces.toml");
    if path.exists() {
        let text = read(&path)?;
        let sigs = read(&sig_path(&path)).map_err(|_| "namespaces is not signed".to_string())?;
        let ns = Namespaces::parse(&text).map_err(|e| format!("namespaces: {e}"))?;
        verify_by(&root, Role::Namespaces, text.as_bytes(), &sigs).map_err(|e| format!("namespaces: {e}"))?;
        ns.header.check_fresh(&now).map_err(|e| format!("namespaces: {e}"))?;
        println!(
            "namespaces {}: signed by the namespaces keys of root {latest}; expires {}",
            ns.header.version, ns.header.expires
        );
        for (publisher, status, key) in ns.entries() {
            let s = match status {
                KeyStatus::Active => "active".to_string(),
                KeyStatus::Retired(at) => format!("retired {at}"),
                KeyStatus::Revoked(at) => format!("revoked {at}"),
            };
            println!("  {publisher}/  {key}  {s}");
        }
    } else {
        println!("no namespaces document yet: no publisher can sign releases");
    }
    println!("OK: ready to upload.");
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Keygen { file, print } => keygen(file.as_deref(), *print),
        Command::Pubkey { file, stdin } => {
            let seed = match (file, stdin) {
                (Some(f), false) => load_key(f),
                (None, true) => stdin_key(),
                _ => Err("give a key FILE, or --stdin".into()),
            };
            seed.map(|seed| println!("{}", godspeed_package::ManifestSignature::public_key(&seed)))
        }
        Command::Root { dir, expires, roles } => new_root(dir, expires, roles),
        Command::Namespaces { dir, expires, action } => edit_namespaces(dir, expires.as_deref(), action),
        Command::Sign {
            document,
            keys,
            key_stdin,
        } => sign(document, keys, *key_stdin),
        Command::Verify { dir, now } => verify(dir, now.as_deref()),
    };
    if let Err(e) = result {
        eprintln!("godspeed-trust: error: {e}");
        std::process::exit(1);
    }
}
