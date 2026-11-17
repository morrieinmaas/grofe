//! Installing grofe on a remote that has none, or one too old (spec
//! ssh-remote §2 "Bootstrap"): ask, learn the remote's os and arch,
//! download the release archive on this machine, check it against the
//! release's `SHA256SUMS`, unpack it and copy the binary over ssh into
//! `~/.local/share/grofe/bin/<version>/`. Never sudo, never the remote's
//! config.
//!
//! Ruling: SHA-256 is a small pure function here (FIPS 180-4, its test
//! vectors below) instead of `shasum`/`sha256sum`, which differ between
//! macOS and Linux and may be missing. Cost: about 60 lines to keep.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::connect::{ConnectError, ConnectOpts, Link, Transport};
use super::ssh::{self, SshOpts, Target};

/// Overrides the `remote-release-url` option (tests, mirrors).
pub const RELEASE_URL_ENV: &str = "GROFE_REMOTE_RELEASE_URL";

/// What to do when the remote needs grofe installed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Install {
    /// Ask on the terminal; no terminal means no.
    #[default]
    Ask,
    /// `--install`.
    Yes,
    /// `--no-install`.
    No,
    /// `--install-from <path>`: this local binary, unchecked.
    From(PathBuf),
}

/// The release target triple for `uname -sm` output (`Linux x86_64`).
pub fn release_target(uname: &str) -> Result<&'static str, String> {
    let mut words = uname.split_whitespace();
    let (os, arch) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    match (os, arch) {
        ("Linux", "x86_64" | "amd64") => Ok("x86_64-unknown-linux-musl"),
        ("Linux", "aarch64" | "arm64") => Ok("aarch64-unknown-linux-musl"),
        ("Darwin", "x86_64") => Ok("x86_64-apple-darwin"),
        ("Darwin", "arm64" | "aarch64") => Ok("aarch64-apple-darwin"),
        _ => Err(format!("no grofe release for {} {}", os, arch)),
    }
}

/// The release archive's file name, as `.github/workflows/release.yml`
/// names it.
pub fn archive_name(version: &str, target: &str) -> String {
    format!("grofe-v{version}-{target}.tar.gz")
}

/// Where a release file is: `<base>/v<version>/<file>`. The base is an
/// `http(s)://` or `file://` URL or a plain directory.
pub fn release_file_url(base: &str, version: &str, file: &str) -> String {
    format!("{}/v{version}/{file}", base.trim_end_matches('/'))
}

/// Whether `url` is `http://` to a loopback literal (`127.0.0.0/8`,
/// `[::1]`, `localhost`), the one place plain http is taken.
pub fn loopback_http(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(inner) => match inner.split_once(']') {
            Some((h, after)) if after.is_empty() || after.starts_with(':') => h,
            _ => return false,
        },
        None => authority.split(':').next().unwrap_or(""),
    };
    host == "localhost"
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Reads a release file: `curl` for `https://` (TLS 1.2 or later, and
/// https only, redirects included), else from disk. Plain `http://` is
/// refused, since the archive and its sums would both come over it
/// unchecked, except to a loopback address (the container e2e serves its
/// release dir there), and then without following redirects.
pub fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let curl: &[&str] = if url.starts_with("https://") {
        &[
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "-fsSL",
        ]
    } else if loopback_http(url) {
        // A local test or mirror server: http, no redirects (no -L).
        &["--proto", "=http", "-fsS"]
    } else if url.starts_with("http://") {
        return Err(format!(
            "refusing {}: release downloads must use https (plain http only on a loopback address)",
            ssh::sanitize(url)
        ));
    } else {
        &[]
    };
    if !curl.is_empty() {
        let out = Command::new("curl")
            .args(curl)
            .args(["--", url])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run curl: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "downloading {} failed: {}",
                ssh::sanitize(url),
                ssh::sanitize(String::from_utf8_lossy(&out.stderr).trim())
            ));
        }
        return Ok(out.stdout);
    }
    let path = url.strip_prefix("file://").unwrap_or(url);
    std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))
}

