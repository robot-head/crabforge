//! An ephemeral gres instance for tests.
//!
//! Runs `crabka-gres` in its in-memory mode: no broker, no tenant, no data
//! directory, gone when the fixture drops. That is the right trade for testing
//! SQL — substrate mode (where gres journals to a broker topic) is what
//! production uses, but it replays its whole write-ahead log on every start and
//! would make each test pay for it. The SQL surface is identical either way.
//!
//! The binary is not a Cargo dependency: it lives in the co-developed crabka
//! checkout. Tests that need SQL call [`Gres::try_start`] and skip themselves
//! when it is absent, so the suite stays green for contributors who have not
//! built crabka yet.

use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncBufReadExt as _, BufReader},
    process::{Child, Command},
};

/// Printed by `crabka-gres` on stdout once it is accepting connections.
const READY_MARKER: &str = "CRABKA_GRES_READY";

/// How long to wait for that marker before giving up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// A running `crabka-gres`, killed on drop.
pub struct Gres {
    child: Child,
    port: u16,
}

impl Gres {
    /// Locate the `crabka-gres` binary.
    ///
    /// Checked in order: `CRABFORGE_GRES_BIN`, this workspace's target
    /// directory (where `just` builds it), then `$CRABKA_DIR/target/debug`.
    pub fn find_binary() -> Option<PathBuf> {
        if let Ok(explicit) = std::env::var("CRABFORGE_GRES_BIN") {
            let path = PathBuf::from(explicit);
            return path.is_file().then_some(path);
        }

        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .map(|root| root.join("target/debug/crabka-gres"));
        if let Some(path) = workspace
            && path.is_file()
        {
            return Some(path);
        }

        let crabka = std::env::var("CRABKA_DIR").unwrap_or_else(|_| "../crabka".to_string());
        let path = PathBuf::from(crabka).join("target/debug/crabka-gres");
        path.is_file().then_some(path)
    }

    /// Start gres, or return `None` if the binary is not available.
    ///
    /// Tests should skip rather than fail in that case — see the module docs.
    pub async fn try_start() -> Option<Self> {
        let binary = Self::find_binary()?;

        // Ask the OS for a free port, then release it. There is a race here in
        // principle; in practice the window is a few milliseconds and gres
        // binds immediately.
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
            listener.local_addr().ok()?.port()
        };

        let mut child = Command::new(&binary)
            .arg("--listen")
            .arg(format!("127.0.0.1:{port}"))
            // No --data-dir: ephemeral in-memory engine.
            .arg("--auth")
            .arg("trust")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;

        let stdout = child.stdout.take()?;
        let ready = tokio::time::timeout(STARTUP_TIMEOUT, async {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.contains(READY_MARKER) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);

        if !ready {
            let _ = child.kill().await;
            return None;
        }
        Some(Self { child, port })
    }

    /// Connection string for `tokio_postgres::connect`.
    pub fn dsn(&self) -> String {
        format!(
            "host=127.0.0.1 port={} user=forge dbname=crab connect_timeout=10",
            self.port
        )
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Gres {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Skip the calling test unless gres is available, printing why.
///
/// ```ignore
/// let Some(gres) = require_gres().await else { return };
/// ```
pub async fn require_gres() -> Option<Gres> {
    match Gres::try_start().await {
        Some(gres) => Some(gres),
        None => {
            crate::skip(
                "crabka-gres",
                "not found or failed to start. Build it with\n  \
                 cargo build --manifest-path $CRABKA_DIR/Cargo.toml -p crabka-gres --bin crabka-gres\n  \
                 (or set CRABFORGE_GRES_BIN)",
            );
            None
        }
    }
}

/// Wait until a TCP port accepts connections.
pub async fn wait_for_port(port: u16, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}
