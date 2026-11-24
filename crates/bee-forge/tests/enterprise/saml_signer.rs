//! A test identity provider: signs SAML responses with a fixed RSA test key (generated with
//! `openssl genpkey`, used only here) so the SSO flows can be tested end to end.
#![allow(dead_code)]

use bee_forge::enterprise::saml::{b64_decode, b64_encode, exc_c14n, format_instant};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use roxmltree::Document;

/// The test IdP's entity id.
pub const IDP_ENTITY: &str = "https://idp.example.com/metadata";
/// The test IdP's SSO URL.
pub const IDP_SSO: &str = "https://idp.example.com/sso";

const KEY_PKCS8_B64: &str = "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC0gdpuKZydF+riziRSO5GDGyiIZ2TQ93HA1FG8xeo4Y8+R9Yb8W2Nrpz73/wXtjNjzm+OVR6Rh/GP1eeiuaGzQsRShcR8/fBjgcpap0fCNYq9Ca+OKgpx7ISb/m5fPr5rbPLkTRVuN5cw6e1YcFoZICn71E53daUC+HFzwGiu2GnVijZqbrjjZPSADhdKczd7hAX6R1TrAKEieiuqs47MBS1Gusl1Lqj0kmmxVUMkJnyC9z9vV2JRCkcmtwns4r5Gan3r0vHGN1W+bZ/fe6Q6mrZ/SRMuxE4rsRryhpk/qcs1Sbd+zoBrLYCrP//wIYStHHPo063dF6uR3EDE69UG7AgMBAAECggEAOhpYXMCBPKHvxizzzRKGnsyVqf6aGZek4pyVaO3OHHOoY4PEfSPPp13w63QIDPSa+yNxSOKEO9Q5utEQgF91s2Fk0iivFcjU2x6Xd7dvtpsO7hL7beDoDMxZmMqVPdF5R9vOeTEIrO7M5ABCcg0gET1rq2iylowkPm2zzselzVpBU+Q4EngJdOELs/w3kspvRrVEjZgMBujbE74nQx0ln697eGaUiVaRYRGQ0usZUb3vtHwFsBwdCpavl8UrSbsI9921foyk+XfSAASimzBdneJ6Jx5f85PcMxuzOJWmlXg+HGe1mAqpR6GUZF+t49T9TONYgbs/rDDqEz+eNfAHzQKBgQDjmp/WKzBG7JlDxbjcObx0SRjBb8/4En6gKPgUIESrqtjrpCfXI9IzBZU8wObcBAAPafY/TuzqcGcpEGi3tpl3sHCDu4rg9qBnWpiV2CmbdElwpM/m+3QrTE0yp1vcDiWbU7VhMD6yvokLTjTmY0/xbxmDAS+W3seT8zhZTt8eNwKBgQDLBwUDT2EnqqFRJXbbFR3tUzwo4LN9zVGtZziG9nwInreg56IiTrZDrupFZjD/ECCeXWXTmGp+Vnf/8zRi5O4vk9UpAukoyLWhWpuquAha569z9Y+mnq1QSUmcFIEuQdPV2ZbAhSMTYPIWYDIleBVO0KhkPWDbZEj8O8Nj0TEWnQKBgQDEyfRY4I6lQ/xgNJxZDTJ7+JeG6H4QJy2EnOehvySgqN9VHmG0Ciy8IJ+gH8l7gZnrudpHcp41NWp8fsuLzD101wDxZT32tEaMFjDHnlabKjg1RMrdE5ylsxssKLer1C9/1SiGF4ieU096juv+Ju//9+c+9c+HyqcUrL6TuTYAkQKBgQClltgT1w5al+RylqfWD+cUxapvfLVoialwUJveF/MZ5LnIVqP6b1Bg66zPZYXYwoKYJ30Bk3JoEIZXDzlO34EU9PIKPQGF7g2VyJzmJKtL2WNUpJfBQfIBZYTP9DlYTQBFju5Rn0T4bT2F2neL7+ERI0X0W8s/pQ+SCfxW0SvEUQKBgBr8C0c5saX9XDJq//ccZGuBZLSrtKj67EaSKrVdMupiurdf3IgbiwFV6fbQJqY/tSX5VeaOYACIdBm5PIRhI1H/4M+yFuFwnbWY2u3Q5mlc7vpWy1y/Rv8oBLeA7TuESO277HyiTu8pdsbQbT1XetJjyV3Rh79iDprrHgd0OncW";

