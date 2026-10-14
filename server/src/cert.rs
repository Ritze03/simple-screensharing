//! The relay's TLS certificate: generation, persistence, rotation.
//!
//! Browsers can't be told to trust a self-signed cert, but WebTransport lets a
//! page pin one by hash (`serverCertificateHashes`). That API only accepts an
//! **ECDSA P-256** cert whose total validity is **at most 14 days**, so the
//! relay's default mode is: generate such a cert, keep it in `state/`, rotate
//! it before it expires, and have clients carry its SHA-256 in the join link
//! (see `net.rs` on the client side).
//!
//! Two modes:
//!
//! - [`CertSource::SelfSigned`] (default) — ECDSA P-256, [`DEFAULT_VALIDITY`]
//!   (13 days: one day of slack under the browsers' 14-day limit), persisted
//!   under the state dir, regenerated when fewer than [`DEFAULT_ROTATE_BEFORE`]
//!   (2 days) are left.
//! - [`CertSource::Provided`] (`--cert` / `--key`) — a real CA-issued chain
//!   (e.g. Let's Encrypt). Browsers verify it normally, so no hash is needed
//!   and nothing is rotated; the files are re-read when their mtime changes so
//!   a renewed cert is picked up without a restart.
//!
//! Rotation swaps the endpoint's server config
//! (`quinn::Endpoint::set_server_config`). That only affects *new* handshakes;
//! connections already established keep the cert they were set up with, so a
//! rotation never drops anybody.
//!
//! `Why a sidecar `cert.meta` file instead of parsing the cert:` reading
//! `notAfter` back out of a persisted cert needs an X.509 parser, a dependency
//! this server otherwise has no use for. The relay writes the cert, so it
//! writes the dates it chose next to it; if the meta is missing or doesn't
//! match what's asked for (different SANs), the cert is simply regenerated.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use quinn::Endpoint;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::protocol::{CertHash, NATIVE_ALPN, WEBTRANSPORT_ALPN, cert_hash_of_der};
use crate::room::Registry;

/// Validity of a generated cert. Browsers refuse anything over 14 days; 13
/// leaves a day of slack (the cert is also back-dated, see [`CLOCK_SKEW`]).
pub const DEFAULT_VALIDITY: Duration = Duration::from_secs(13 * 24 * 3600);
/// Rotate once less than this is left. The previous cert stays valid for the
/// connections already using it, but a *join link* carrying its hash stops
/// working for browsers the moment the relay presents the new one — so this is
/// also roughly how long before expiry links start going stale.
pub const DEFAULT_ROTATE_BEFORE: Duration = Duration::from_secs(2 * 24 * 3600);
/// `notBefore` is pushed this far into the past so a browser whose clock runs
/// slightly behind doesn't see a cert from the future. Counted *inside* the
/// validity, so the 14-day cap still holds.
const CLOCK_SKEW: Duration = Duration::from_secs(3600);
/// Upper bound on how long the rotation task sleeps between checks (guards
/// against suspend/resume and clock jumps).
const MAX_CHECK_INTERVAL: Duration = Duration::from_secs(3600);

/// SANs put on a generated cert. Browsers don't check the name when pinning a
/// hash, but a native client or curl might, so cover the usual local names.
const DEFAULT_SANS: [&str; 4] = ["localhost", "simple-screenshare.local", "127.0.0.1", "::1"];

pub enum CertSource {
    /// Generate (and persist, if `state_dir` is set) a pinned-hash cert.
    SelfSigned {
        state_dir: Option<PathBuf>,
        /// Extra SANs beyond [`DEFAULT_SANS`] (e.g. the relay's public host).
        extra_sans: Vec<String>,
        validity: Duration,
        rotate_before: Duration,
    },
    /// Use a CA-issued PEM chain + key; never rotate.
    Provided { cert: PathBuf, key: PathBuf },
}

impl CertSource {
    /// In-memory, no persistence, default timings — for tests.
    #[cfg(test)]
    pub fn ephemeral() -> Self {
        Self::SelfSigned {
            state_dir: None,
            extra_sans: Vec::new(),
            validity: DEFAULT_VALIDITY,
            rotate_before: DEFAULT_ROTATE_BEFORE,
        }
    }
}

