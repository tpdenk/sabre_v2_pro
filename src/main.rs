use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};

const MOUSE_HID_ID: &str = "HID_ID=0003:00001B1C:00002B28";
const DONGLE_HID_ID: &str = "HID_ID=0003:00001B1C:00002B2A";
const REPORT_ID: u8 = 0x08;
const CMD_READ: u8 = 0x04;
const CMD_READ_DONGLE: u8 = 0x03;
const BATTERY_TAG: u8 = 0x49;
const PRESENCE_TAG: u8 = 0x4a;
const TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Parser)]
#[command(name = "sabre_v2_pro", about = "Corsair Sabre v2 Pro utility")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print battery level (0-100), nothing and exit code 1 when not connected
    Battery,
    /// Print 1 while charging, 0 otherwise, nothing and exit code 1 when not connected
    Charging,
}

#[derive(PartialEq)]
enum Connection {
    Wired,
    Dongle,
}

/// While charging, cell voltage reads above resting and the firmware naively
/// maps voltage to percent, inflating the level. Compensate by re-mapping the
/// corrected voltage through the firmware's own curve. 55 mV fits two
/// simultaneous wired-vs-wireless comparisons (raw 70 vs 55, raw 95 vs 90).
const CHARGE_MV_OFFSET: u16 = 55;

/// Voltage-to-percent curve. Anchors at 3917/4030/4076 mV are values the
/// firmware itself reported, the rest is a generic Li-ion discharge curve.
const OCV_CURVE: [(u16, u8); 8] = [
    (3500, 0),
    (3600, 10),
    (3700, 25),
    (3800, 40),
    (3917, 55),
    (4030, 85),
    (4076, 95),
    (4200, 100),
];

fn ocv_percent(mv: u16) -> u8 {
    if mv <= OCV_CURVE[0].0 {
        return OCV_CURVE[0].1;
    }
    for pair in OCV_CURVE.windows(2) {
        let ((v0, p0), (v1, p1)) = (pair[0], pair[1]);
        if mv <= v1 {
            let frac = u32::from(mv - v0) * u32::from(p1 - p0) / u32::from(v1 - v0);
            return p0 + frac as u8;
        }
    }
    100
}

struct BatteryStatus {
    level: u8,
    charging: bool,
    millivolts: u16,
}

impl BatteryStatus {
    fn effective_level(&self) -> u8 {
        if self.charging {
            ocv_percent(self.millivolts.saturating_sub(CHARGE_MV_OFFSET))
        } else {
            self.level
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Some(status) = battery_status() else {
        return ExitCode::FAILURE;
    };
    match cli.command {
        Command::Battery => println!("{}", status.effective_level()),
        Command::Charging => println!("{}", u8::from(status.charging)),
    }
    ExitCode::SUCCESS
}

fn battery_status() -> Option<BatteryStatus> {
    let (node, connection) = vendor_node()?;
    let mut dev = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(node)
        .ok()?;

    if connection == Connection::Dongle {
        let presence = transfer(&mut dev, CMD_READ_DONGLE, PRESENCE_TAG)?;
        if !mouse_online(&presence) {
            return None;
        }
    }
    parse_battery(&transfer(&mut dev, CMD_READ, BATTERY_TAG)?)
}

/// Write a vendor command and read until the matching echo, other input
/// reports (wheel, dpi events) share the same hidraw node and are skipped.
fn transfer(dev: &mut fs::File, cmd: u8, tag: u8) -> Option<[u8; 64]> {
    let mut query = [0u8; 64];
    query[0] = REPORT_ID;
    query[1] = cmd;
    query[16] = tag;
    dev.write_all(&query).ok()?;

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let mut pfd = libc::pollfd {
            fd: dev.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as i32) } <= 0 {
            return None;
        }
        let mut resp = [0u8; 64];
        // hidraw returns one report per read, a short read is a foreign report
        let n = dev.read(&mut resp).ok()?;
        if n >= 8 && resp[0] == REPORT_ID && resp[1] == cmd {
            return Some(resp);
        }
    }
}

/// Prefers the wired mouse: with both cable and dongle plugged in, the dongle
/// reports the mouse as offline.
fn vendor_node() -> Option<(PathBuf, Connection)> {
    let mut dongle = None;
    for entry in fs::read_dir("/sys/class/hidraw").ok()?.flatten() {
        let Ok(uevent) = fs::read_to_string(entry.path().join("device/uevent")) else {
            continue;
        };
        let Some(connection) = connection_kind(&uevent) else {
            continue;
        };
        let node = PathBuf::from("/dev").join(entry.file_name());
        match connection {
            Connection::Wired => return Some((node, connection)),
            Connection::Dongle => dongle = Some((node, connection)),
        }
    }
    dongle
}