/// The SHA-256 `SHA256SUMS` lists for `file` (`<hex>  <name>`, the name
/// maybe with a leading `./` or `*`).
pub fn listed_sum<'a>(sums: &'a str, file: &str) -> Option<&'a str> {
    sums.lines().find_map(|l| {
        let (hash, name) = l.split_once(char::is_whitespace)?;
        let name = name.trim().trim_start_matches('*').trim_start_matches("./");
        (name == file).then_some(hash.trim())
    })
}

/// Checks `data` against the sum `sums` lists for `file`.
pub fn verify(data: &[u8], sums: &str, file: &str) -> Result<(), String> {
    let want = listed_sum(sums, file).ok_or_else(|| format!("SHA256SUMS does not list {file}"))?;
    let got = hex(&sha256(data));
    if !got.eq_ignore_ascii_case(want) {
        return Err(format!(
            "checksum mismatch for {file}: SHA256SUMS says {want}, the download is {got}; nothing was installed"
        ));
    }
    Ok(())
}

/// `uname -sm` on the remote.
pub fn remote_uname(target: &Target) -> Result<String, String> {
    let out = ssh::command()
        .args(ssh::argv(target, SshOpts::default(), "uname -sm"))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", ssh::program()))?;
    if !out.status.success() {
        return Err(format!(
            "uname on {} failed: {}",
            target.host_name(),
            ssh::sanitize(String::from_utf8_lossy(&out.stderr).trim())
        ));
    }
    Ok(ssh::sanitize(String::from_utf8_lossy(&out.stdout).trim()))
}

/// The question asked before an install.
pub fn question(host: &str, uname: &str, version: &str) -> String {
    format!(
        "grofe is not installed on {host} ({}). Install v{version} to ~/{}/? [y/N] ",
        uname.to_lowercase(),
        ssh::install_dir(version)
    )
}

/// Whether to install: the flags answer, else the terminal.
fn approve(install: &Install, prompt: &str) -> bool {
    match install {
        Install::Yes | Install::From(_) => true,
        Install::No => false,
        Install::Ask => {
            if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
                return false;
            }
            let mut err = io::stderr();
            let _ = err.write_all(prompt.as_bytes());
            let _ = err.flush();
            let mut answer = String::new();
            if io::stdin().lock().read_line(&mut answer).is_err() {
                return false;
            }
            matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
        }
    }
}

/// The grofe binary for `uname`, downloaded and checked, or the user's file.
fn binary(install: &Install, release_url: &str, uname: &str) -> Result<Vec<u8>, String> {
    if let Install::From(path) = install {
        return std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()));
    }
    let version = env!("CARGO_PKG_VERSION");
    let target = release_target(uname)?;
    let name = archive_name(version, target);
    let archive = fetch(&release_file_url(release_url, version, &name))?;
    let sums = fetch(&release_file_url(release_url, version, "SHA256SUMS"))?;
    verify(&archive, &String::from_utf8_lossy(&sums), &name)?;
    unpack(&archive, &format!("grofe-v{version}-{target}/grofe"))
}

/// The file at `member` in the `.tar.gz` `archive`, unpacked with `tar`.
fn unpack(archive: &[u8], member: &str) -> Result<Vec<u8>, String> {
    let dir = TempDir::new()?;
    let file = dir.0.join("grofe.tar.gz");
    std::fs::write(&file, archive).map_err(|e| format!("writing the archive: {e}"))?;
    let out = Command::new("tar")
        .arg("xzf")
        .arg(&file)
        .arg("-C")
        .arg(&dir.0)
        .arg(member)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run tar: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "unpacking the archive failed: {}",
            ssh::sanitize(String::from_utf8_lossy(&out.stderr).trim())
        ));
    }
    // The member must be a plain file: an archive whose `grofe` is a
    // symlink would make this read a file of this machine's choosing.
    let path = dir.0.join(member);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_file() => {}
        Ok(_) => return Err(format!("{member} in the archive is not a plain file")),
        Err(e) => return Err(format!("reading {member}: {e}")),
    }
    std::fs::read(&path).map_err(|e| format!("reading {member}: {e}"))
}

