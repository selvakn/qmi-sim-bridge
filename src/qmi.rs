//! Minimal QMI client: QMUX framing, the libqmi `qmi-proxy` handshake, CTL
//! client-id allocation and the three UIM messages needed for raw APDU access
//! (open logical channel, send APDU, close logical channel).
//!
//! The modem rejects APDUs on the basic channel (0), and a logical channel is
//! owned by the QMI client that opened it, so one client is kept for the
//! lifetime of the process.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::time::Duration;

const QMUX_MARKER: u8 = 0x01;

const SVC_CTL: u8 = 0x00;
const SVC_UIM: u8 = 0x0B;

const CTL_ALLOCATE_CID: u16 = 0x0022;
const CTL_RELEASE_CID: u16 = 0x0023;
const CTL_INTERNAL_PROXY_OPEN: u16 = 0xFF00;

const UIM_SEND_APDU: u16 = 0x003B;
const UIM_LOGICAL_CHANNEL: u16 = 0x003F;
const UIM_OPEN_LOGICAL_CHANNEL: u16 = 0x0042;

const SDU_FLAG_INDICATION: u8 = 0x04;

const TLV_RESULT: u8 = 0x02;

#[derive(Debug)]
pub enum QmiError {
    Io(io::Error),
    /// QMI result TLV reported failure: (result, error code).
    Protocol(u16, u16),
    Malformed(String),
}

impl fmt::Display for QmiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QmiError::Io(e) => write!(f, "io: {e}"),
            QmiError::Protocol(r, e) => write!(f, "QMI error {e} (result {r})"),
            QmiError::Malformed(m) => write!(f, "malformed QMI message: {m}"),
        }
    }
}

impl From<io::Error> for QmiError {
    fn from(e: io::Error) -> Self {
        QmiError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, QmiError>;

/// One TLV: type, value.
#[derive(Debug, Clone, PartialEq)]
pub struct Tlv(pub u8, pub Vec<u8>);

fn encode_tlvs(tlvs: &[Tlv]) -> Vec<u8> {
    let mut out = Vec::new();
    for Tlv(t, v) in tlvs {
        out.push(*t);
        out.extend_from_slice(&(v.len() as u16).to_le_bytes());
        out.extend_from_slice(v);
    }
    out
}

fn decode_tlvs(mut b: &[u8]) -> Result<Vec<Tlv>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        if b.len() < 3 {
            return Err(QmiError::Malformed("truncated TLV header".into()));
        }
        let len = u16::from_le_bytes([b[1], b[2]]) as usize;
        if b.len() < 3 + len {
            return Err(QmiError::Malformed("truncated TLV value".into()));
        }
        out.push(Tlv(b[0], b[3..3 + len].to_vec()));
        b = &b[3 + len..];
    }
    Ok(out)
}

/// Encode a complete QMUX frame (marker included).
pub fn encode_frame(service: u8, client: u8, txn: u16, msg_id: u16, tlvs: &[Tlv]) -> Vec<u8> {
    let body = encode_tlvs(tlvs);
    let mut sdu = vec![0x00]; // request
    if service == SVC_CTL {
        sdu.push(txn as u8);
    } else {
        sdu.extend_from_slice(&txn.to_le_bytes());
    }
    sdu.extend_from_slice(&msg_id.to_le_bytes());
    sdu.extend_from_slice(&(body.len() as u16).to_le_bytes());
    sdu.extend_from_slice(&body);

    let qmux_len = 5 + sdu.len(); // len(2) + flags + service + client + sdu
    let mut frame = vec![QMUX_MARKER];
    frame.extend_from_slice(&(qmux_len as u16).to_le_bytes());
    frame.extend_from_slice(&[0x00, service, client]);
    frame.extend_from_slice(&sdu);
    frame
}

/// A decoded frame from the modem.
#[derive(Debug, PartialEq)]
pub struct Message {
    pub service: u8,
    pub client: u8,
    pub sdu_flags: u8,
    pub txn: u16,
    pub msg_id: u16,
    pub tlvs: Vec<Tlv>,
}

impl Message {
    pub fn tlv(&self, t: u8) -> Option<&[u8]> {
        self.tlvs.iter().find(|x| x.0 == t).map(|x| x.1.as_slice())
    }

    /// Fail on a missing or non-success result TLV.
    pub fn check(&self) -> Result<()> {
        let r = self
            .tlv(TLV_RESULT)
            .ok_or_else(|| QmiError::Malformed("no result TLV".into()))?;
        if r.len() < 4 {
            return Err(QmiError::Malformed("short result TLV".into()));
        }
        let result = u16::from_le_bytes([r[0], r[1]]);
        let error = u16::from_le_bytes([r[2], r[3]]);
        if result != 0 {
            return Err(QmiError::Protocol(result, error));
        }
        Ok(())
    }
}

