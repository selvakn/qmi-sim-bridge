//! at-qmi-bridge: present a modem's AT port as a serial-like pty and answer
//! `AT+CSIM` (raw SIM APDUs) over QMI UIM, for modems whose firmware lacks
//! `AT+CSIM` but whose QMI UIM service supports logical channels.
//!
//! Everything else is forwarded to the real AT port unchanged, except:
//! - `AT+QCFG="ims"` reports IMS disabled (`2`); setting it is acknowledged.
//! - `AT+CGSN` is sent as `AT+GSN` (this firmware never terminates CGSN).
//! - `AT+CFUN=<n>` is not forwarded (it would drop the radio and the data
//!   link); it resets the SIM session instead and returns OK.

mod at;
mod pty;
mod qmi;
mod sim;

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use at::Command;

/// Partial USIM AID (RID + application code); used when discovery fails.
const USIM_AID_PREFIX: &[u8] = &[0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x02];

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

struct Args {
    at_port: PathBuf,
    qmi_device: String,
    link: PathBuf,
    slot: u8,
    aid: Option<Vec<u8>>,
    proxy: bool,
    proxy_wait: u64,
}

const USAGE: &str = "\
Usage: at-qmi-bridge [options]

  --at <path>      modem AT port              (default /dev/wwan0at1)
  --qmi <path>     modem QMI device           (default /dev/wwan0qmi0)
  --link <path>    symlink to create for the pty (default /dev/ttyUSB3)
  --slot <n>       SIM slot                   (default 1)
  --aid <hex>      USIM AID (default: discovered with AT+CUAD)
  --no-proxy       open the QMI device directly instead of via qmi-proxy
  --proxy-wait <s> how long to wait for qmi-proxy before opening the
                   device directly (default 120)
  -h, --help       show this help
";

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        at_port: "/dev/wwan0at1".into(),
        qmi_device: "/dev/wwan0qmi0".into(),
        link: "/dev/ttyUSB3".into(),
        slot: 1,
        aid: None,
        proxy: true,
        proxy_wait: 120,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{k} needs a value"));
        match k.as_str() {
            "--at" => a.at_port = val()?.into(),
            "--qmi" => a.qmi_device = val()?,
            "--link" => a.link = val()?.into(),
            "--slot" => a.slot = val()?.parse().map_err(|_| "bad --slot".to_string())?,
            "--aid" => a.aid = Some(at::hex_decode(&val()?).ok_or("bad --aid hex")?),
            "--no-proxy" => a.proxy = false,
            "--proxy-wait" => {
                a.proxy_wait = val()?.parse().map_err(|_| "bad --proxy-wait".to_string())?
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument {k}\n\n{USAGE}")),
        }
    }
    Ok(a)
}

/// Send one command on the raw AT port and collect lines until a final result.
fn at_query(port: &mut File, cmd: &str, timeout: Duration) -> std::io::Result<Vec<String>> {
    port.write_all(format!("{cmd}\r").as_bytes())?;
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    while Instant::now() < deadline {
        if !pty::readable(port.as_raw_fd(), 200)? {
            continue;
        }
        let n = port.read(&mut chunk)?;
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf);
        let lines: Vec<String> = text
            .split(['\r', '\n'])
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        if lines.iter().any(|l| l == "OK" || l == "ERROR" || l.starts_with("+CME ERROR")) {
            return Ok(lines);
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("{cmd}: no final result")))
}

/// Find the USIM AID in an `AT+CUAD` reply (EF_DIR records, tag 61 / 4F).
fn usim_aid_from_cuad(lines: &[String]) -> Option<Vec<u8>> {
    let hex = lines.iter().find_map(|l| l.strip_prefix("+CUAD:"))?;
    let data = at::hex_decode(hex.trim().trim_matches('"'))?;
    let mut i = 0;
    while i + 2 <= data.len() {
        let (tag, len) = (data[i], data[i + 1] as usize);
        let rec = data.get(i + 2..i + 2 + len)?;
        if tag == 0x61 && rec.len() >= 2 && rec[0] == 0x4F {
            let aid = rec.get(2..2 + rec[1] as usize)?;
            if aid.starts_with(USIM_AID_PREFIX) {
                return Some(aid.to_vec());
            }
        }
        i += 2 + len;
    }
    None
}

fn connect_qmi(a: &Args) -> Result<qmi::Uim, qmi::QmiError> {
    if a.proxy {
        // At boot ModemManager starts qmi-proxy some time after us; wait for
        // it rather than racing it for the device.
        let deadline = Instant::now() + Duration::from_secs(a.proxy_wait);
        loop {
            match qmi::Uim::connect_proxy(&a.qmi_device, Duration::from_secs(15)) {
                Ok(q) => return Ok(q),
                Err(e) if Instant::now() < deadline && !STOP.load(Ordering::SeqCst) => {
                    eprintln!("qmi-proxy not ready ({e}); retrying");
                    std::thread::sleep(Duration::from_secs(5));
                }
                Err(e) => {
                    eprintln!("qmi-proxy unavailable ({e}); opening {} directly", a.qmi_device);
                    break;
                }
            }
        }
    }
    qmi::Uim::connect_device(&a.qmi_device)
}

