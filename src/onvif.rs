//! ONVIF (Profile S/T) client, the parts a HomeKit camera needs:
//! - WS-Discovery probe (find cameras on the LAN)
//! - device: GetSystemDateAndTime (clock offset for auth), GetDeviceInformation, GetCapabilities
//! - media: GetProfiles, GetStreamUri, GetSnapshotUri
//! - events: CreatePullPointSubscription + PullMessages (motion), Renew
//!
//! Auth: WS-Security UsernameToken with a password digest, its timestamp shifted to the camera's
//! clock (cameras reject tokens from "the future" or "the past"); HTTP Digest is retried
//! automatically by `httpc` when a camera asks for it instead.

use rand_core::{OsRng, RngCore};
use sha1::{Digest, Sha1};
use std::net::UdpSocket;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct Onvif {
    /// device service URL (http://host[:port]/onvif/device_service)
    device: String,
    user: String,
    pass: String,
    /// camera clock − our clock, seconds
    offset: i64,
    media: String,
    events: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub token: String,
    pub name: String,
    pub encoding: String,
    pub width: u32,
    pub height: u32,
    pub bitrate_kbps: u32,
}

// ------------------------------------------------------------------------------- helpers

/// "2026-11-18T19:25:50Z" from Unix seconds.
fn iso8601(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // civil from days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Unix seconds from y-m-d h:m:s (UTC).
fn unix(y: i64, m: i64, d: i64, hh: i64, mm: i64, ss: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe - 719468) * 86400 + hh * 3600 + mm * 60 + ss
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The UsernameToken header (password digest).
fn security(user: &str, pass: &str, created_unix: i64) -> String {
    let mut nonce = [0u8; 16];
    OsRng.fill_bytes(&mut nonce);
    let created = iso8601(created_unix);
    let mut h = Sha1::new();
    h.update(nonce);
    h.update(created.as_bytes());
    h.update(pass.as_bytes());
    let digest = crate::tlv::b64(&h.finalize());
    format!(
        r#"<Security s:mustUnderstand="1" xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"><UsernameToken><Username>{}</Username><Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</Password><Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{}</Nonce><Created xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">{created}</Created></UsernameToken></Security>"#,
        xml_escape(user),
        crate::tlv::b64(&nonce)
    )
}

fn envelope(header: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:wsa="http://www.w3.org/2005/08/addressing" xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2"><s:Header>{header}</s:Header><s:Body>{body}</s:Body></s:Envelope>"#
    )
}

/// Text of the first element with this local name.
pub fn text(doc: &roxmltree::Document, name: &str) -> Option<String> {
    doc.descendants().find(|n| n.tag_name().name() == name).and_then(|n| n.text()).map(|t| t.trim().to_string())
}

fn child<'a>(n: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    n.descendants().find(|c| c.tag_name().name() == name)
}

/// Put `user:pass@` into an rtsp:// or http:// URI (credentials percent-encoded).
pub fn with_credentials(uri: &str, user: &str, pass: &str) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
            .collect()
    };
    match uri.split_once("://") {
        Some((scheme, rest)) if !rest.split('/').next().unwrap_or("").contains('@') => {
            format!("{scheme}://{}:{}@{rest}", enc(user), enc(pass))
        }
        _ => uri.to_string(),
    }
}

// ------------------------------------------------------------------------------- client

impl Onvif {
    /// `url`: the camera's address (`http://host[:port]`) or its device service URL.
    pub fn connect(url: &str, user: &str, pass: &str) -> Result<Onvif, String> {
        let base = url.trim_end_matches('/');
        let device = if base.contains("/onvif/") { base.to_string() } else { format!("{base}/onvif/device_service") };
        let mut o = Onvif { device, user: user.into(), pass: pass.into(), offset: 0, media: String::new(), events: None };
        // clock first: unauthenticated by the standard
        if let Ok(doc) = o.call_raw(&o.device.clone(), "", "<tds:GetSystemDateAndTime/>") {
            let d = roxmltree::Document::parse(&doc).map_err(|e| e.to_string())?;
            if let Some(utc) = d.descendants().find(|n| n.tag_name().name() == "UTCDateTime") {
                let g = |n: &str| child(utc, n).and_then(|x| x.text()).and_then(|t| t.trim().parse::<i64>().ok()).unwrap_or(0);
                let cam = unix(g("Year"), g("Month"), g("Day"), g("Hour"), g("Minute"), g("Second"));
                if g("Year") > 2000 {
                    o.offset = cam - now();
                }
            }
        }
        let caps = o.call(&o.device.clone(), "<tds:GetCapabilities><tds:Category>All</tds:Category></tds:GetCapabilities>")?;
        let d = roxmltree::Document::parse(&caps).map_err(|e| e.to_string())?;
        let xaddr = |section: &str| {
            d.descendants().find(|n| n.tag_name().name() == section).and_then(|s| child(s, "XAddr")).and_then(|x| x.text()).map(|t| t.trim().to_string())
        };
        o.media = xaddr("Media").ok_or("ONVIF: no media service")?;
        o.events = xaddr("Events");
        Ok(o)
    }