/// Decode a frame given everything after the marker byte.
pub fn decode_frame(after_marker: &[u8]) -> Result<Message> {
    if after_marker.len() < 5 {
        return Err(QmiError::Malformed("short QMUX header".into()));
    }
    let service = after_marker[3];
    let client = after_marker[4];
    let sdu = &after_marker[5..];
    let (sdu_flags, txn, rest) = if service == SVC_CTL {
        if sdu.len() < 2 {
            return Err(QmiError::Malformed("short CTL SDU".into()));
        }
        (sdu[0], sdu[1] as u16, &sdu[2..])
    } else {
        if sdu.len() < 3 {
            return Err(QmiError::Malformed("short SDU".into()));
        }
        (sdu[0], u16::from_le_bytes([sdu[1], sdu[2]]), &sdu[3..])
    };
    if rest.len() < 4 {
        return Err(QmiError::Malformed("short message header".into()));
    }
    let msg_id = u16::from_le_bytes([rest[0], rest[1]]);
    let len = u16::from_le_bytes([rest[2], rest[3]]) as usize;
    let body = rest
        .get(4..4 + len)
        .ok_or_else(|| QmiError::Malformed("truncated message body".into()))?;
    Ok(Message {
        service,
        client,
        sdu_flags,
        txn,
        msg_id,
        tlvs: decode_tlvs(body)?,
    })
}

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

/// A QMI connection holding one UIM client id.
pub struct Uim {
    io: Box<dyn Stream>,
    cid: u8,
    ctl_txn: u8,
    txn: u16,
}

impl Uim {
    /// Connect through libqmi's `qmi-proxy` (what ModemManager uses), so this
    /// process can share the device with it.
    pub fn connect_proxy(device: &str, timeout: Duration) -> Result<Self> {
        let addr = SocketAddr::from_abstract_name(b"qmi-proxy")?;
        let sock = UnixStream::connect_addr(&addr)?;
        sock.set_read_timeout(Some(timeout))?;
        let mut q = Self::new(Box::new(sock));
        q.ctl(
            CTL_INTERNAL_PROXY_OPEN,
            &[Tlv(0x01, device.as_bytes().to_vec())],
        )?;
        q.allocate()?;
        Ok(q)
    }