/// Both the mouse and its dongle expose three HID interfaces, the vendor
/// command channel is on input1.
fn connection_kind(uevent: &str) -> Option<Connection> {
    if !uevent
        .lines()
        .any(|l| l.starts_with("HID_PHYS=") && l.ends_with("/input1"))
    {
        return None;
    }
    match uevent.lines().find(|l| l.starts_with("HID_ID="))? {
        MOUSE_HID_ID => Some(Connection::Wired),
        DONGLE_HID_ID => Some(Connection::Dongle),
        _ => None,
    }
}

fn mouse_online(resp: &[u8]) -> bool {
    matches!(resp, [REPORT_ID, CMD_READ_DONGLE, _, _, _, _, 1, ..])
}

fn parse_battery(resp: &[u8]) -> Option<BatteryStatus> {
    match resp {
        [
            REPORT_ID,
            CMD_READ,
            0x00,
            _,
            _,
            _,
            level,
            charging,
            mv_hi,
            mv_lo,
            ..,
        ] => Some(BatteryStatus {
            level: *level,
            charging: *charging != 0,
            millivolts: u16::from_be_bytes([*mv_hi, *mv_lo]),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_battery_ok() {
        let resp = [0x08, 0x04, 0x00, 0x00, 0x00, 0x02, 55, 0x01, 0x0f, 0x4d];
        let status = parse_battery(&resp).expect("valid response");
        assert_eq!(status.level, 55, "level is byte 6");
        assert!(status.charging, "charging flag is byte 7");
        assert_eq!(status.millivolts, 3917, "voltage is bytes 8-9 big-endian");
    }

    #[test]
    fn ocv_percent_bounds_and_anchors() {
        assert_eq!(ocv_percent(3200), 0, "below curve clamps to 0");
        assert_eq!(ocv_percent(4300), 100, "above curve clamps to 100");
        assert_eq!(ocv_percent(3917), 55, "firmware-observed anchor");
        assert_eq!(ocv_percent(4030), 85, "firmware-observed anchor");
    }

    #[test]
    fn effective_level_compensates_only_while_charging() {
        let resting = BatteryStatus {
            level: 55,
            charging: false,
            millivolts: 3917,
        };
        assert_eq!(
            resting.effective_level(),
            55,
            "resting level passes through"
        );
        let charging = BatteryStatus {
            level: 85,
            charging: true,
            millivolts: 4030,
        };
        let level = charging.effective_level();
        assert!(level < 85, "charging level must be deflated, got {level}");
        assert_eq!(
            level,
            ocv_percent(4030 - CHARGE_MV_OFFSET),
            "corrected voltage through curve"
        );
    }

    #[test]
    fn parse_battery_rejects_error_status() {
        let resp = [0x08, 0x04, 0x01, 0, 0, 0, 0, 0];
        assert!(parse_battery(&resp).is_none(), "non-zero status byte");
    }

    #[test]
    fn parse_battery_rejects_foreign_or_short_report() {
        assert!(
            parse_battery(&[0x10, 0, 0, 0, 0, 0, 0, 0]).is_none(),
            "unrelated report id"
        );
        assert!(parse_battery(&[0x08, 0x04]).is_none(), "truncated report");
    }

    #[test]
    fn mouse_online_flag() {
        assert!(
            mouse_online(&[0x08, 0x03, 0, 0, 0, 0, 1, 0]),
            "online when byte 6 is 1"
        );
        assert!(
            !mouse_online(&[0x08, 0x03, 0, 0, 0, 0, 0, 0]),
            "offline when byte 6 is 0"
        );
    }

    #[test]
    fn connection_kind_by_hid_id() {
        let wired = "DRIVER=hid-generic\nHID_ID=0003:00001B1C:00002B28\nHID_PHYS=usb-0000:67:00.0-1.2/input1\n";
        assert!(
            matches!(connection_kind(wired), Some(Connection::Wired)),
            "mouse pid on input1"
        );
        let dongle = wired.replace("2B28", "2B2A");
        assert!(
            matches!(connection_kind(&dongle), Some(Connection::Dongle)),
            "dongle pid on input1"
        );
        assert!(
            connection_kind(&wired.replace("input1", "input0")).is_none(),
            "input0 is not the vendor channel"
        );
    }
}