/// What the TUI (and tests) need to know about the cert currently served.
#[derive(Debug, Clone)]
pub struct CertInfo {
    /// SHA-256 of the leaf cert's DER — the join link's 4th field.
    pub hash: CertHash,
    /// `None` for a provided CA cert (we don't parse its dates).
    pub not_after: Option<SystemTime>,
    /// When the relay will swap it for a fresh one, `None` if it never does.
    pub rotate_at: Option<SystemTime>,
    /// `false` for a CA-provided cert.
    pub self_signed: bool,
    /// How many times this process has rotated the cert.
    pub rotations: u32,
}

struct Material {
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    info: CertInfo,
}

pub struct CertManager {
    source: CertSource,
    current: std::sync::Mutex<Material>,
}

impl CertManager {
    pub fn new(source: CertSource) -> Result<Arc<Self>> {
        let material = Self::load(&source)?;
        Ok(Arc::new(Self {
            source,
            current: std::sync::Mutex::new(material),
        }))
    }

    pub fn info(&self) -> CertInfo {
        self.current.lock().unwrap().info.clone()
    }

    /// A fresh `quinn::ServerConfig` for the current cert: ALPNs `h3`
    /// (browsers) and `ssshare/1` (native), see `relay::handle_connection`.
    pub fn server_config(&self) -> Result<quinn::ServerConfig> {
        let m = self.current.lock().unwrap();
        build_server_config(m.chain.clone(), m.key.clone_key())
    }

    fn load(source: &CertSource) -> Result<Material> {
        match source {
            CertSource::Provided { cert, key } => load_provided(cert, key),
            CertSource::SelfSigned {
                state_dir,
                extra_sans,
                validity,
                rotate_before,
            } => {
                let sans = sans_for(extra_sans);
                if let Some(dir) = state_dir
                    && let Some(m) = load_persisted(dir, &sans, *validity, *rotate_before)
                {
                    return Ok(m);
                }
                let m = generate(&sans, *validity, *rotate_before, 0)?;
                if let Some(dir) = state_dir {
                    if let Err(e) = persist(dir, &m, &sans) {
                        // Not fatal: the relay works, it just can't keep the
                        // hash stable across a restart.
                        tracing::warn!("could not persist certificate to {}: {e:#}", dir.display());
                    }
                }
                Ok(m)
            }
        }
    }

    /// Replace the served cert with a freshly generated one. Returns the new
    /// info. Only meaningful for [`CertSource::SelfSigned`].
    pub fn rotate_now(&self) -> Result<CertInfo> {
        let CertSource::SelfSigned {
            state_dir,
            extra_sans,
            validity,
            rotate_before,
        } = &self.source
        else {
            bail!("a provided certificate is not rotated by the relay");
        };
        let sans = sans_for(extra_sans);
        let rotations = self.current.lock().unwrap().info.rotations + 1;
        let m = generate(&sans, *validity, *rotate_before, rotations)?;
        if let Some(dir) = state_dir {
            if let Err(e) = persist(dir, &m, &sans) {
                tracing::warn!("could not persist rotated certificate: {e:#}");
            }
        }
        let info = m.info.clone();
        *self.current.lock().unwrap() = m;
        Ok(info)
    }

    /// Re-read a provided cert from disk (picks up a CA renewal).
    fn reload_provided(&self) -> Result<CertInfo> {
        let CertSource::Provided { .. } = &self.source else {
            bail!("only a provided certificate is reloaded");
        };
        let mut m = Self::load(&self.source)?;
        let mut cur = self.current.lock().unwrap();
        m.info.rotations = cur.info.rotations + 1;
        let info = m.info.clone();
        *cur = m;
        Ok(info)
    }