    /// Open the QMI character device directly (no proxy running).
    pub fn connect_device(device: &str) -> Result<Self> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(device)?;
        let mut q = Self::new(Box::new(f));
        q.allocate()?;
        Ok(q)
    }

    fn new(io: Box<dyn Stream>) -> Self {
        Uim {
            io,
            cid: 0,
            ctl_txn: 0,
            txn: 0,
        }
    }

    fn allocate(&mut self) -> Result<()> {
        let resp = self.ctl(CTL_ALLOCATE_CID, &[Tlv(0x01, vec![SVC_UIM])])?;
        let v = resp
            .tlv(0x01)
            .filter(|v| v.len() >= 2 && v[0] == SVC_UIM)
            .ok_or_else(|| QmiError::Malformed("allocate CID: no client id".into()))?;
        self.cid = v[1];
        Ok(())
    }

    pub fn cid(&self) -> u8 {
        self.cid
    }

    fn read_frame(&mut self) -> Result<Message> {
        let mut hdr = [0u8; 3];
        self.io.read_exact(&mut hdr)?;
        if hdr[0] != QMUX_MARKER {
            return Err(QmiError::Malformed(format!("bad marker {:#04x}", hdr[0])));
        }
        let len = u16::from_le_bytes([hdr[1], hdr[2]]) as usize;
        if len < 5 {
            return Err(QmiError::Malformed("QMUX length too short".into()));
        }
        let mut rest = vec![0u8; len - 2];
        self.io.read_exact(&mut rest)?;
        let mut whole = hdr[1..].to_vec();
        whole.extend_from_slice(&rest);
        decode_frame(&whole)
    }

    /// Send a request and wait for its response, skipping indications and
    /// traffic for other clients.
    fn transact(&mut self, service: u8, client: u8, txn: u16, msg_id: u16, tlvs: &[Tlv]) -> Result<Message> {
        self.io
            .write_all(&encode_frame(service, client, txn, msg_id, tlvs))?;
        loop {
            let m = self.read_frame()?;
            if m.service == service
                && m.client == client
                && m.msg_id == msg_id
                && m.txn == txn
                && m.sdu_flags & SDU_FLAG_INDICATION == 0
            {
                m.check()?;
                return Ok(m);
            }
        }
    }

    fn ctl(&mut self, msg_id: u16, tlvs: &[Tlv]) -> Result<Message> {
        self.ctl_txn = self.ctl_txn.wrapping_add(1).max(1);
        let txn = self.ctl_txn as u16;
        self.transact(SVC_CTL, 0, txn, msg_id, tlvs)
    }

    fn uim(&mut self, msg_id: u16, tlvs: &[Tlv]) -> Result<Message> {
        self.txn = self.txn.wrapping_add(1).max(1);
        let (txn, cid) = (self.txn, self.cid);
        self.transact(SVC_UIM, cid, txn, msg_id, tlvs)
    }

    /// Open a logical channel to the application `aid`; returns the channel id
    /// and the card's SELECT response (FCP template, possibly empty).
    pub fn open_channel(&mut self, slot: u8, aid: &[u8]) -> Result<(u8, Vec<u8>)> {
        let resp = self.uim(UIM_OPEN_LOGICAL_CHANNEL, &open_channel_tlvs(slot, aid))?;
        let ch = resp
            .tlv(0x10)
            .and_then(|v| v.first().copied())
            .ok_or_else(|| QmiError::Malformed("open channel: no channel id".into()))?;
        let fcp = resp
            .tlv(0x12)
            .and_then(|v| v.get(1..1 + *v.first()? as usize))
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        Ok((ch, fcp))
    }

    /// Send one APDU on `channel`; returns response data + SW1 SW2.
    pub fn send_apdu(&mut self, slot: u8, channel: u8, apdu: &[u8]) -> Result<Vec<u8>> {
        let resp = self.uim(UIM_SEND_APDU, &send_apdu_tlvs(slot, channel, apdu))?;
        let v = resp
            .tlv(0x10)
            .ok_or_else(|| QmiError::Malformed("send APDU: no response TLV".into()))?;
        if v.len() < 2 {
            return Err(QmiError::Malformed("send APDU: short response TLV".into()));
        }
        let n = u16::from_le_bytes([v[0], v[1]]) as usize;
        v.get(2..2 + n)
            .map(|d| d.to_vec())
            .ok_or_else(|| QmiError::Malformed("send APDU: truncated response".into()))
    }

    pub fn close_channel(&mut self, slot: u8, channel: u8) -> Result<()> {
        self.uim(UIM_LOGICAL_CHANNEL, &close_channel_tlvs(slot, channel))
            .map(|_| ())
    }

    pub fn release(&mut self) -> Result<()> {
        let cid = self.cid;
        self.ctl(CTL_RELEASE_CID, &[Tlv(0x01, vec![SVC_UIM, cid])])
            .map(|_| ())
    }
}

fn open_channel_tlvs(slot: u8, aid: &[u8]) -> Vec<Tlv> {
    let mut a = vec![aid.len() as u8];
    a.extend_from_slice(aid);
    vec![Tlv(0x10, a), Tlv(0x01, vec![slot])]
}

fn send_apdu_tlvs(slot: u8, channel: u8, apdu: &[u8]) -> Vec<Tlv> {
    let mut a = (apdu.len() as u16).to_le_bytes().to_vec();
    a.extend_from_slice(apdu);
    vec![Tlv(0x10, vec![channel]), Tlv(0x02, a), Tlv(0x01, vec![slot])]
}

fn close_channel_tlvs(slot: u8, channel: u8) -> Vec<Tlv> {
    vec![Tlv(0x11, vec![channel]), Tlv(0x01, vec![slot])]
}

#[cfg(test)]
mod tests {
    use super::*;

    // Frames captured from `qmicli --verbose-full` against the HMUF02-V5 modem.