/// The matching self-signed certificate (valid 2026-10-18 .. 2126-09-24).
pub const CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIC6DCCAdCgAwIBAgIUIzYHz4Yi+xs3ABoHtFNwH3slsg8wDQYJKoZIhvcNAQEL
BQAwEzERMA8GA1UEAwwIdGVzdC1pZHAwIBcNMjYxMDE4MTAzODEzWhgPMjEyNjA5
MjQxMDM4MTNaMBMxETAPBgNVBAMMCHRlc3QtaWRwMIIBIjANBgkqhkiG9w0BAQEF
AAOCAQ8AMIIBCgKCAQEAtIHabimcnRfq4s4kUjuRgxsoiGdk0PdxwNRRvMXqOGPP
kfWG/Ftja6c+9/8F7YzY85vjlUekYfxj9Xnormhs0LEUoXEfP3wY4HKWqdHwjWKv
QmvjioKceyEm/5uXz6+a2zy5E0VbjeXMOntWHBaGSAp+9ROd3WlAvhxc8Borthp1
Yo2am6442T0gA4XSnM3e4QF+kdU6wChInorqrOOzAUtRrrJdS6o9JJpsVVDJCZ8g
vc/b1diUQpHJrcJ7OK+Rmp969LxxjdVvm2f33ukOpq2f0kTLsROK7Ea8oaZP6nLN
Um3fs6Aay2Aqz//8CGErRxz6NOt3RerkdxAxOvVBuwIDAQABozIwMDAdBgNVHQ4E
FgQUHNqVuzaZRAHw2vWIFcZMVq9jKV4wDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG
9w0BAQsFAAOCAQEAmUlfHKVqYhqgChUVOpjpWDCZpwzGgaIWWyWMjXrHvWWQdhlz
zHXGkcL80IPNLUJu/+T+ZJiX3GV+l9whexhxG1TtVycqYk949c7BxDCKVs3OlnQI
6aslll9uiOiidT+77FnWYet0nOsQCcIX/ufklCnucwSQr9uyFowDnI9qL+uj8u7v
lRC9gRVBTaxnBdSIE1QtW7+bk9McdCGa7d9RJMcT3AhMiry9CFtGczWT6BVuNMHa
HZW32VY9EfZNcBgKW1Kgsf77jIf8C4IH6rurzrN8FJcpzrbMRvOJBTg2/Qx3eNzH
xy46+07lnSZDzfdmcm6Flj+9wUOHKIBLCxt4Og==
-----END CERTIFICATE-----
";

/// What a response says.
#[derive(Clone, Debug)]
pub struct Reply {
    /// The request this answers (`None`: IdP-initiated).
    pub in_response_to: Option<String>,
    /// Subject NameID.
    pub name_id: String,
    /// The `groups` attribute values.
    pub groups: Vec<String>,
    /// An email attribute, if any.
    pub email: Option<String>,
    /// The SP's ACS URL.
    pub acs: String,
    /// The audience (SP entity id).
    pub audience: String,
    /// Issuer.
    pub issuer: String,
    /// Issue time (unix seconds); the window is `now - 60s .. now + 300s`.
    pub now: u64,
    /// The assertion id.
    pub assertion_id: String,
    /// When the IdP session ends, if stated.
    pub session_not_on_or_after: Option<u64>,
}

