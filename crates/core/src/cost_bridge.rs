//! BLE bridge between the thClaws REPL and a `thClaws-Cost` Cardputer
//! display. Best-effort sidecar: the REPL keeps running even when no
//! device is in range; the bridge silently retries connection in the
//! background.
//!
//! ## Wire shape
//!
//! Nordic UART Service (`6e400001-…`), line-delimited JSON.
//!
//! - REPL → device:  `{"cost": 0.0234}`  (sent after each turn)
//! - device → REPL:  `{"reset": true}`   (user hit Backspace / Fn+`)
//!
//! ## Channels
//!
//! - `tx_cost`  — REPL sends accumulated USD; bridge writes to NUS RX
//! - `rx_reset` — bridge surfaces reset notifications; REPL zeros its
//!                `session_cost_usd` in response
//!
//! Both are unbounded; the volumes are tiny (one event per turn).

use std::time::Duration;

use btleplug::api::{Central, Manager as _, Peripheral, ScanFilter, WriteType};
use btleplug::platform::{Manager, Peripheral as PlatformPeripheral};
use futures::stream::StreamExt;
use tokio::sync::mpsc;
use uuid::Uuid;

// Nordic UART Service: the thClaws-Cost firmware advertises this and
// exposes RX (writeable) + TX (notify) characteristics with these UUIDs.
const NUS_RX_UUID: Uuid = Uuid::from_u128(0x6e400002_b5a3_f393_e0a9_e50e24dcca9e);
const NUS_TX_UUID: Uuid = Uuid::from_u128(0x6e400003_b5a3_f393_e0a9_e50e24dcca9e);

/// Local-name prefix the firmware advertises. We match any peripheral
/// whose name starts with this so multiple devices in the same room
/// still get picked up (the firmware suffixes with the BT MAC).
const DEVICE_NAME_PREFIX: &str = "thClaws-Cost";

/// Handle returned by [`spawn`]. Hold onto it for the lifetime of the
/// REPL; dropping it closes the cost channel which terminates the
/// background task on its next iteration.
pub struct CostBridge {
    /// Cost updates from REPL → bridge. Cloneable if you need multiple
    /// senders; we only use one in `run_interactive` today.
    pub tx_cost: mpsc::UnboundedSender<f64>,
    /// Reset notifications from bridge → REPL. The REPL polls
    /// `try_recv` once per loop iteration; we use an unbounded channel
    /// because a missed reset would surprise the user.
    pub rx_reset: mpsc::UnboundedReceiver<()>,
}

/// Spawn the background reconnect loop. Returns immediately. The task
/// will keep retrying connection forever; closing `tx_cost` is the
/// only way to terminate it.
pub fn spawn() -> CostBridge {
    let (tx_cost, rx_cost) = mpsc::unbounded_channel();
    let (tx_reset, rx_reset) = mpsc::unbounded_channel();
    tokio::spawn(run(rx_cost, tx_reset));
    CostBridge { tx_cost, rx_reset }
}

async fn run(mut rx_cost: mpsc::UnboundedReceiver<f64>, tx_reset: mpsc::UnboundedSender<()>) {
    // Remember the most recent cost so a fresh connection can immediately
    // render the right number instead of $0.0000 until the next turn.
    let mut latest_cost: f64 = 0.0;
    let mut have_cost = false;

    loop {
        // Drain any pending cost updates that arrived while disconnected,
        // keeping only the latest. We'll push it once we're back online.
        while let Ok(c) = rx_cost.try_recv() {
            latest_cost = c;
            have_cost = true;
        }
        let _ = try_once(&mut rx_cost, &tx_reset, &mut latest_cost, &mut have_cost).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn try_once(
    rx_cost: &mut mpsc::UnboundedReceiver<f64>,
    tx_reset: &mpsc::UnboundedSender<()>,
    latest_cost: &mut f64,
    have_cost: &mut bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let manager = Manager::new().await?;
    let adapter = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or("no BLE adapter")?;

    adapter.start_scan(ScanFilter::default()).await?;

    // Poll for a matching peripheral. 20s ceiling is long enough for
    // most pair-on-boot scenarios; if nothing matches we fall through
    // to the outer reconnect-wait.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let peripheral: PlatformPeripheral = loop {
        if std::time::Instant::now() > deadline {
            let _ = adapter.stop_scan().await;
            return Err("scan timeout — no thClaws-Cost in range".into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        let candidates = adapter.peripherals().await?;
        let mut found: Option<PlatformPeripheral> = None;
        for p in candidates {
            if let Ok(Some(props)) = p.properties().await {
                if let Some(name) = props.local_name {
                    if name.starts_with(DEVICE_NAME_PREFIX) {
                        found = Some(p);
                        break;
                    }
                }
            }
        }
        if let Some(p) = found {
            let _ = adapter.stop_scan().await;
            break p;
        }
    };

    peripheral.connect().await?;
    peripheral.discover_services().await?;

    let chars = peripheral.characteristics();
    let rx_char = chars
        .iter()
        .find(|c| c.uuid == NUS_RX_UUID)
        .ok_or("NUS RX characteristic not advertised")?
        .clone();
    let tx_char = chars
        .iter()
        .find(|c| c.uuid == NUS_TX_UUID)
        .ok_or("NUS TX characteristic not advertised")?
        .clone();

    peripheral.subscribe(&tx_char).await?;
    let mut notif_stream = peripheral.notifications().await?;

    // Push the latest cost on (re)connect so the user sees the right
    // total even if no new turn has fired since pairing.
    if *have_cost {
        let _ = write_cost(&peripheral, &rx_char, *latest_cost).await;
    }

    loop {
        tokio::select! {
            cost = rx_cost.recv() => {
                let Some(c) = cost else { return Ok(()); }; // REPL shutting down
                *latest_cost = c;
                *have_cost = true;
                if write_cost(&peripheral, &rx_char, c).await.is_err() {
                    return Err("write failed — peripheral likely dropped".into());
                }
            }
            notif = notif_stream.next() => {
                let Some(n) = notif else {
                    return Err("notification stream ended".into());
                };
                if let Ok(s) = std::str::from_utf8(&n.value) {
                    if s.contains("\"reset\"") {
                        let _ = tx_reset.send(());
                    }
                }
            }
        }
    }
}

async fn write_cost(
    peripheral: &PlatformPeripheral,
    rx_char: &btleplug::api::Characteristic,
    cost: f64,
) -> Result<(), btleplug::Error> {
    // Six decimal places leaves headroom — display rounds to four. The
    // float-precision overhead vs four decimals is one byte on the wire.
    let line = format!("{{\"cost\":{:.6}}}\n", cost);
    peripheral
        .write(rx_char, line.as_bytes(), WriteType::WithResponse)
        .await
}
