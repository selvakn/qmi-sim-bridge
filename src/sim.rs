//! Basic-channel APDU semantics on top of a QMI logical channel.
//!
//! `AT+CSIM` clients address the basic channel (CLA channel bits 0). This
//! modem only accepts APDUs on logical channels, so the session opens one to
//! the USIM, rewrites each APDU's CLA to that channel, and fetches `61xx`
//! continuation data with GET RESPONSE so the client sees one complete reply.

use crate::qmi::{QmiError, Uim};

pub trait Transport {
    /// Returns the channel id and the SELECT response (FCP) of `aid`.
    fn open_channel(&mut self, slot: u8, aid: &[u8]) -> Result<(u8, Vec<u8>), QmiError>;
    fn send_apdu(&mut self, slot: u8, channel: u8, apdu: &[u8]) -> Result<Vec<u8>, QmiError>;
    fn close_channel(&mut self, slot: u8, channel: u8) -> Result<(), QmiError>;
}

impl Transport for Uim {
    fn open_channel(&mut self, slot: u8, aid: &[u8]) -> Result<(u8, Vec<u8>), QmiError> {
        Uim::open_channel(self, slot, aid)
    }
    fn send_apdu(&mut self, slot: u8, channel: u8, apdu: &[u8]) -> Result<Vec<u8>, QmiError> {
        Uim::send_apdu(self, slot, channel, apdu)
    }
    fn close_channel(&mut self, slot: u8, channel: u8) -> Result<(), QmiError> {
        Uim::close_channel(self, slot, channel)
    }
}

/// Encode logical `channel` into `cla` (ISO/IEC 7816-4 §5.4.1).
pub fn cla_for_channel(cla: u8, channel: u8) -> u8 {
    if channel <= 3 {
        // First interindustry (and proprietary 8x) class: b2..b1 = channel.
        let base = if cla & 0x40 != 0 { cla & 0x80 } else { cla & 0xFC };
        base | channel
    } else {
        // Further interindustry class: b7 = 1, b4..b1 = channel - 4,
        // keep b8 (proprietary) and b5 (command chaining).
        0x40 | (cla & 0x80) | (cla & 0x10) | (channel - 4)
    }
}

pub struct Session<T: Transport> {
    t: T,
    slot: u8,
    aid: Vec<u8>,
    channel: Option<u8>,
    /// SELECT response of `aid` from the latest channel open.
    fcp: Vec<u8>,
}

impl<T: Transport> Session<T> {
    pub fn new(t: T, slot: u8, aid: Vec<u8>) -> Self {
        Session {
            t,
            slot,
            aid,
            channel: None,
            fcp: Vec::new(),
        }
    }

    fn channel(&mut self) -> Result<u8, QmiError> {
        if let Some(c) = self.channel {
            return Ok(c);
        }
        let (c, fcp) = self.t.open_channel(self.slot, &self.aid)?;
        self.fcp = fcp;
        eprintln!("opened logical channel {c} to AID {}", crate::at::hex_encode(&self.aid));
        self.channel = Some(c);
        Ok(c)
    }

    /// Drop the logical channel; the next APDU opens a fresh one, which
    /// resets the selected file state the way a card reset would.
    pub fn reset(&mut self) {
        if let Some(c) = self.channel.take() {
            if let Err(e) = self.t.close_channel(self.slot, c) {
                eprintln!("close channel {c}: {e}");
            }
        }
    }

    pub fn into_inner(self) -> T {
        self.t
    }

    /// Transmit a basic-channel APDU; returns data + SW1 SW2.
    pub fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>, QmiError> {
        match self.transmit_once(apdu) {
            Ok(r) => Ok(r),
            // The channel may have been closed under us (SIM refresh, modem
            // restart, a stale client): reopen once and retry.
            Err(QmiError::Protocol(..)) => {
                self.reset();
                self.transmit_once(apdu)
            }
            // The QMI connection died (qmi-proxy restarted with ModemManager).
            // The channel belonged to that client and is gone with it; the
            // transport reconnects on the next call.
            Err(QmiError::Io(e)) => {
                eprintln!("QMI connection lost ({e}); reconnecting");
                self.channel = None;
                self.transmit_once(apdu)
            }
            Err(e) => {
                self.reset();
                Err(e)
            }
        }
    }

