//! Classifies AT command lines arriving on the pty and formats the replies for
//! the commands this bridge answers itself.

#[derive(Debug, PartialEq)]
pub enum Command {
    /// `AT+CSIM=<len>,"<hex>"`, APDU already hex-decoded.
    Csim(Vec<u8>),
    /// `AT+CSIM=<len>,"<hex>"` whose length or hex is invalid.
    CsimInvalid,
    /// `AT+CSIM=?`
    CsimTest,
    /// `AT+QCFG="ims"` (query).
    QcfgImsQuery,
    /// `AT+QCFG="ims",<n>[,...]` (set).
    QcfgImsSet,
    /// `AT+CFUN=<fun>[,<rst>]`: would take the radio (and the LTE data link)
    /// down on this modem, so it is answered locally.
    CfunSet(String),
    /// Forwarded as a different command (modem firmware quirk workaround).
    Rewrite(&'static str),
    /// `ATE0` / `ATE1`: forwarded, but tracked to echo locally-answered lines.
    Echo(bool),
    /// Anything else goes to the modem unchanged.
    Passthrough,
}

pub fn classify(line: &str) -> Command {
    let t = line.trim();
    let u = t.to_ascii_uppercase();
    match u.as_str() {
        "ATE0" => return Command::Echo(false),
        "ATE1" | "ATE" => return Command::Echo(true),
        "AT+CSIM=?" => return Command::CsimTest,
        // HIMI_U01 firmware answers AT+CGSN with the IMEI but never sends the
        // final OK; AT+GSN returns the same IMEI and terminates properly.
        "AT+CGSN" => return Command::Rewrite("AT+GSN"),
        _ => {}
    }
    if let Some(args) = u.strip_prefix("AT+CSIM=") {
        return parse_csim(args);
    }
    if let Some(args) = u.strip_prefix("AT+QCFG=") {
        let mut it = args.splitn(2, ',');
        if it.next().map(|a| a.trim_matches('"')) == Some("IMS") {
            return match it.next() {
                None => Command::QcfgImsQuery,
                Some(_) => Command::QcfgImsSet,
            };
        }
        return Command::Passthrough;
    }
    if let Some(args) = u.strip_prefix("AT+CFUN=") {
        if args != "?" && !args.contains(';') {
            return Command::CfunSet(args.to_string());
        }
    }
    Command::Passthrough
}

fn parse_csim(args: &str) -> Command {
    let Some((len, data)) = args.split_once(',') else {
        return Command::CsimInvalid;
    };
    let data = data.trim().trim_matches('"');
    match (len.trim().parse::<usize>(), hex_decode(data)) {
        (Ok(n), Some(apdu)) if n == data.len() && apdu.len() >= 4 => Command::Csim(apdu),
        _ => Command::CsimInvalid,
    }
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

pub fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// Reply to a locally-answered command: optional echo, info lines, final code.
pub fn reply(echo_of: Option<&str>, info: &[String], ok: bool) -> Vec<u8> {
    let mut s = String::new();
    if let Some(l) = echo_of {
        s.push_str(l);
        s.push('\r');
    }
    for i in info {
        s.push_str("\r\n");
        s.push_str(i);
        s.push_str("\r\n");
    }
    s.push_str(if ok { "\r\nOK\r\n" } else { "\r\nERROR\r\n" });
    s.into_bytes()
}

pub fn csim_info(resp: &[u8]) -> String {
    let h = hex_encode(resp);
    format!("+CSIM: {},\"{}\"", h.len(), h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_csim() {
        assert_eq!(
            classify(r#"AT+CSIM=14,"00A4000C023F00""#),
            Command::Csim(vec![0x00, 0xA4, 0x00, 0x0C, 0x02, 0x3F, 0x00])
        );
        assert_eq!(
            classify(r#"at+csim=10,"80f2000000""#),
            Command::Csim(vec![0x80, 0xF2, 0, 0, 0])
        );
        assert_eq!(classify(r#"AT+CSIM=12,"80F2000000""#), Command::CsimInvalid);
        assert_eq!(classify(r#"AT+CSIM=3,"80F""#), Command::CsimInvalid);
        assert_eq!(classify("AT+CSIM=?"), Command::CsimTest);
    }

    #[test]
    fn classifies_qcfg_ims() {
        assert_eq!(classify(r#"AT+QCFG="ims""#), Command::QcfgImsQuery);
        assert_eq!(classify(r#"AT+QCFG="ims",2"#), Command::QcfgImsSet);
        assert_eq!(classify(r#"AT+QCFG="usbnet""#), Command::Passthrough);
    }

    #[test]
    fn classifies_cfun() {
        assert_eq!(classify("AT+CFUN=0"), Command::CfunSet("0".into()));
        assert_eq!(classify("AT+CFUN=1,1"), Command::CfunSet("1,1".into()));
        assert_eq!(classify("AT+CFUN?"), Command::Passthrough);
        assert_eq!(classify("AT+CFUN=?"), Command::Passthrough);
    }

    #[test]
    fn rewrites_cgsn() {
        assert_eq!(classify("AT+CGSN"), Command::Rewrite("AT+GSN"));
        assert_eq!(classify("at+cgsn"), Command::Rewrite("AT+GSN"));
        assert_eq!(classify("AT+CGSN=1"), Command::Passthrough);
    }

    #[test]
    fn passes_through_the_rest() {
        for c in [
            "AT",
            "ATI",
            "AT+CIMI",
            "AT+COPS=3,2;+COPS?",
            "AT+CRSM=176,28589,0,0,4",
        ] {
            assert_eq!(classify(c), Command::Passthrough, "{c}");
        }
    }

    #[test]
    fn formats_csim_reply() {
        let r = reply(None, &[csim_info(&[0x90, 0x00])], true);
        assert_eq!(r, b"\r\n+CSIM: 4,\"9000\"\r\n\r\nOK\r\n");
        let r = reply(Some("AT+X"), &[], false);
        assert_eq!(r, b"AT+X\r\r\nERROR\r\n");
    }
}
