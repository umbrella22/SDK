use std::sync::{Arc, OnceLock};

use uda_core::capability::{Capability, SupportLevel};
use uda_core::wakelock::{WakeLockGuard, WakeLockManager, WakeLockType};
use zbus::Connection;

use crate::error::UdaError;
use crate::internal_dbus;

/// `org.freedesktop.ScreenSaver` on the session bus, where a wake lock lives.
const SCREEN_SAVER_SERVICE: &str = "org.freedesktop.ScreenSaver";
const SCREEN_SAVER_PATH: &str = "/org/freedesktop/ScreenSaver";
const SCREEN_SAVER_INTERFACE: &str = "org.freedesktop.ScreenSaver";

/// `org.freedesktop.login1` on the system bus: systemd-logind, the component
/// that receives - and therefore is the only one that can honour - a
/// `systemd-inhibit` lock.
const LOGIND_SERVICE: &str = "org.freedesktop.login1";

pub struct LinuxWakeLockManager {
    connection: Arc<Connection>,
}

impl LinuxWakeLockManager {
    pub async fn new() -> Result<Self, UdaError> {
        let connection =
            internal_dbus("connecting to the session bus", Connection::session()).await?;
        Ok(Self {
            connection: Arc::new(connection),
        })
    }

    /// The native backend's capability set, cached after the first probe.
    ///
    /// `WAKE_LOCK` requires the ScreenSaver service this manager calls.
    /// The FFI layer's separate logind CLI fallback is not part of this manager.
    pub fn capabilities(&self) -> Capability {
        wake_lock_support(Self::reachability()).0
    }

    /// `Partial` when ScreenSaver is reachable: display-idle inhibition is
    /// supported, but the protocol cannot request system-suspend inhibition.
    /// `None` means this manager has no receiver for either lock type.
    pub fn support_level(&self) -> SupportLevel {
        wake_lock_support(Self::reachability()).1
    }

    /// Whether the native wake-lock receiver is reachable, probed once per process.
    ///
    /// A probe error caches as "absent": a receiver that cannot be reached
    /// cannot be claimed either, the same collapse
    /// `LinuxAppearanceManager::accent_color_key_available` applies.
    fn reachability() -> bool {
        static REACHABLE: OnceLock<bool> = OnceLock::new();
        *REACHABLE.get_or_init(|| probe_or_absent(crate::sync::run_async(screensaver_present())))
    }
}

/// Is systemd-logind (`org.freedesktop.login1`) reachable on the system bus?
///
/// The CLI wake-lock tier spawns `systemd-inhibit`, whose lock only exists
/// because logind receives it: with no logind on the system bus nothing
/// answers the inhibit, the screen still sleeps, and reporting success would
/// hand the host a lock that is pure fiction (any host without a system
/// manager qualifies - a container or a `dbus-run-session` sandbox, or WSL
/// with systemd disabled). The granularity is deliberately manager-level:
/// `Inhibit` needs the daemon, not a seat session, so a systemd-enabled WSL2
/// (which runs logind but never establishes a seat session) answers honestly
/// here. Every step is bounded by `crate::DBUS_TIMEOUT` and mapped per the
/// crate's D-Bus error style; any error means "not proven present", and
/// callers must refuse the CLI tier rather than spawn a lock nobody honours.
pub async fn logind_present() -> Result<bool, UdaError> {
    let connection = internal_dbus("connecting to the system bus", Connection::system()).await?;
    bus_has_service(
        &connection,
        "building the system bus daemon proxy",
        LOGIND_SERVICE,
    )
    .await
}

/// Is the native ScreenSaver inhibit service reachable on the session bus?
///
/// Same contract as [`logind_present`], one tier up: this is the receiver the
/// manager's own `acquire` speaks to, and one of the two the session backend's
/// capability matrix probes.
pub(crate) async fn screensaver_present() -> Result<bool, UdaError> {
    let connection = internal_dbus("connecting to the session bus", Connection::session()).await?;
    bus_has_service(
        &connection,
        "building the session bus daemon proxy",
        SCREEN_SAVER_SERVICE,
    )
    .await
}