    /// SELECT by DF name (`A4 04`) naming the session's application, by full
    /// or partial AID.
    fn selects_session_aid(&self, apdu: &[u8]) -> bool {
        if apdu.len() < 5 || apdu[1] != 0xA4 || apdu[2] != 0x04 {
            return false;
        }
        let name = &apdu[5..apdu.len().min(5 + apdu[4] as usize)];
        !name.is_empty() && (self.aid.starts_with(name) || name.starts_with(&self.aid))
    }

    fn transmit_once(&mut self, apdu: &[u8]) -> Result<Vec<u8>, QmiError> {
        // The modem refuses to SELECT an application by AID through a raw
        // APDU (it owns application selection). A freshly opened channel has
        // exactly that application selected; on an established one, select
        // it again by its FID alias 7FFF ("current ADF"). Reopening the
        // channel instead makes the modem announce SIM changes, which
        // ModemManager may treat as a SIM swap.
        let alias;
        let apdu = if self.selects_session_aid(apdu) {
            if self.channel.is_none() {
                self.channel()?;
                let mut r = if apdu[3] & 0x0C == 0x0C { Vec::new() } else { self.fcp.clone() };
                r.extend_from_slice(&[0x90, 0x00]);
                return Ok(r);
            }
            alias = [apdu[0], 0xA4, 0x00, apdu[3], 0x02, 0x7F, 0xFF];
            &alias[..]
        } else {
            apdu
        };
        let ch = self.channel()?;
        let mut cmd = apdu.to_vec();
        cmd[0] = cla_for_channel(apdu[0], ch);
        let mut resp = self.t.send_apdu(self.slot, ch, &cmd)?;
        let mut data = Vec::new();
        // GET RESPONSE loop for SW1=61 (more data available).
        for _ in 0..16 {
            let n = resp.len();
            if n >= 2 && resp[n - 2] == 0x61 {
                let le = resp[n - 1];
                data.extend_from_slice(&resp[..n - 2]);
                let get = [cla_for_channel(apdu[0] & 0x80, ch), 0xC0, 0x00, 0x00, le];
                resp = self.t.send_apdu(self.slot, ch, &get)?;
            } else {
                break;
            }
        }
        data.extend_from_slice(&resp);
        if data.len() < 2 {
            return Err(QmiError::Malformed("APDU response without status word".into()));
        }
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn cla_mapping() {
        assert_eq!(cla_for_channel(0x00, 1), 0x01);
        assert_eq!(cla_for_channel(0x80, 2), 0x82);
        assert_eq!(cla_for_channel(0x00, 4), 0x40);
        assert_eq!(cla_for_channel(0x80, 5), 0xC1);
        assert_eq!(cla_for_channel(0x10, 7), 0x53);
    }

    #[derive(Default)]
    struct Fake {
        opens: u32,
        sent: Vec<(u8, Vec<u8>)>,
        replies: VecDeque<Result<Vec<u8>, QmiError>>,
        closed: Vec<u8>,
    }

    impl Transport for &mut Fake {
        fn open_channel(&mut self, _slot: u8, _aid: &[u8]) -> Result<(u8, Vec<u8>), QmiError> {
            self.opens += 1;
            Ok((self.opens as u8, vec![0x62, 0x00]))
        }
        fn send_apdu(&mut self, _slot: u8, ch: u8, apdu: &[u8]) -> Result<Vec<u8>, QmiError> {
            self.sent.push((ch, apdu.to_vec()));
            self.replies.pop_front().unwrap()
        }
        fn close_channel(&mut self, _slot: u8, ch: u8) -> Result<(), QmiError> {
            self.closed.push(ch);
            Ok(())
        }
    }

    #[test]
    fn rewrites_cla_and_opens_channel_once() {
        let mut f = Fake::default();
        f.replies.push_back(Ok(vec![0x90, 0x00]));
        f.replies.push_back(Ok(vec![0x98, 0x62]));
        {
            let mut s = Session::new(&mut f, 1, vec![0xA0]);
            assert_eq!(s.transmit(&[0x00, 0xA4, 0x00, 0x0C, 0x02, 0x3F, 0x00]).unwrap(), [0x90, 0x00]);
            assert_eq!(s.transmit(&[0x00, 0x88, 0x00, 0x81, 0x00]).unwrap(), [0x98, 0x62]);
        }
        assert_eq!(f.opens, 1);
        assert_eq!(f.sent[0], (1, vec![0x01, 0xA4, 0x00, 0x0C, 0x02, 0x3F, 0x00]));
        assert_eq!(f.sent[1].1[0], 0x01);
    }

    #[test]
    fn follows_61xx_with_get_response() {
        let mut f = Fake::default();
        f.replies.push_back(Ok(vec![0x61, 0x03]));
        f.replies.push_back(Ok(vec![0xDB, 0x01, 0x02, 0x90, 0x00]));
        {
            let mut s = Session::new(&mut f, 1, vec![0xA0]);
            assert_eq!(s.transmit(&[0x00, 0x88, 0x00, 0x81, 0x00]).unwrap(), [0xDB, 0x01, 0x02, 0x90, 0x00]);
        }
        assert_eq!(f.sent[1], (1, vec![0x01, 0xC0, 0x00, 0x00, 0x03]));
    }

    #[test]
    fn reopens_channel_after_qmi_error() {
        let mut f = Fake::default();
        f.replies.push_back(Err(QmiError::Protocol(1, 0x30)));
        f.replies.push_back(Ok(vec![0x90, 0x00]));
        {
            let mut s = Session::new(&mut f, 1, vec![0xA0]);
            assert_eq!(s.transmit(&[0x00, 0xF2, 0x00, 0x00, 0x00]).unwrap(), [0x90, 0x00]);
        }
        assert_eq!(f.opens, 2);
        assert_eq!(f.closed, vec![1]);
        assert_eq!(f.sent[1], (2, vec![0x02, 0xF2, 0x00, 0x00, 0x00]));
    }

    #[test]
    fn select_of_session_aid_uses_7fff_alias() {
        let mut f = Fake::default();
        f.replies.push_back(Ok(vec![0x90, 0x00]));
        f.replies.push_back(Ok(vec![0x90, 0x00]));
        {
            let mut s = Session::new(&mut f, 1, vec![0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x02, 0xFF]);
            // First SELECT AID opens the channel and is answered locally.
            let full = [0x00, 0xA4, 0x04, 0x0C, 0x08, 0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x02, 0xFF];
            assert_eq!(s.transmit(&full).unwrap(), [0x90, 0x00]);
            let fcp = [0x00, 0xA4, 0x04, 0x04, 0x07, 0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x02];
            s.reset();
            assert_eq!(s.transmit(&fcp).unwrap(), [0x62, 0x00, 0x90, 0x00]);
            // On an open channel it becomes SELECT 7FFF, with the same P2.
            s.transmit(&[0x00, 0xF2, 0x00, 0x00, 0x00]).unwrap();
            assert_eq!(s.transmit(&full).unwrap(), [0x90, 0x00]);
        }
        assert_eq!(f.opens, 2);
        assert_eq!(f.closed, vec![1]);
        assert_eq!(f.sent[1], (2, vec![0x02, 0xA4, 0x00, 0x0C, 0x02, 0x7F, 0xFF]));
    }

    #[test]
    fn select_of_other_aid_is_sent_to_card() {
        let mut f = Fake::default();
        f.replies.push_back(Ok(vec![0x6A, 0x82]));
        {
            let mut s = Session::new(&mut f, 1, vec![0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x02]);
            let isim = [0x00, 0xA4, 0x04, 0x0C, 0x07, 0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x04];
            assert_eq!(s.transmit(&isim).unwrap(), [0x6A, 0x82]);
        }
        assert_eq!(f.sent.len(), 1);
    }

    #[test]
    fn reconnects_after_io_error_without_closing() {
        let mut f = Fake::default();
        f.replies.push_back(Err(QmiError::Io(std::io::ErrorKind::BrokenPipe.into())));
        f.replies.push_back(Ok(vec![0x90, 0x00]));
        {
            let mut s = Session::new(&mut f, 1, vec![0xA0]);
            assert_eq!(s.transmit(&[0x00, 0xF2, 0x00, 0x00, 0x00]).unwrap(), [0x90, 0x00]);
        }
        assert_eq!(f.opens, 2);
        assert!(f.closed.is_empty(), "a dead client's channel is not closed");
    }

    #[test]
    fn reset_closes_and_reopens() {
        let mut f = Fake::default();
        f.replies.push_back(Ok(vec![0x90, 0x00]));
        f.replies.push_back(Ok(vec![0x90, 0x00]));
        {
            let mut s = Session::new(&mut f, 1, vec![0xA0]);
            s.transmit(&[0x00, 0xF2, 0x00, 0x00, 0x00]).unwrap();
            s.reset();
            s.transmit(&[0x00, 0xF2, 0x00, 0x00, 0x00]).unwrap();
        }
        assert_eq!(f.closed, vec![1]);
        assert_eq!(f.opens, 2);
    }
}