    fn hex(s: &str) -> Vec<u8> {
        s.split(':').map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    #[test]
    fn encodes_proxy_open() {
        let f = encode_frame(
            SVC_CTL,
            0,
            1,
            CTL_INTERNAL_PROXY_OPEN,
            &[Tlv(0x01, b"/dev/wwan0qmi0".to_vec())],
        );
        assert_eq!(f, hex("01:1C:00:00:00:00:00:01:00:FF:11:00:01:0E:00:2F:64:65:76:2F:77:77:61:6E:30:71:6D:69:30"));
    }

    #[test]
    fn encodes_allocate_cid() {
        let f = encode_frame(SVC_CTL, 0, 2, CTL_ALLOCATE_CID, &[Tlv(0x01, vec![SVC_UIM])]);
        assert_eq!(f, hex("01:0F:00:00:00:00:00:02:22:00:04:00:01:01:00:0B"));
    }

    #[test]
    fn decodes_allocate_cid_response() {
        let f = hex("01:17:00:80:00:00:01:02:22:00:0C:00:02:04:00:00:00:00:00:01:02:00:0B:02");
        let m = decode_frame(&f[1..]).unwrap();
        m.check().unwrap();
        assert_eq!((m.txn, m.msg_id), (2, CTL_ALLOCATE_CID));
        assert_eq!(m.tlv(0x01), Some(&[0x0B, 0x02][..]));
    }

    #[test]
    fn encodes_open_logical_channel() {
        let aid = hex("A0:00:00:00:87:10:02:FF:F6:05:FF:89:00:00:01:FF");
        let f = encode_frame(SVC_UIM, 2, 1, UIM_OPEN_LOGICAL_CHANNEL, &open_channel_tlvs(1, &aid));
        assert_eq!(f, hex("01:24:00:00:0B:02:00:01:00:42:00:18:00:10:11:00:10:A0:00:00:00:87:10:02:FF:F6:05:FF:89:00:00:01:FF:01:01:00:01"));
    }

    #[test]
    fn decodes_open_logical_channel_response() {
        let f = hex("01:52:00:80:0B:02:02:01:00:42:00:46:00:02:04:00:00:00:00:00:12:33:00:32:62:30:82:02:78:21:83:02:AD:F1:84:10:A0:00:00:00:87:10:02:FF:F6:05:FF:89:00:00:01:FF:8A:01:05:8B:06:2F:06:00:03:01:03:C6:09:90:01:40:83:01:01:83:01:81:11:02:00:90:00:10:01:00:01");
        let m = decode_frame(&f[1..]).unwrap();
        m.check().unwrap();
        assert_eq!(m.tlv(0x10), Some(&[0x01][..]));
        assert_eq!(m.tlv(0x11), Some(&[0x90, 0x00][..]));
        assert_eq!(&m.tlv(0x12).unwrap()[..3], &[0x32, 0x62, 0x30]);
    }

    #[test]
    fn encodes_send_apdu() {
        let f = encode_frame(SVC_UIM, 2, 1, UIM_SEND_APDU, &send_apdu_tlvs(1, 1, &hex("01:F2:00:00:00")));
        assert_eq!(f, hex("01:1E:00:00:0B:02:00:01:00:3B:00:12:00:10:01:00:01:02:07:00:05:00:01:F2:00:00:00:01:01:00:01"));
    }

    #[test]
    fn decodes_send_apdu_response() {
        let f = hex("01:1A:00:80:0B:02:02:01:00:3B:00:0E:00:02:04:00:00:00:00:00:10:04:00:02:00:6D:00");
        let m = decode_frame(&f[1..]).unwrap();
        m.check().unwrap();
        assert_eq!(m.tlv(0x10), Some(&[0x02, 0x00, 0x6D, 0x00][..]));
    }

    #[test]
    fn encodes_close_channel() {
        let f = encode_frame(SVC_UIM, 2, 1, UIM_LOGICAL_CHANNEL, &close_channel_tlvs(1, 1));
        assert_eq!(f, hex("01:14:00:00:0B:02:00:01:00:3F:00:08:00:11:01:00:01:01:01:00:01"));
    }

    #[test]
    fn decodes_indication_flags() {
        let f = hex("01:1B:00:80:0B:02:04:01:00:43:00:0F:00:13:04:00:01:00:00:00:01:01:00:01:11:01:00:01");
        let m = decode_frame(&f[1..]).unwrap();
        assert_eq!(m.sdu_flags & SDU_FLAG_INDICATION, SDU_FLAG_INDICATION);
        assert_eq!(m.msg_id, 0x43);
    }

    #[test]
    fn error_result_is_reported() {
        let m = Message {
            service: SVC_UIM,
            client: 2,
            sdu_flags: 2,
            txn: 1,
            msg_id: UIM_SEND_APDU,
            tlvs: vec![Tlv(TLV_RESULT, vec![1, 0, 0x30, 0])],
        };
        assert!(matches!(m.check(), Err(QmiError::Protocol(1, 0x30))));
    }

    /// Scripted transport: replies are queued frames; an indication and a
    /// foreign-client frame precede the real response and must be skipped.
    struct Script {
        replies: io::Cursor<Vec<u8>>,
        written: Vec<u8>,
    }
    impl Read for Script {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.replies.read(b)
        }
    }
    impl Write for Script {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn transact_skips_indications() {
        let mut replies = hex("01:1B:00:80:0B:02:04:01:00:43:00:0F:00:13:04:00:01:00:00:00:01:01:00:01:11:01:00:01");
        replies.extend(hex("01:13:00:80:0B:02:02:01:00:3F:00:07:00:02:04:00:00:00:00:00"));
        let mut q = Uim::new(Box::new(Script {
            replies: io::Cursor::new(replies),
            written: vec![],
        }));
        q.cid = 2;
        q.close_channel(1, 1).unwrap();
    }
}