/// The unique names currently owned on `connection`, from one listing.
///
/// `DBusProxy::list_names` resolves to the fdo error type rather than
/// `zbus::Error`, so that one call cannot go through
/// [`crate::internal_dbus`]; it gets its own `DBUS_TIMEOUT` and the same
/// mapping instead. [`bus_has_service`] is the one-name convenience wrapper
/// over it; the session backend's logout-candidate probe is a different query
/// (it needs activatable names too) and keeps its own helper in `session.rs`
/// rather than sharing this round trip.
pub(crate) async fn bus_unique_names(
    connection: &Connection,
    step: &str,
) -> Result<Vec<String>, UdaError> {
    let proxy = internal_dbus(step, zbus::fdo::DBusProxy::new(connection)).await?;
    match tokio::time::timeout(crate::DBUS_TIMEOUT, proxy.list_names()).await {
        Ok(Ok(names)) => Ok(names.iter().map(|name| name.as_str().to_string()).collect()),
        Ok(Err(e)) => Err(UdaError::Internal(format!("listing bus names failed: {e}"))),
        Err(_) => Err(UdaError::Internal(format!(
            "listing bus names timed out after {:?}",
            crate::DBUS_TIMEOUT
        ))),
    }
}

/// Whether `service` is currently owned on `connection`.
///
/// One [`bus_unique_names`] round trip plus an exact-name match; the
/// name-matching semantics are pinned by the tests below.
pub(crate) async fn bus_has_service(
    connection: &Connection,
    step: &str,
    service: &str,
) -> Result<bool, UdaError> {
    let names = bus_unique_names(connection, step).await?;
    Ok(bus_names_contain(
        names.iter().map(|name| name.as_str()),
        service,
    ))
}

/// Collapse a bridged probe into a plain bool.
///
/// The sync bridge wraps the probe's own `Result`, so there are two failure
/// layers: the bridge failing to run the future at all, and the probe failing
/// on the bus. For capability honesty both mean the same thing - "not proven
/// present" - so both collapse to `false`, the same answer
/// `LinuxAppearanceManager::accent_color_key_available` gives its probe.
fn probe_or_absent(probe: Result<Result<bool, UdaError>, UdaError>) -> bool {
    match probe {
        Ok(Ok(reachable)) => reachable,
        Ok(Err(error)) | Err(error) => {
            log::debug!("wake-lock reachability probe did not answer: {error}");
            false
        }
    }
}

/// Turn probe answers into the honest capability bit and support level.
///
/// ScreenSaver exposes display-idle inhibition only. A logind service does not
/// add a capability to this manager because its `acquire` never calls logind.
fn wake_lock_support(screensaver: bool) -> (Capability, SupportLevel) {
    if screensaver {
        return (
            Capability::WAKE_LOCK,
            SupportLevel::Partial(
                "ScreenSaver supports display-idle inhibition only; system-suspend \
                 inhibition is not supported by this native backend"
                    .to_string(),
            ),
        );
    }
    (Capability::empty(), SupportLevel::None)
}

/// Whether the well-known `service` name appears in `names`.
///
/// Pure so the matching rule is testable without a bus. Only an exact match
/// counts, and the comparison is case-sensitive, because D-Bus well-known
/// names are: `org.freedesktop.Login1` is a different name from logind's, and
/// a unique connection name (`:1.42`) never matches a well-known service.
fn bus_names_contain<'a, I>(names: I, service: &str) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    names.into_iter().any(|name| name == service)
}