    fn call_raw(&self, url: &str, header: &str, body: &str) -> Result<String, String> {
        let creds_url = with_credentials(url, &self.user, &self.pass);
        let xml = envelope(header, body);
        let r = crate::httpc::request("POST", &creds_url, Some("application/soap+xml; charset=utf-8"), xml.as_bytes(), Duration::from_secs(15))?;
        let text = String::from_utf8_lossy(&r.body).into_owned();
        if r.status != 200 {
            let reason = roxmltree::Document::parse(&text).ok().and_then(|d| text_of_fault(&d)).unwrap_or_default();
            return Err(format!("ONVIF: HTTP {} {reason}", r.status));
        }
        Ok(text)
    }

    fn call(&self, url: &str, body: &str) -> Result<String, String> {
        self.call_raw(url, &security(&self.user, &self.pass, now() + self.offset), body)
    }

    pub fn device_info(&self) -> Result<(String, String), String> {
        let x = self.call(&self.device, "<tds:GetDeviceInformation/>")?;
        let d = roxmltree::Document::parse(&x).map_err(|e| e.to_string())?;
        Ok((text(&d, "Manufacturer").unwrap_or_default(), text(&d, "Model").unwrap_or_default()))
    }

    pub fn profiles(&self) -> Result<Vec<Profile>, String> {
        let x = self.call(&self.media, "<trt:GetProfiles/>")?;
        let d = roxmltree::Document::parse(&x).map_err(|e| e.to_string())?;
        Ok(d.descendants()
            .filter(|n| n.tag_name().name() == "Profiles")
            .map(|p| {
                let venc = child(p, "VideoEncoderConfiguration");
                let num = |n: roxmltree::Node, name: &str| child(n, name).and_then(|x| x.text()).and_then(|t| t.trim().parse().ok()).unwrap_or(0);
                Profile {
                    token: p.attribute("token").unwrap_or("").to_string(),
                    name: child(p, "Name").and_then(|x| x.text()).unwrap_or("").to_string(),
                    encoding: venc.and_then(|v| child(v, "Encoding")).and_then(|x| x.text()).unwrap_or("").to_string(),
                    width: venc.and_then(|v| child(v, "Resolution")).map(|r| num(r, "Width")).unwrap_or(0),
                    height: venc.and_then(|v| child(v, "Resolution")).map(|r| num(r, "Height")).unwrap_or(0),
                    bitrate_kbps: venc.and_then(|v| child(v, "RateControl")).map(|r| num(r, "BitrateLimit")).unwrap_or(0),
                }
            })
            .filter(|p| !p.token.is_empty())
            .collect())
    }

    pub fn stream_uri(&self, profile: &str) -> Result<String, String> {
        let body = format!(
            "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>{}</trt:ProfileToken></trt:GetStreamUri>",
            xml_escape(profile)
        );
        let x = self.call(&self.media, &body)?;
        let d = roxmltree::Document::parse(&x).map_err(|e| e.to_string())?;
        text(&d, "Uri").ok_or_else(|| "ONVIF: no stream URI".into())
    }

    pub fn snapshot_uri(&self, profile: &str) -> Result<String, String> {
        let x = self.call(&self.media, &format!("<trt:GetSnapshotUri><trt:ProfileToken>{}</trt:ProfileToken></trt:GetSnapshotUri>", xml_escape(profile)))?;
        let d = roxmltree::Document::parse(&x).map_err(|e| e.to_string())?;
        text(&d, "Uri").ok_or_else(|| "ONVIF: no snapshot URI".into())
    }

    // --------------------------------------------------------------------------- events

    /// A pull-point subscription; returns its address.
    fn subscribe(&self) -> Result<String, String> {
        let events = self.events.as_ref().ok_or("ONVIF: the camera has no event service")?;
        let x = self.call(events, "<tev:CreatePullPointSubscription><tev:InitialTerminationTime>PT120S</tev:InitialTerminationTime></tev:CreatePullPointSubscription>")?;
        let d = roxmltree::Document::parse(&x).map_err(|e| e.to_string())?;
        let reference = d.descendants().find(|n| n.tag_name().name() == "SubscriptionReference").ok_or("ONVIF: no subscription")?;
        child(reference, "Address").and_then(|a| a.text()).map(|t| t.trim().to_string()).ok_or_else(|| "ONVIF: no subscription address".into())
    }

