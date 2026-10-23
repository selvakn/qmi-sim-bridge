//! Virtual smart card for pcscd's vpcd driver (vsmartcard), backed by the SIM
//! over QMI.
//!
//! pcscd's `libifdvpcd` listens on TCP; a virtual card connects to it. Every
//! message is a 2-byte big-endian length plus payload. A 1-byte payload is a
//! control message (00 power off, 01 power on, 02 reset, 04 get ATR); anything
//! longer is a command APDU, answered with the response APDU.
//!
//! Clients (strongSwan's eap-sim-pcsc, gsm-sip-bridge's PC/SC transport) speak
//! classic T=0: a SELECT or AUTHENTICATE that returns data is presented as
//! `61xx`, and the data is served on the following GET RESPONSE. The QMI path
//! returns data and status word in one go, so the full response is cached and
//! the `61xx` is synthesized here.

use std::io::{self, Read, Write};

/// Minimal legal ATR (direct convention, no interface/historical bytes); the
/// same one gsm-sip-bridge's own vpcd bridge presents.
pub const ATR: &[u8] = &[0x3B, 0x00];

#[derive(Debug, PartialEq)]
pub enum Message {
    PowerOff,
    PowerOn,
    Reset,
    GetAtr,
    Apdu(Vec<u8>),
    Unknown(u8),
}

impl Message {
    pub fn from_payload(p: &[u8]) -> Self {
        if p.len() == 1 {
            match p[0] {
                0x00 => Message::PowerOff,
                0x01 => Message::PowerOn,
                0x02 => Message::Reset,
                0x04 => Message::GetAtr,
                b => Message::Unknown(b),
            }
        } else {
            Message::Apdu(p.to_vec())
        }
    }
}

/// Reads one frame. `Ok(None)` on a clean EOF between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 2];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut p = vec![0u8; u16::from_be_bytes(len) as usize];
    r.read_exact(&mut p)?;
    Ok(Some(p))
}