/// A private directory under the temp dir, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<TempDir, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir =
            std::env::temp_dir().join(format!("grofe-install-{}-{nanos}", std::process::id()));
        std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
            .create(&dir)
            .map_err(|e| format!("creating {}: {e}", dir.display()))?;
        Ok(TempDir(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Copies `binary` to the remote's versioned install directory.
fn upload(target: &Target, binary: &[u8]) -> Result<(), String> {
    let remote = ssh::install_command(
        env!("CARGO_PKG_VERSION"),
        binary.len(),
        &hex(&sha256(binary)),
    );
    let mut child = ssh::command()
        .args(ssh::argv(target, SshOpts::default(), &remote))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", ssh::program()))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(binary)
            .map_err(|e| format!("copying grofe to {}: {e}", target.host_name()))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("waiting for ssh: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "installing on {} failed: {}",
            target.host_name(),
            ssh::sanitize(String::from_utf8_lossy(&out.stderr).trim())
        ));
    }
    Ok(())
}

/// Connects to `target`, installing grofe there first when it has none
/// or an incompatible one and `install` allows it.
pub fn connect(target: &Target, install: &Install, release_url: &str) -> Result<Link, String> {
    let transport = Transport::Ssh(target.clone());
    let why = match Link::connect(&transport, ConnectOpts::default()) {
        Ok(link) => return Ok(link),
        Err(ConnectError::Failed(why)) => return Err(why),
        Err(e) => e,
    };
    let host = target.host_name();
    let uname = remote_uname(target)?;
    let version = env!("CARGO_PKG_VERSION");
    let mut prompt = question(host, &uname, version);
    if let ConnectError::Incompatible(detail) = &why {
        prompt = format!("{detail}\n{prompt}");
    }
    if !approve(install, &prompt) {
        return Err(format!(
            "{why} ({host}); run with --install to install v{version} there"
        ));
    }
    if !matches!(install, Install::From(_)) && release_url != DEFAULT_RELEASE_URL {
        eprintln!(
            "grofe: downloading from {} (not the default release URL)",
            ssh::sanitize(release_url)
        );
    }
    let binary = binary(install, release_url, &uname)?;
    upload(target, &binary)?;
    eprintln!("grofe: installed v{version} on {host}");
    Link::connect(&transport, ConnectOpts::default()).map_err(|e| e.to_string())
}