    /// Headers some cameras want on subscription calls (WS-Addressing).
    fn addressed(&self, address: &str, action: &str) -> String {
        format!(
            "{}<wsa:Action>{action}</wsa:Action><wsa:To>{}</wsa:To>",
            security(&self.user, &self.pass, now() + self.offset),
            xml_escape(address)
        )
    }

    /// One PullMessages call (waits up to `wait` on the camera): (topic, item name, value) triples.
    fn pull(&self, address: &str, wait: u64) -> Result<Vec<(String, String, String)>, String> {
        let body = format!("<tev:PullMessages><tev:Timeout>PT{wait}S</tev:Timeout><tev:MessageLimit>32</tev:MessageLimit></tev:PullMessages>");
        let x = self.call_raw(address, &self.addressed(address, "http://www.onvif.org/ver10/events/wsdl/PullPointSubscription/PullMessagesRequest"), &body)?;
        Ok(parse_notifications(&x))
    }

    fn renew(&self, address: &str) -> Result<(), String> {
        let body = "<wsnt:Renew><wsnt:TerminationTime>PT120S</wsnt:TerminationTime></wsnt:Renew>";
        self.call_raw(address, &self.addressed(address, "http://docs.oasis-open.org/wsn/bw-2/SubscriptionManager/RenewRequest"), body).map(|_| ())
    }

    /// Follow events for ever (re-subscribing on errors), calling `on(topic, name, value)`.
    pub fn follow(self: std::sync::Arc<Self>, on: impl Fn(&str, &str, &str) + Send + 'static) {
        std::thread::spawn(move || {
            let mut backoff = 2;
            loop {
                match self.subscribe() {
                    Ok(addr) => {
                        crate::log("onvif: following events".into());
                        backoff = 2;
                        let mut renewed = std::time::Instant::now();
                        loop {
                            match self.pull(&addr, 10) {
                                Ok(msgs) => msgs.iter().for_each(|(t, n, v)| on(t, n, v)),
                                Err(e) => {
                                    crate::log(format!("onvif: events: {e}"));
                                    break;
                                }
                            }
                            if renewed.elapsed() > Duration::from_secs(60) {
                                if let Err(e) = self.renew(&addr) {
                                    crate::log(format!("onvif: renew: {e}"));
                                    break;
                                }
                                renewed = std::time::Instant::now();
                            }
                        }
                    }
                    Err(e) => crate::log(format!("onvif: subscribe: {e}")),
                }
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
            }
        });
    }
}

fn text_of_fault(d: &roxmltree::Document) -> Option<String> {
    d.descendants().find(|n| n.tag_name().name() == "Text" || n.tag_name().name() == "faultstring").and_then(|n| n.text()).map(|t| t.trim().to_string())
}

/// NotificationMessage → (topic, first SimpleItem name, its value) from the message's Data.
pub fn parse_notifications(xml: &str) -> Vec<(String, String, String)> {
    let Ok(d) = roxmltree::Document::parse(xml) else { return vec![] };
    d.descendants()
        .filter(|n| n.tag_name().name() == "NotificationMessage")
        .filter_map(|m| {
            let topic = child(m, "Topic").and_then(|t| t.text()).unwrap_or("").trim();
            // strip namespace prefixes: "tns1:RuleEngine/CellMotionDetector/Motion" → "RuleEngine/CellMotionDetector/Motion"
            let topic: String = topic.split('/').map(|p| p.rsplit(':').next().unwrap_or(p)).collect::<Vec<_>>().join("/");
            let data = child(m, "Data")?;
            let item = data.descendants().find(|n| n.tag_name().name() == "SimpleItem")?;
            Some((topic, item.attribute("Name")?.to_string(), item.attribute("Value")?.to_string()))
        })
        .collect()
}

/// Whether an event means motion (`Some(true/false)`), for topics containing one of `keys`.
pub fn motion_value(topic: &str, value: &str, keys: &[String]) -> Option<bool> {
    let t = topic.to_ascii_lowercase();
    if !keys.iter().any(|k| t.contains(&k.to_ascii_lowercase())) {
        return None;
    }
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "active" => Some(true),
        "false" | "0" | "inactive" => Some(false),
        _ => None,
    }
}

// ------------------------------------------------------------------------------- discovery