impl Reply {
    /// A reply for `name_id` at `now` to the SP of `scope_name` (an org).
    pub fn for_org(org: &str, name_id: &str, now_ms: u64, in_response_to: Option<&str>) -> Reply {
        let entity = format!("https://hub.example.com/saml/orgs/{org}");
        Reply {
            in_response_to: in_response_to.map(str::to_string),
            name_id: name_id.into(),
            groups: Vec::new(),
            email: None,
            acs: format!("{entity}/acs"),
            audience: entity,
            issuer: IDP_ENTITY.into(),
            now: now_ms / 1000,
            assertion_id: format!("_a{}", rand_hex()),
            session_not_on_or_after: None,
        }
    }
}

fn rand_hex() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    format!("{:016x}", N.fetch_add(1, Ordering::SeqCst))
}

/// The test IdP's metadata XML.
pub fn metadata() -> String {
    let body: String = CERT_PEM
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();
    format!(
        "<md:EntityDescriptor xmlns:md=\"urn:oasis:names:tc:SAML:2.0:metadata\" xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\" entityID=\"{IDP_ENTITY}\">\
<md:IDPSSODescriptor protocolSupportEnumeration=\"urn:oasis:names:tc:SAML:2.0:protocol\">\
<md:KeyDescriptor use=\"signing\"><ds:KeyInfo><ds:X509Data><ds:X509Certificate>{body}</ds:X509Certificate></ds:X509Data></ds:KeyInfo></md:KeyDescriptor>\
<md:SingleSignOnService Binding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect\" Location=\"{IDP_SSO}\"/>\
</md:IDPSSODescriptor></md:EntityDescriptor>"
    )
}

/// Builds and signs a response (the assertion is signed, enveloped, RSA-SHA256, exclusive
/// c14n); returns it base64 encoded as the IdP would POST it.
pub fn sign(reply: &Reply) -> String {
    let xml = build(reply);
    b64_encode(sign_xml(&xml).as_bytes())
}