    /// Long-running task: keep `endpoint` serving a valid cert. Publishes the
    /// current [`CertInfo`] into `registry` for the TUI on every change.
    ///
    /// Self-signed: sleep until `rotate_at`, regenerate, `set_server_config`.
    /// Provided: poll the files' mtime hourly and reload on change.
    pub async fn maintain(self: Arc<Self>, endpoint: Endpoint, registry: Arc<Registry>) {
        registry.set_cert_info(self.info());
        let mut provided_mtime = self.provided_mtime();
        loop {
            let sleep_for = match self.info().rotate_at {
                Some(at) => at
                    .duration_since(SystemTime::now())
                    .unwrap_or(Duration::ZERO)
                    .min(MAX_CHECK_INTERVAL)
                    .max(Duration::from_millis(50)),
                None => MAX_CHECK_INTERVAL,
            };
            tokio::time::sleep(sleep_for).await;

            let renewed = match &self.source {
                CertSource::SelfSigned { .. } => {
                    let due = self
                        .info()
                        .rotate_at
                        .is_some_and(|at| SystemTime::now() >= at);
                    if !due {
                        continue;
                    }
                    self.rotate_now()
                }
                CertSource::Provided { .. } => {
                    let mtime = self.provided_mtime();
                    if mtime == provided_mtime {
                        continue;
                    }
                    provided_mtime = mtime;
                    self.reload_provided()
                }
            };
            match renewed.and_then(|info| self.server_config().map(|cfg| (info, cfg))) {
                Ok((info, cfg)) => {
                    endpoint.set_server_config(Some(cfg));
                    tracing::info!(
                        hash = %crate::protocol::encode_cert_hash(&info.hash),
                        rotations = info.rotations,
                        "certificate renewed; existing connections are unaffected"
                    );
                    registry.set_cert_info(info);
                }
                Err(e) => {
                    tracing::error!("certificate renewal failed, keeping the old one: {e:#}");
                    // Avoid a hot loop if it keeps failing.
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    fn provided_mtime(&self) -> Option<(SystemTime, SystemTime)> {
        let CertSource::Provided { cert, key } = &self.source else {
            return None;
        };
        let m = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        Some((m(cert)?, m(key)?))
    }
}

fn sans_for(extra: &[String]) -> Vec<String> {
    let mut sans: Vec<String> = DEFAULT_SANS.iter().map(|s| s.to_string()).collect();
    for s in extra {
        if !sans.contains(s) {
            sans.push(s.clone());
        }
    }
    sans
}

fn unix(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

fn generate(
    sans: &[String],
    validity: Duration,
    rotate_before: Duration,
    rotations: u32,
) -> Result<Material> {
    // Back-date by CLOCK_SKEW, but never more than a tenth of the validity so
    // the test hook's tiny validities still make sense.
    let skew = CLOCK_SKEW.min(validity / 10);
    let not_before = SystemTime::now() - skew;
    let not_after = not_before + validity;

    let mut params = CertificateParams::new(sans.to_vec()).context("building certificate params")?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "simple-screensharing relay");
    params.distinguished_name = dn;
    params.not_before = time::OffsetDateTime::from_unix_timestamp(unix(not_before))?;
    params.not_after = time::OffsetDateTime::from_unix_timestamp(unix(not_after))?;

    // ECDSA P-256 is the only key type browsers accept for hash pinning.
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).context("generating P-256 key")?;
    let cert = params
        .self_signed(&key_pair)
        .context("self-signing certificate")?;

    let cert_der: CertificateDer<'static> = cert.der().clone();
    let key: PrivateKeyDer<'static> = PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into();
    let hash = cert_hash_of_der(cert_der.as_ref());
    Ok(Material {
        chain: vec![cert_der],
        key,
        info: CertInfo {
            hash,
            not_after: Some(not_after),
            rotate_at: Some(not_after.checked_sub(rotate_before).unwrap_or(not_before)),
            self_signed: true,
            rotations,
        },
    })
}

// -- persistence -------------------------------------------------------

fn meta_text(m: &Material, sans: &[String]) -> String {
    let not_after = m.info.not_after.map(unix).unwrap_or(0);
    format!("not_after={not_after}\nsans={}\n", sans.join(","))
}

fn persist(dir: &Path, m: &Material, sans: &[String]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let cert_pem = pem_encode("CERTIFICATE", m.chain[0].as_ref());
    let key_pem = pem_encode("PRIVATE KEY", m.key.secret_der());
    write_private(&dir.join("key.pem"), key_pem.as_bytes())?;
    std::fs::write(dir.join("cert.pem"), cert_pem)?;
    // Written last: its presence marks the other two as a complete set.
    std::fs::write(dir.join("cert.meta"), meta_text(m, sans))?;
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

fn pem_encode(label: &str, der: &[u8]) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// Reuse the persisted cert if it is complete, was made for these SANs, and
/// still has more than `rotate_before` left (otherwise a restart would keep
/// serving a cert the rotation task is about to replace anyway — fine, but
/// regenerating up front is simpler and equally correct).
fn load_persisted(
    dir: &Path,
    sans: &[String],
    validity: Duration,
    rotate_before: Duration,
) -> Option<Material> {
    let meta = std::fs::read_to_string(dir.join("cert.meta")).ok()?;
    let mut not_after = None;
    let mut meta_sans = None;
    for line in meta.lines() {
        if let Some(v) = line.strip_prefix("not_after=") {
            not_after = v.parse::<i64>().ok();
        } else if let Some(v) = line.strip_prefix("sans=") {
            meta_sans = Some(v.to_string());
        }
    }
    if meta_sans? != sans.join(",") {
        return None;
    }
    let not_after = UNIX_EPOCH + Duration::from_secs(u64::try_from(not_after?).ok()?);
    let rotate_at = not_after.checked_sub(rotate_before)?;
    if SystemTime::now() >= rotate_at {
        return None;
    }
    // A cert persisted under a longer validity setting than requested (e.g.
    // someone lowered it) must not outlive the new policy.
    if not_after.duration_since(SystemTime::now()).ok()? > validity {
        return None;
    }
    let chain: Vec<_> = CertificateDer::pem_file_iter(dir.join("cert.pem"))
        .ok()?
        .collect::<Result<_, _>>()
        .ok()?;
    let key = PrivateKeyDer::from_pem_file(dir.join("key.pem")).ok()?;
    let hash = cert_hash_of_der(chain.first()?.as_ref());
    Some(Material {
        chain,
        key,
        info: CertInfo {
            hash,
            not_after: Some(not_after),
            rotate_at: Some(rotate_at),
            self_signed: true,
            rotations: 0,
        },
    })
}

fn load_provided(cert: &Path, key: &Path) -> Result<Material> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("opening certificate {}", cert.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing certificate {}", cert.display()))?;
    if chain.is_empty() {
        bail!("no certificate found in {}", cert.display());
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("reading private key {}", key.display()))?;
    let hash = cert_hash_of_der(chain[0].as_ref());
    Ok(Material {
        chain,
        key,
        info: CertInfo {
            hash,
            not_after: None,
            rotate_at: None,
            self_signed: false,
            rotations: 0,
        },
    })
}

/// Build the quinn server config around a cert: TLS 1.3 only (QUIC requires
/// it), both ALPNs, and the keep-alive/idle settings the native client matches.
pub fn build_server_config(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<quinn::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server_crypto = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("selecting TLS 1.3")?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .context("building rustls server config")?;
    // Offer h3 first: a browser only offers h3; native only offers ssshare/1.
    server_crypto.alpn_protocols = vec![WEBTRANSPORT_ALPN.to_vec(), NATIVE_ALPN.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
        .context("adapting rustls config for QUIC")?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));

    // Keep idle connections alive across manual-testing pauses (dialogs,
    // UI clicking, etc). Without this, quinn's default ~30s idle timeout
    // drops clients that briefly go quiet. keep_alive_interval must stay
    // comfortably below max_idle_timeout on at least one peer so pings
    // keep the connection from ever reaching the idle deadline, while a
    // truly dead peer still gets reaped in a reasonable time.
    let mut transport_config = quinn::TransportConfig::default();
    transport_config
        .max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(Duration::from_secs(60)).expect("60s fits in IdleTimeout"),
        ))
        .keep_alive_interval(Some(Duration::from_secs(15)));
    server_config.transport_config(Arc::new(transport_config));

    Ok(server_config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(validity: Duration, rotate_before: Duration, dir: Option<PathBuf>) -> CertSource {
        CertSource::SelfSigned {
            state_dir: dir,
            extra_sans: vec![],
            validity,
            rotate_before,
        }
    }

    #[test]
    fn generated_cert_is_browser_pinnable() {
        let mgr = CertManager::new(CertSource::ephemeral()).unwrap();
        let info = mgr.info();
        let remaining = info.not_after.unwrap().duration_since(SystemTime::now()).unwrap();
        assert!(remaining <= Duration::from_secs(14 * 24 * 3600));
        assert!(remaining > Duration::from_secs(12 * 24 * 3600));
        let rotate_in = info.rotate_at.unwrap().duration_since(SystemTime::now()).unwrap();
        assert!(rotate_in > Duration::from_secs(10 * 24 * 3600));
        mgr.server_config().expect("usable as a quinn server config");
    }

    #[test]
    fn persisted_cert_survives_restart_until_rotation_is_due() {
        let dir = std::env::temp_dir().join(format!("ssshare-cert-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = || short(DEFAULT_VALIDITY, DEFAULT_ROTATE_BEFORE, Some(dir.clone()));
        let first = CertManager::new(src()).unwrap().info().hash;
        let second = CertManager::new(src()).unwrap().info().hash;
        assert_eq!(first, second, "restart must keep the hash");

        // Different SANs => different cert.
        let with_san = CertSource::SelfSigned {
            state_dir: Some(dir.clone()),
            extra_sans: vec!["relay.example.com".into()],
            validity: DEFAULT_VALIDITY,
            rotate_before: DEFAULT_ROTATE_BEFORE,
        };
        assert_ne!(CertManager::new(with_san).unwrap().info().hash, first);

        // Corrupt state => regenerate rather than fail.
        std::fs::write(dir.join("cert.pem"), "garbage").unwrap();
        CertManager::new(src()).unwrap();

        // A cert already inside its rotation window is not reused.
        let due = || short(Duration::from_secs(100), Duration::from_secs(99), Some(dir.clone()));
        let a = CertManager::new(due()).unwrap().info();
        std::thread::sleep(Duration::from_millis(1100));
        let b = CertManager::new(due()).unwrap().info();
        assert_ne!(a.hash, b.hash);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotate_now_changes_hash_and_counts() {
        let mgr = CertManager::new(CertSource::ephemeral()).unwrap();
        let before = mgr.info();
        let after = mgr.rotate_now().unwrap();
        assert_ne!(before.hash, after.hash);
        assert_eq!(after.rotations, 1);
        assert_eq!(mgr.info().hash, after.hash);
    }

    /// `--cert/--key` (what certbot / Let's Encrypt produce): served as-is,
    /// never rotated by us, picked up again by `reload_provided`.
    #[test]
    fn provided_ca_cert_is_served_unrotated_and_reloadable() {
        let dir = std::env::temp_dir().join(format!("ssshare-provided-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let write_pair = |name: &str| {
            let key = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
            std::fs::write(dir.join("fullchain.pem"), key.cert.pem()).unwrap();
            std::fs::write(dir.join("privkey.pem"), key.signing_key.serialize_pem()).unwrap();
            cert_hash_of_der(key.cert.der())
        };
        let first = write_pair("relay.example.com");
        let mgr = CertManager::new(CertSource::Provided {
            cert: dir.join("fullchain.pem"),
            key: dir.join("privkey.pem"),
        })
        .unwrap();
        let info = mgr.info();
        assert_eq!(info.hash, first);
        assert!(!info.self_signed && info.rotate_at.is_none() && info.not_after.is_none());
        mgr.server_config().expect("usable as a quinn server config");
        assert!(mgr.rotate_now().is_err(), "a CA cert is never rotated by the relay");

        let second = write_pair("relay.example.com");
        assert_ne!(first, second);
        let reloaded = mgr.reload_provided().unwrap();
        assert_eq!(reloaded.hash, second);
        assert_eq!(mgr.info().hash, second);

        // Missing files are an error at startup, not a silent fallback.
        assert!(
            CertManager::new(CertSource::Provided {
                cert: dir.join("nope.pem"),
                key: dir.join("privkey.pem"),
            })
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