/// WS-Discovery probe: (device service URL, scopes) of the cameras that answer within `wait`.
pub fn discover(wait: Duration) -> Vec<(String, String)> {
    let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)) else { return vec![] };
    let mut id = [0u8; 16];
    OsRng.fill_bytes(&mut id);
    let uuid: String = id.iter().map(|b| format!("{b:02x}")).collect();
    let probe = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl"><e:Header><w:MessageID>uuid:{}-{}-{}-{}-{}</w:MessageID><w:To e:mustUnderstand="true">urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To><w:Action e:mustUnderstand="true">http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action></e:Header><e:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></e:Body></e:Envelope>"#,
        &uuid[..8],
        &uuid[8..12],
        &uuid[12..16],
        &uuid[16..20],
        &uuid[20..]
    );
    let _ = sock.send_to(probe.as_bytes(), "239.255.255.250:3702");
    let _ = sock.set_read_timeout(Some(Duration::from_millis(300)));
    let start = std::time::Instant::now();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut buf = vec![0u8; 65536];
    while start.elapsed() < wait {
        if let Ok((n, _)) = sock.recv_from(&mut buf) {
            let x = String::from_utf8_lossy(&buf[..n]).into_owned();
            if let Ok(d) = roxmltree::Document::parse(&x) {
                let xaddrs = text(&d, "XAddrs").unwrap_or_default();
                let scopes = text(&d, "Scopes").unwrap_or_default();
                if let Some(url) = xaddrs.split_whitespace().find(|u| u.starts_with("http://")) {
                    if !out.iter().any(|(u, _)| u == url) {
                        out.push((url.to_string(), scopes));
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        let t = unix(2026, 11, 18, 19, 25, 50);
        assert_eq!(iso8601(t), "2026-11-18T19:25:50Z");
        assert_eq!(iso8601(unix(2024, 2, 29, 0, 0, 0)), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn password_digest_matches_the_spec_formula() {
        // the token's digest is base64(sha1(nonce + created + password))
        let h = security("admin", "secret", unix(2026, 1, 2, 3, 4, 5));
        assert!(h.contains("<Created xmlns=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd\">2026-01-02T03:04:05Z</Created>"));
        let fixed = h.replace("s:mustUnderstand", "mustUnderstand");
        let doc = roxmltree::Document::parse(&fixed).unwrap();
        let nonce = crate::tlv::unb64(&text(&doc, "Nonce").unwrap()).unwrap();
        let mut s = Sha1::new();
        s.update(&nonce);
        s.update(b"2026-01-02T03:04:05Z");
        s.update(b"secret");
        assert_eq!(text(&doc, "Password").unwrap(), crate::tlv::b64(&s.finalize()));
    }

    #[test]
    fn credentials_in_uris() {
        assert_eq!(with_credentials("rtsp://10.0.0.5:554/cam/realmonitor?channel=1&subtype=0", "admin", "p@ss"), "rtsp://admin:p%40ss@10.0.0.5:554/cam/realmonitor?channel=1&subtype=0");
        assert_eq!(with_credentials("rtsp://u:p@h/x", "a", "b"), "rtsp://u:p@h/x");
    }

    #[test]
    fn motion_notifications() {
        let x = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><tev:PullMessagesResponse xmlns:tev="http://www.onvif.org/ver10/events/wsdl"><wsnt:NotificationMessage><wsnt:Topic Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">tns1:VideoSource/MotionAlarm</wsnt:Topic><wsnt:Message><tt:Message UtcTime="2026-11-18T19:00:00Z" PropertyOperation="Changed"><tt:Source><tt:SimpleItem Name="Source" Value="VideoSourceToken"/></tt:Source><tt:Data><tt:SimpleItem Name="State" Value="true"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage><wsnt:NotificationMessage><wsnt:Topic>tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic><wsnt:Message><tt:Message><tt:Data><tt:SimpleItem Name="IsMotion" Value="false"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage></tev:PullMessagesResponse></s:Body></s:Envelope>"#;
        let n = parse_notifications(x);
        assert_eq!(n[0], ("VideoSource/MotionAlarm".into(), "State".into(), "true".into()));
        assert_eq!(n[1].0, "RuleEngine/CellMotionDetector/Motion");
        let keys = vec!["Motion".to_string()];
        assert_eq!(motion_value(&n[0].0, &n[0].2, &keys), Some(true));
        assert_eq!(motion_value(&n[1].0, &n[1].2, &keys), Some(false));
        assert_eq!(motion_value("Device/Trigger/DigitalInput", "true", &keys), None);
    }
}