// ------------------------------------------------------------------ SHA-256

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 of `data` (FIPS 180-4).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }
    let mut out = [0u8; 32];
    for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *chunk = word.to_be_bytes();
    }
    out
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where a test or mirror points the release download, else `option`.
pub fn release_url(option: Option<String>) -> String {
    std::env::var(RELEASE_URL_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .or(option)
        .unwrap_or_else(|| DEFAULT_RELEASE_URL.to_string())
}

/// `remote-release-url`'s default: grofe's GitHub release downloads.
pub const DEFAULT_RELEASE_URL: &str = "https://github.com/termage/grofe/releases/download";

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-2 appendix B vectors, plus the empty string.
    #[test]
    fn sha256_matches_the_fips_vectors() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(&vec![b'a'; 1_000_000])),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn a_checksum_mismatch_is_refused() {
        let name = "grofe-v1.0.0-x86_64-unknown-linux-musl.tar.gz";
        let sums = format!("{}  {name}\n{}  other\n", hex(&sha256(b"good")), "00");
        assert!(verify(b"good", &sums, name).is_ok());
        let err = verify(b"evil", &sums, name).unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(verify(b"good", &sums, "missing.tar.gz").is_err());
        // `./name` and `*name` spellings are the same file.
        let sums = format!("{}  ./{name}\n", hex(&sha256(b"good")));
        assert!(verify(b"good", &sums, name).is_ok());
        let sums = format!("{} *{name}\n", hex(&sha256(b"good")).to_uppercase());
        assert!(verify(b"good", &sums, name).is_ok());
    }

    #[test]
    fn archive_names_follow_the_release_workflow() {
        for (uname, target) in [
            ("Linux x86_64", "x86_64-unknown-linux-musl"),
            ("Linux aarch64", "aarch64-unknown-linux-musl"),
            ("Darwin arm64", "aarch64-apple-darwin"),
            ("Darwin x86_64", "x86_64-apple-darwin"),
        ] {
            assert_eq!(release_target(uname).unwrap(), target, "{uname}");
        }
        assert!(release_target("FreeBSD amd64").is_err());
        assert_eq!(
            archive_name("0.3.1", "aarch64-apple-darwin"),
            "grofe-v0.3.1-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            release_file_url("https://example.invalid/dl/", "0.3.1", "SHA256SUMS"),
            "https://example.invalid/dl/v0.3.1/SHA256SUMS"
        );
    }

    #[test]
    fn the_question_names_host_platform_and_place() {
        assert_eq!(
            question("box", "Linux x86_64", "0.3.1"),
            "grofe is not installed on box (linux x86_64). Install v0.3.1 to ~/.local/share/grofe/bin/0.3.1/? [y/N] "
        );
    }

    #[test]
    fn a_symlinked_member_is_refused_and_a_plain_one_read() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("grofe-v1-x");
        std::fs::create_dir(&stage).unwrap();
        let tar = |name: &str| {
            let out = dir.path().join(name);
            let ok = Command::new("tar")
                .arg("czf")
                .arg(&out)
                .arg("-C")
                .arg(dir.path())
                .arg("grofe-v1-x")
                .status()
                .unwrap();
            assert!(ok.success());
            std::fs::read(out).unwrap()
        };
        std::os::unix::fs::symlink("/etc/hosts", stage.join("grofe")).unwrap();
        let err = unpack(&tar("link.tar.gz"), "grofe-v1-x/grofe").unwrap_err();
        assert!(err.contains("not a plain file"), "{err}");
        std::fs::remove_file(stage.join("grofe")).unwrap();
        std::fs::write(stage.join("grofe"), b"bin").unwrap();
        assert_eq!(
            unpack(&tar("plain.tar.gz"), "grofe-v1-x/grofe").unwrap(),
            b"bin"
        );
    }

    #[test]
    fn plain_http_is_taken_only_on_loopback() {
        for ok in [
            "http://127.0.0.1:8080/r",
            "http://127.1.2.3/r",
            "http://localhost:9/r",
            "http://[::1]:9/r",
            "http://[::1]/r",
        ] {
            assert!(loopback_http(ok), "{ok}");
        }
        for bad in [
            "http://example.invalid/r",
            "http://127.0.0.1.evil.example/r",
            "http://localhost.evil.example/r",
            "http://evil@127.0.0.1/r",
            "http://128.0.0.1/r",
            "http://[::2]/r",
            "http://[::1]x/r",
            "https://127.0.0.1/r",
            "file:///r",
        ] {
            assert!(!loopback_http(bad), "{bad}");
        }
        // A loopback http URL is fetched (nothing listens on port 9, so
        // curl fails) rather than refused.
        let err = fetch("http://127.0.0.1:9/v1/SHA256SUMS").unwrap_err();
        assert!(!err.contains("must use https"), "{err}");
    }

    #[test]
    fn plain_http_downloads_are_refused() {
        let err = fetch("http://example.invalid/v1/SHA256SUMS").unwrap_err();
        assert!(err.contains("https"), "{err}");
    }

    #[test]
    fn file_urls_and_plain_paths_are_read_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x");
        std::fs::write(&f, b"data").unwrap();
        assert_eq!(fetch(&f.display().to_string()).unwrap(), b"data");
        assert_eq!(fetch(&format!("file://{}", f.display())).unwrap(), b"data");
        assert!(fetch("/nonexistent/grofe/x").is_err());
    }
}