#[async_trait::async_trait]
impl WakeLockManager for LinuxWakeLockManager {
    async fn acquire(
        &self,
        lock_type: WakeLockType,
        reason: &str,
    ) -> Result<WakeLockGuard, UdaError> {
        if lock_type == WakeLockType::PreventSystemIdle {
            return Err(UdaError::NotSupported(
                "ScreenSaver cannot request system-suspend inhibition".to_string(),
            ));
        }

        let proxy = internal_dbus(
            "building the ScreenSaver proxy",
            zbus::Proxy::new(
                &self.connection,
                SCREEN_SAVER_SERVICE,
                SCREEN_SAVER_PATH,
                SCREEN_SAVER_INTERFACE,
            ),
        )
        .await?;

        let cookie: u32 = internal_dbus(
            "the Inhibit call",
            proxy.call("Inhibit", &(env!("CARGO_PKG_NAME"), reason)),
        )
        .await?;

        let connection = Arc::clone(&self.connection);

        Ok(WakeLockGuard::new(Box::new(move || {
            // Releasing is best effort: the guard has already been consumed, so
            // a failure can only be logged. The crate's `run_async` bridge is
            // used instead of `std::thread::spawn` because this is a `Drop`
            // path and thread construction there panics when the OS refuses;
            // `run_async` reports that failure as a value.
            let release = crate::sync::run_async(async move {
                let proxy = internal_dbus(
                    "building the ScreenSaver proxy to release the wake lock",
                    zbus::Proxy::new(
                        &connection,
                        SCREEN_SAVER_SERVICE,
                        SCREEN_SAVER_PATH,
                        SCREEN_SAVER_INTERFACE,
                    ),
                )
                .await?;

                internal_dbus(
                    "releasing the wake lock",
                    proxy.call::<&str, (u32,), ()>("UnInhibit", &(cookie,)),
                )
                .await
            });
            // The bridge has two failure layers: it may fail to run the future
            // at all, or the future itself may report that `UnInhibit` failed.
            // Both leave the lock held, so both are logged.
            match release {
                Ok(Ok(())) => log::debug!("wake lock released"),
                Ok(Err(error)) | Err(error) => {
                    log::warn!("could not release the wake lock: {error}");
                }
            }
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct IsolatedBus {
        child: std::process::Child,
        directory: std::path::PathBuf,
    }

    impl Drop for IsolatedBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[derive(Default)]
    struct ScreenSaverCalls {
        inhibit: std::sync::Mutex<Vec<(String, String)>>,
        uninhibit: std::sync::Mutex<Vec<u32>>,
        released: tokio::sync::Notify,
    }

    struct MockScreenSaver(Arc<ScreenSaverCalls>);

    #[zbus::interface(name = "org.freedesktop.ScreenSaver")]
    impl MockScreenSaver {
        // The real protocol accepts precisely two strings, without flags.
        fn inhibit(&self, app_name: &str, reason: &str) -> zbus::fdo::Result<u32> {
            self.0
                .inhibit
                .lock()
                .map_err(|_| zbus::fdo::Error::Failed("calls lock poisoned".to_string()))?
                .push((app_name.to_string(), reason.to_string()));
            Ok(42)
        }

        fn un_inhibit(&self, cookie: u32) -> zbus::fdo::Result<()> {
            self.0
                .uninhibit
                .lock()
                .map_err(|_| zbus::fdo::Error::Failed("calls lock poisoned".to_string()))?
                .push(cookie);
            self.0.released.notify_one();
            Ok(())
        }
    }

    #[tokio::test]
    async fn native_acquisition_obeys_the_screen_saver_protocol(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::io::BufRead as _;
        use std::process::Stdio;

        // Explicit addresses keep the fixture away from the host's real buses.
        let directory =
            std::env::temp_dir().join(format!("uda-wakelock-mock-{}", std::process::id()));
        std::fs::create_dir(&directory)?;
        let config = directory.join("bus.conf");
        std::fs::write(
            &config,
            format!(
                "<busconfig><type>session</type><listen>unix:tmpdir={}</listen><auth>EXTERNAL</auth><policy context=\"default\"><allow send_destination=\"*\"/><allow receive_sender=\"*\"/><allow own=\"*\"/></policy></busconfig>",
                directory.display()
            ),
        )?;
        let mut bus = IsolatedBus {
            child: std::process::Command::new("dbus-daemon")
                .args(["--nofork", "--print-address=1"])
                .arg(format!("--config-file={}", config.display()))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?,
            directory,
        };
        let stdout = bus.child.stdout.take().ok_or("missing bus address pipe")?;
        let mut address = String::new();
        std::io::BufReader::new(stdout).read_line(&mut address)?;
        let address = address.trim();
        let calls = Arc::new(ScreenSaverCalls::default());
        let _server = zbus::connection::Builder::address(address)?
            .name(SCREEN_SAVER_SERVICE)?
            .serve_at(SCREEN_SAVER_PATH, MockScreenSaver(Arc::clone(&calls)))?
            .build()
            .await?;
        let manager = LinuxWakeLockManager {
            connection: Arc::new(zbus::connection::Builder::address(address)?.build().await?),
        };

        assert!(matches!(
            manager
                .acquire(WakeLockType::PreventSystemIdle, "system sleep")
                .await,
            Err(UdaError::NotSupported(_))
        ));
        assert!(calls
            .inhibit
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .is_empty());

        let guard = manager
            .acquire(WakeLockType::PreventDisplaySleep, "playing a movie")
            .await?;
        assert_eq!(
            *calls.inhibit.lock().map_err(|_| "calls lock poisoned")?,
            vec![(
                env!("CARGO_PKG_NAME").to_string(),
                "playing a movie".to_string()
            )]
        );
        // Release joins a blocking cleanup worker; keep this runtime polling
        // the mock service until its UnInhibit reply has been delivered.
        tokio::task::spawn_blocking(move || guard.release()).await?;
        tokio::time::timeout(crate::DBUS_TIMEOUT, calls.released.notified()).await?;
        assert_eq!(
            *calls.uninhibit.lock().map_err(|_| "calls lock poisoned")?,
            vec![42]
        );
        Ok(())
    }

    #[test]
    fn guard_runs_release_on_drop() {
        let released = Arc::new(AtomicBool::new(false));
        let released_clone = Arc::clone(&released);
        let guard = WakeLockGuard::new(Box::new(move || {
            released_clone.store(true, Ordering::SeqCst);
        }));
        drop(guard);
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn guard_manual_release() {
        let released = Arc::new(AtomicBool::new(false));
        let released_clone = Arc::clone(&released);
        let guard = WakeLockGuard::new(Box::new(move || {
            released_clone.store(true, Ordering::SeqCst);
        }));
        guard.release();
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn bus_names_match_only_the_exact_well_known_service() {
        let bus = ["org.freedesktop.DBus", "org.freedesktop.login1", ":1.42"];
        assert!(bus_names_contain(bus, LOGIND_SERVICE));
        assert!(bus_names_contain(
            ["org.freedesktop.ScreenSaver"],
            SCREEN_SAVER_SERVICE
        ));

        // D-Bus names are case-sensitive: logind is lowercase, and a different
        // case is a different peer.
        assert!(!bus_names_contain(
            ["org.freedesktop.Login1"],
            LOGIND_SERVICE
        ));
        // A name that merely contains or extends the service is not the
        // service.
        assert!(!bus_names_contain(
            ["org.freedesktop.login1.daemon"],
            LOGIND_SERVICE
        ));
        assert!(!bus_names_contain(
            ["xxorg.freedesktop.login1"],
            LOGIND_SERVICE
        ));
        // Unique connection names never count: without the well-known name a
        // caller cannot address the service anyway.
        assert!(!bus_names_contain([":1.42"], LOGIND_SERVICE));
        assert!(!bus_names_contain([] as [&str; 0], LOGIND_SERVICE));
    }

    #[test]
    fn screen_saver_support_is_partial_with_a_reason() {
        let (caps, level) = wake_lock_support(true);
        assert_eq!(caps, Capability::WAKE_LOCK);
        let reason = match &level {
            SupportLevel::Partial(reason) => reason.as_str(),
            _ => panic!("ScreenSaver reachability must degrade to Partial, got {level:?}"),
        };
        assert!(
            !reason.is_empty(),
            "a degraded answer must carry its reason (AGENTS.md Principle 1)"
        );
        assert_eq!(level.reason(), Some(reason));
    }

    #[test]
    fn no_receiver_claims_nothing() {
        let (caps, level) = wake_lock_support(false);
        assert!(caps.is_empty(), "no receiver, no claimed capability");
        assert_eq!(level, SupportLevel::None);
    }
}