pub fn write_frame(w: &mut impl Write, p: &[u8]) -> io::Result<()> {
    let len = u16::try_from(p.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(p)?;
    w.flush()
}

/// T=0 presentation layer over a transmit function that returns full
/// responses (data + SW1 SW2).
#[derive(Default)]
pub struct Card {
    powered: bool,
    last: Option<Vec<u8>>,
}

impl Card {
    pub fn handle(
        &mut self,
        msg: Message,
        transmit: &mut dyn FnMut(&[u8]) -> Result<Vec<u8>, String>,
    ) -> Option<Vec<u8>> {
        match msg {
            Message::PowerOff => {
                self.powered = false;
                self.last = None;
                None
            }
            Message::PowerOn | Message::Reset => {
                self.powered = true;
                self.last = None;
                None
            }
            Message::GetAtr => Some(ATR.to_vec()),
            Message::Unknown(b) => {
                eprintln!("vpcd: unknown control byte {b:#04x}, ignored");
                None
            }
            Message::Apdu(apdu) => Some(self.apdu(&apdu, transmit)),
        }
    }

    fn apdu(
        &mut self,
        apdu: &[u8],
        transmit: &mut dyn FnMut(&[u8]) -> Result<Vec<u8>, String>,
    ) -> Vec<u8> {
        if !self.powered {
            return vec![0x6F, 0x00];
        }
        if apdu.len() < 4 {
            return vec![0x67, 0x00]; // wrong length
        }
        // GET RESPONSE: serve the cached full response, if there is one.
        if apdu[1] == 0xC0 {
            if let Some(r) = self.last.take() {
                return r;
            }
        }
        match transmit(apdu) {
            Ok(r) => {
                let classic = matches!(apdu[1], 0xA4 | 0x88);
                let n = r.len();
                if classic && n > 2 && r[n - 2] == 0x90 && r[n - 1] == 0x00 {
                    let data_len = (n - 2).min(0xFF) as u8;
                    self.last = Some(r);
                    vec![0x61, data_len]
                } else {
                    self.last = None;
                    r
                }
            }
            Err(e) => {
                eprintln!("vpcd: APDU {} failed: {e}", crate::at::hex_encode(apdu));
                self.last = None;
                vec![0x6F, 0x00]
            }
        }
    }
}

/// Serves one vpcd connection until it closes.
pub fn serve(
    stream: &mut (impl Read + Write),
    transmit: &mut dyn FnMut(&[u8]) -> Result<Vec<u8>, String>,
    trace: bool,
) -> io::Result<()> {
    let mut card = Card::default();
    while let Some(p) = read_frame(stream)? {
        let msg = Message::from_payload(&p);
        if trace {
            eprintln!("vpcd > {msg:?}");
        }
        if let Some(resp) = card.handle(msg, transmit) {
            if trace {
                eprintln!("vpcd < {}", crate::at::hex_encode(&resp));
            }
            write_frame(stream, &resp)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(p: &[u8]) -> Vec<u8> {
        let mut v = (p.len() as u16).to_be_bytes().to_vec();
        v.extend_from_slice(p);
        v
    }

    struct Duplex {
        input: io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Read for Duplex {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.input.read(b)
        }
    }
    impl Write for Duplex {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn parses_control_messages() {
        assert_eq!(Message::from_payload(&[0x00]), Message::PowerOff);
        assert_eq!(Message::from_payload(&[0x01]), Message::PowerOn);
        assert_eq!(Message::from_payload(&[0x02]), Message::Reset);
        assert_eq!(Message::from_payload(&[0x04]), Message::GetAtr);
        assert_eq!(Message::from_payload(&[0x09]), Message::Unknown(0x09));
        assert_eq!(
            Message::from_payload(&[0x00, 0xA4]),
            Message::Apdu(vec![0x00, 0xA4])
        );
    }

    #[test]
    fn session_round_trip_with_t0_chaining() {
        let auth = [0x00, 0x88, 0x00, 0x81, 0x22];
        let mut input = Vec::new();
        input.extend(frame(&[0x04])); // ATR
        input.extend(frame(&[0x01])); // power on
        input.extend(frame(&[0x00, 0xA4, 0x00, 0x0C, 0x02, 0x3F, 0x00]));
        input.extend(frame(&auth));
        input.extend(frame(&[0x00, 0xC0, 0x00, 0x00, 0x03]));
        let mut s = Duplex {
            input: io::Cursor::new(input),
            output: vec![],
        };
        let mut sent = Vec::new();
        let mut transmit = |a: &[u8]| -> Result<Vec<u8>, String> {
            sent.push(a.to_vec());
            Ok(if a[1] == 0x88 {
                vec![0xDB, 0x01, 0x02, 0x90, 0x00]
            } else {
                vec![0x90, 0x00]
            })
        };
        serve(&mut s, &mut transmit, false).unwrap();

        let mut expect = Vec::new();
        expect.extend(frame(ATR));
        expect.extend(frame(&[0x90, 0x00])); // SELECT without data: plain 9000
        expect.extend(frame(&[0x61, 0x03])); // AUTHENTICATE with data: 61xx
        expect.extend(frame(&[0xDB, 0x01, 0x02, 0x90, 0x00])); // GET RESPONSE from cache
        assert_eq!(s.output, expect);
        assert_eq!(sent.len(), 2, "GET RESPONSE never reaches the SIM");
    }

    #[test]
    fn unpowered_and_errors_answer_6f00() {
        let mut card = Card::default();
        let mut ok = |_: &[u8]| -> Result<Vec<u8>, String> { Ok(vec![0x90, 0x00]) };
        assert_eq!(
            card.handle(Message::Apdu(vec![0, 0xF2, 0, 0, 0]), &mut ok),
            Some(vec![0x6F, 0x00])
        );
        card.handle(Message::PowerOn, &mut ok);
        let mut fail = |_: &[u8]| -> Result<Vec<u8>, String> { Err("x".into()) };
        assert_eq!(
            card.handle(Message::Apdu(vec![0, 0xF2, 0, 0, 0]), &mut fail),
            Some(vec![0x6F, 0x00])
        );
    }

    #[test]
    fn non_chaining_commands_pass_data_through() {
        let mut card = Card::default();
        let mut t = |_: &[u8]| -> Result<Vec<u8>, String> { Ok(vec![0x08, 0x49, 0x90, 0x00]) };
        card.handle(Message::PowerOn, &mut t);
        // READ BINARY returns its data directly.
        assert_eq!(
            card.handle(Message::Apdu(vec![0x00, 0xB0, 0x00, 0x00, 0x02]), &mut t),
            Some(vec![0x08, 0x49, 0x90, 0x00])
        );
    }
}