fn run(a: Args) -> Result<(), String> {
    let mut modem = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&a.at_port)
        .map_err(|e| format!("open {}: {e}", a.at_port.display()))?;

    let aid = match a.aid.clone() {
        Some(aid) => aid,
        None => match at_query(&mut modem, "AT+CUAD", Duration::from_secs(5)) {
            Ok(lines) => usim_aid_from_cuad(&lines).unwrap_or_else(|| {
                eprintln!("no USIM in AT+CUAD reply {lines:?}; using partial AID");
                USIM_AID_PREFIX.to_vec()
            }),
            Err(e) => {
                eprintln!("AT+CUAD failed ({e}); using partial AID");
                USIM_AID_PREFIX.to_vec()
            }
        },
    };

    let uim = connect_qmi(&a).map_err(|e| format!("QMI: {e}"))?;
    eprintln!("QMI UIM client {} on {}", uim.cid(), a.qmi_device);
    let mut session = sim::Session::new(uim, a.slot, aid);

    let pty = pty::open().map_err(|e| format!("openpty: {e}"))?;
    pty::link(&pty.slave_path, &a.link)
        .map_err(|e| format!("link {}: {e}", a.link.display()))?;
    eprintln!(
        "serving {} -> {} (AT port {})",
        a.link.display(),
        pty.slave_path.display(),
        a.at_port.display()
    );

    let out = Arc::new(Mutex::new(pty.master.try_clone().map_err(|e| e.to_string())?));

    // Modem -> pty: responses and unsolicited result codes, verbatim.
    {
        let out = Arc::clone(&out);
        let mut rx = modem.try_clone().map_err(|e| e.to_string())?;
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match rx.read(&mut buf) {
                    // EOF: the port went away (modem restart). Exit and let
                    // procd respawn us against the new port.
                    Ok(0) => {
                        eprintln!("AT port closed");
                        STOP.store(true, Ordering::SeqCst);
                        return;
                    }
                    Ok(n) => {
                        let _ = out.lock().unwrap().write_all(&buf[..n]);
                    }
                    Err(e) => {
                        eprintln!("AT port read failed: {e}");
                        STOP.store(true, Ordering::SeqCst);
                        return;
                    }
                }
            }
        });
    }

    // pty -> modem, intercepting the commands this bridge answers itself.
    let mut master = pty.master;
    let mut line: Vec<u8> = Vec::new();
    let mut echo = true;
    let mut buf = [0u8; 1024];
    while !STOP.load(Ordering::SeqCst) {
        if !pty::readable(master.as_raw_fd(), 1000).map_err(|e| e.to_string())? {
            continue;
        }
        let n = master.read(&mut buf).map_err(|e| format!("pty read: {e}"))?;
        for &b in &buf[..n] {
            match b {
                b'\r' => {
                    let l = String::from_utf8_lossy(&line).into_owned();
                    line.clear();
                    if l.trim().is_empty() {
                        continue;
                    }
                    handle_line(&l, &mut echo, &mut session, &mut modem, &out)?;
                }
                b'\n' => {}
                // Ctrl-Z / ESC end an SMS text/PDU prompt: forward as-is.
                0x1A | 0x1B => {
                    line.push(b);
                    modem.write_all(&line).map_err(|e| e.to_string())?;
                    line.clear();
                }
                _ => line.push(b),
            }
        }
    }
    pty::unlink_if_ours(&pty.slave_path, &a.link);
    session.reset();
    if let Err(e) = session.into_inner().release() {
        eprintln!("release QMI client: {e}");
    }
    Ok(())
}

fn handle_line(
    l: &str,
    echo: &mut bool,
    session: &mut sim::Session<qmi::Uim>,
    modem: &mut File,
    out: &Mutex<File>,
) -> Result<(), String> {
    let cmd = at::classify(l);
    let shown = if *echo { Some(l) } else { None };
    let reply = match cmd {
        Command::Passthrough => None,
        Command::Rewrite(to) => {
            return modem
                .write_all(format!("{to}\r").as_bytes())
                .map_err(|e| format!("AT port write: {e}"));
        }
        Command::Echo(on) => {
            *echo = on;
            None
        }
        Command::Csim(apdu) => Some(match session.transmit(&apdu) {
            Ok(r) => at::reply(shown, &[at::csim_info(&r)], true),
            Err(e) => {
                eprintln!("CSIM {}: {e}", at::hex_encode(&apdu));
                at::reply(shown, &[], false)
            }
        }),
        Command::CsimInvalid => Some(at::reply(shown, &[], false)),
        Command::CsimTest => Some(at::reply(shown, &[], true)),
        Command::QcfgImsQuery => Some(at::reply(shown, &["+QCFG: \"ims\",2,0".into()], true)),
        Command::QcfgImsSet => {
            eprintln!("ignoring {l} (modem has no IMS stack)");
            Some(at::reply(shown, &[], true))
        }
        Command::CfunSet(arg) => {
            eprintln!("AT+CFUN={arg}: not forwarded; resetting SIM session");
            session.reset();
            Some(at::reply(shown, &[], true))
        }
    };
    match reply {
        Some(r) => out.lock().unwrap().write_all(&r).map_err(|e| e.to_string()),
        None => modem
            .write_all(format!("{l}\r").as_bytes())
            .map_err(|e| format!("AT port write: {e}")),
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_usim_aid_in_cuad() {
        // Captured from the HMUF02-V5 modem.
        let lines = vec![
            "+CUAD: \"61184F10A0000000871002FFF605FF89000001FF50045553494D\"".to_string(),
            "OK".to_string(),
        ];
        assert_eq!(
            usim_aid_from_cuad(&lines).map(|a| at::hex_encode(&a)),
            Some("A0000000871002FFF605FF89000001FF".to_string())
        );
    }

    #[test]
    fn ignores_cuad_without_usim() {
        let lines = vec!["+CUAD: \"61094F07A0000000041010\"".to_string()];
        assert_eq!(usim_aid_from_cuad(&lines), None);
    }
}