/// The unsigned response document of a reply, with signature placeholders.
fn build(r: &Reply) -> String {
    let t0 = format_instant(r.now);
    let nb = format_instant(r.now.saturating_sub(60));
    let na = format_instant(r.now + 300);
    let irt = r
        .in_response_to
        .as_deref()
        .map(|i| format!(" InResponseTo=\"{i}\""))
        .unwrap_or_default();
    let mut attrs = String::new();
    if !r.groups.is_empty() {
        attrs.push_str("<saml:Attribute Name=\"groups\">");
        for g in &r.groups {
            attrs.push_str(&format!("<saml:AttributeValue>{g}</saml:AttributeValue>"));
        }
        attrs.push_str("</saml:Attribute>");
    }
    if let Some(e) = &r.email {
        attrs.push_str(&format!(
            "<saml:Attribute Name=\"email\"><saml:AttributeValue>{e}</saml:AttributeValue></saml:Attribute>"
        ));
    }
    let session_end = r
        .session_not_on_or_after
        .map(|s| format!(" SessionNotOnOrAfter=\"{}\"", format_instant(s)))
        .unwrap_or_default();
    format!(
        "<samlp:Response xmlns:samlp=\"urn:oasis:names:tc:SAML:2.0:protocol\" ID=\"_r{id}\"{irt} IssueInstant=\"{t0}\" Version=\"2.0\" Destination=\"{acs}\">\
<saml:Issuer xmlns:saml=\"urn:oasis:names:tc:SAML:2.0:assertion\">{issuer}</saml:Issuer>\
<samlp:Status><samlp:StatusCode Value=\"urn:oasis:names:tc:SAML:2.0:status:Success\"/></samlp:Status>\
<saml:Assertion xmlns:saml=\"urn:oasis:names:tc:SAML:2.0:assertion\" ID=\"{id}\" IssueInstant=\"{t0}\" Version=\"2.0\">\
<saml:Issuer>{issuer}</saml:Issuer>\
<ds:Signature xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\"><ds:SignedInfo>\
<ds:CanonicalizationMethod Algorithm=\"http://www.w3.org/2001/10/xml-exc-c14n#\"/>\
<ds:SignatureMethod Algorithm=\"http://www.w3.org/2001/04/xmldsig-more#rsa-sha256\"/>\
<ds:Reference URI=\"#{id}\"><ds:Transforms>\
<ds:Transform Algorithm=\"http://www.w3.org/2000/09/xmldsig#enveloped-signature\"/>\
<ds:Transform Algorithm=\"http://www.w3.org/2001/10/xml-exc-c14n#\"/></ds:Transforms>\
<ds:DigestMethod Algorithm=\"http://www.w3.org/2001/04/xmlenc#sha256\"/>\
<ds:DigestValue>@@DIGEST@@</ds:DigestValue></ds:Reference></ds:SignedInfo>\
<ds:SignatureValue>@@SIGNATURE@@</ds:SignatureValue></ds:Signature>\
<saml:Subject><saml:NameID>{name}</saml:NameID>\
<saml:SubjectConfirmation Method=\"urn:oasis:names:tc:SAML:2.0:cm:bearer\">\
<saml:SubjectConfirmationData{irt} NotOnOrAfter=\"{na}\" Recipient=\"{acs}\"/></saml:SubjectConfirmation></saml:Subject>\
<saml:Conditions NotBefore=\"{nb}\" NotOnOrAfter=\"{na}\"><saml:AudienceRestriction><saml:Audience>{aud}</saml:Audience></saml:AudienceRestriction></saml:Conditions>\
<saml:AuthnStatement AuthnInstant=\"{t0}\" SessionIndex=\"_s{id}\"{session_end}><saml:AuthnContext><saml:AuthnContextClassRef>urn:oasis:names:tc:SAML:2.0:ac:classes:PasswordProtectedTransport</saml:AuthnContextClassRef></saml:AuthnContext></saml:AuthnStatement>\
<saml:AttributeStatement>{attrs}</saml:AttributeStatement>\
</saml:Assertion></samlp:Response>",
        id = r.assertion_id,
        acs = r.acs,
        issuer = r.issuer,
        name = r.name_id,
        aud = r.audience,
    )
}

/// Fills in the digest and signature of the (single) assertion signature of `xml`.
pub fn sign_xml(xml: &str) -> String {
    // Pass 1: the digest of the assertion without its signature.
    let digest = {
        let doc = Document::parse(xml).expect("test response parses");
        let sig = doc
            .descendants()
            .find(|n| n.has_tag_name("Signature"))
            .expect("signature");
        let assertion = sig.parent().expect("assertion");
        let c = exc_c14n(&doc, assertion, Some(sig), &[]).expect("c14n");
        ring::digest::digest(&ring::digest::SHA256, c.as_bytes())
    };
    let with_digest = xml.replace("@@DIGEST@@", &b64_encode(digest.as_ref()));
    // Pass 2: the signature over the canonical SignedInfo.
    let signature = {
        let doc = Document::parse(&with_digest).expect("test response parses");
        let si = doc
            .descendants()
            .find(|n| n.has_tag_name("SignedInfo"))
            .expect("signed info");
        let c = exc_c14n(&doc, si, None, &[]).expect("c14n");
        let der = b64_decode(KEY_PKCS8_B64).expect("key");
        let key = RsaKeyPair::from_pkcs8(&der).expect("test key");
        let mut sig = vec![0u8; key.public().modulus_len()];
        key.sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            c.as_bytes(),
            &mut sig,
        )
        .expect("sign");
        sig
    };
    with_digest.replace("@@SIGNATURE@@", &b64_encode(&signature))
}

/// The RelayState a redirect URL carries.
pub fn relay_state_of(url: &str) -> String {
    url.split("RelayState=")
        .nth(1)
        .unwrap_or("")
        .split('&')
        .next()
        .unwrap_or("")
        .to_string()
}
