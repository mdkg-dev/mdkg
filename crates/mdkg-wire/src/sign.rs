//! The client half of the signing profile (S5, S41): RFC 9421 HTTP
//! Message Signatures with the label `mdkg`, and the person signature
//! `mdkg-person` over the machine signature's members.
//!
//! `serve` parses and verifies (L2); the base built here is the same base,
//! byte for byte:
//!
//! ```text
//! "@method": POST
//! "@path": /v1/leases
//! "@query": ?
//! "mdkg-graph": <graph id>
//! "mdkg-boot": <boot id>
//! "mdkg-actor": <actor>
//! "content-digest": sha-256=:…:
//! "idempotency-key": <key>
//! "@signature-params": ("@method" … );created=…;keyid="…";nonce="…";alg="ed25519";tag="mdkg-v1"
//! ```
//!
//! Covered: always `@method`, `@path`, `@query`, `mdkg-graph` and
//! `mdkg-boot`; then each of `mdkg-actor`, `content-digest`,
//! `idempotency-key` and `mdkg-turn` that the request carries, in that
//! order. A signed header appears once; a repeat is refused, never
//! combined.

use mdkg_format::{Code, Diagnostic};
use mdkg_ops::wire::{Signer, headers, profile};

use crate::{b64, digest};

/// Covered by every machine signature.
pub const ALWAYS: [&str; 5] = ["@method", "@path", "@query", "mdkg-graph", "mdkg-boot"];
/// Covered whenever the request carries them.
pub const IF_PRESENT: [&str; 4] = [
    "mdkg-actor",
    "content-digest",
    "idempotency-key",
    "mdkg-turn",
];

/// A FIDO2 person key's `alg` values (S41): a WebAuthn-style assertion
/// over the person signature's base. See [`crate::person`].
pub const ALG_FIDO2_ES256: &str = "fido2-es256";
pub const ALG_FIDO2_ED25519: &str = "fido2-ed25519";

fn refused(message: impl Into<String>) -> Box<Diagnostic> {
    Box::new(Diagnostic::error(Code::ApiUnauthenticated, message))
}

/// A request a client is about to send: what its signatures cover. Header
/// names keep the case they're given; lookups ignore case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    /// `GET`, `POST`.
    pub method: String,
    /// The path as it goes on the request line, percent-encoded: `@path`.
    pub path: String,
    /// The query without its `?`; `None` for none.
    pub query: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A header name RFC 9110 allows (a token), and a value with no control
/// characters but tab, so nothing a caller passes can split the head.
pub(crate) fn header_ok(name: &str, value: &str) -> bool {
    let token = |c: u8| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c);
    !name.is_empty()
        && name.bytes().all(token)
        && value
            .bytes()
            .all(|b| b == b'\t' || (b >= 0x20 && b != 0x7f))
        && value.trim_matches([' ', '\t']) == value
}

impl Outgoing {
    pub fn new(method: impl Into<String>, path: impl Into<String>, query: Option<String>) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            query,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Adds a header. Refuses names that aren't tokens, values with
    /// control characters or surrounding whitespace, and a second copy of
    /// any header.
    pub fn header(
        &mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<&mut Self, Box<Diagnostic>> {
        let (name, value) = (name.into(), value.into());
        if !header_ok(&name, &value) {
            return Err(refused(format!(
                "the header {name:?} can't be sent as given"
            )));
        }
        if self.get(&name).is_some() {
            return Err(refused(format!("the header {name} is set twice")));
        }
        self.headers.push((name, value));
        Ok(self)
    }

    /// The body, with its `Content-Digest` (RFC 9530). An empty body
    /// carries no digest.
    pub fn body(&mut self, body: Vec<u8>) -> Result<&mut Self, Box<Diagnostic>> {
        self.headers
            .retain(|(n, _)| !n.eq_ignore_ascii_case(headers::CONTENT_DIGEST));
        if !body.is_empty() {
            let d = digest::content_digest(&body);
            self.header(headers::CONTENT_DIGEST, d)?;
        }
        self.body = body;
        Ok(self)
    }

    /// The one value of `name`, ignoring case.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn set(&mut self, name: &str, value: String) {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        self.headers.push((name.to_owned(), value));
    }
}

/// RFC 8941's sf-string: printable ASCII, `\` and `"` escaped.
pub(crate) fn sf_string(s: &str) -> Result<String, Box<Diagnostic>> {
    if !s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Err(refused("a signature parameter isn't printable ASCII"));
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

/// The value one covered component has in `req`, as RFC 9421 section 2
/// derives it.
fn component_value<'a>(req: &'a Outgoing, name: &str) -> Option<std::borrow::Cow<'a, str>> {
    use std::borrow::Cow;
    match name {
        "@method" => Some(Cow::Borrowed(req.method.as_str())),
        "@path" if req.path.is_empty() => Some(Cow::Borrowed("/")),
        "@path" => Some(Cow::Borrowed(req.path.as_str())),
        "@query" => Some(Cow::Owned(format!(
            "?{}",
            req.query.as_deref().unwrap_or("")
        ))),
        "@authority" => req.get("host").map(|h| Cow::Owned(h.to_ascii_lowercase())),
        header => req.get(header).map(Cow::Borrowed),
    }
}

/// RFC 9421's signature base for `components`, then
/// `"@signature-params": <params>`. Every component must be present.
pub fn signature_base(
    req: &Outgoing,
    components: &[&str],
    params: &str,
) -> Result<String, Box<Diagnostic>> {
    let mut out = String::new();
    for c in components {
        let value = component_value(req, c)
            .ok_or_else(|| refused(format!("the covered component {c} is missing")))?;
        if value.contains(['\r', '\n']) {
            return Err(refused(format!("the covered component {c} spans lines")));
        }
        out.push_str(&format!("\"{c}\": {value}\n"));
    }
    out.push_str("\"@signature-params\": ");
    out.push_str(params);
    Ok(out)
}

/// The machine signature's covered components for `req`, in profile order.
pub fn covered(req: &Outgoing) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = ALWAYS.to_vec();
    out.extend(IF_PRESENT.iter().filter(|c| req.get(c).is_some()));
    out
}

fn inner_list(components: &[&str]) -> Result<String, Box<Diagnostic>> {
    let items: Result<Vec<String>, _> = components.iter().map(|c| sf_string(c)).collect();
    Ok(format!("({})", items?.join(" ")))
}

/// Signs `req` as the machine `signer` (S5): sets `Signature-Input` and
/// `Signature`, with the label `mdkg`. `created` is Unix seconds on the
/// skew-corrected clock (S7); `nonce` is 16 fresh random bytes.
///
/// Refuses a request without `Mdkg-Graph` or `Mdkg-Boot`, a body without
/// its digest, and a signer whose `alg` isn't the profile's.
pub fn sign_machine(
    req: &mut Outgoing,
    signer: &dyn Signer,
    created: i64,
    nonce: [u8; 16],
) -> Result<(), Box<Diagnostic>> {
    if signer.alg() != profile::ALG {
        return Err(refused("a machine signs with ed25519 only"));
    }
    for h in [headers::GRAPH, headers::BOOT] {
        if req.get(h).is_none() {
            return Err(refused(format!("a signed request needs {h}")));
        }
    }
    match req.get(headers::CONTENT_DIGEST) {
        Some(d) if !digest::matches(d, &req.body) => {
            return Err(refused("Content-Digest doesn't match the body"));
        }
        None if !req.body.is_empty() => {
            return Err(refused("a body needs its Content-Digest"));
        }
        _ => {}
    }
    if req.get(headers::SIGNATURE_INPUT).is_some()
        || req.get(headers::SIGNATURE).is_some()
        || req.get(headers::PERSON).is_some()
    {
        return Err(refused("the request is already signed"));
    }
    let components = covered(req);
    let params = format!(
        "{};created={created};keyid={};nonce={};alg={};tag={}",
        inner_list(&components)?,
        sf_string(signer.key_id())?,
        sf_string(&b64::encode_url(&nonce))?,
        sf_string(profile::ALG)?,
        sf_string(profile::TAG)?,
    );
    let base = signature_base(req, &components, &params)?;
    let sig = signer.sign(base.as_bytes())?;
    if sig.0.len() != 64 {
        return Err(refused("an Ed25519 signature is 64 bytes"));
    }
    req.set(
        headers::SIGNATURE_INPUT,
        format!("{}={params}", profile::LABEL),
    );
    req.set(
        headers::SIGNATURE,
        format!("{}=:{}:", profile::LABEL, b64::encode(&sig.0)),
    );
    Ok(())
}

/// The person signature's base (S41): the machine signature's two
/// members, then the person signature's own parameters.
pub fn person_base(machine_signature: &str, machine_input: &str, person_params: &str) -> String {
    format!(
        "\"signature\";key=\"{l}\": {machine_signature}\n\"signature-input\";key=\"{l}\": {machine_input}\n\"@signature-params\": {person_params}",
        l = profile::LABEL,
    )
}

/// The person signature's covered components: the machine signature's
/// `Signature` and `Signature-Input` members.
pub fn person_components() -> String {
    format!(
        "(\"signature\";key=\"{l}\" \"signature-input\";key=\"{l}\")",
        l = profile::LABEL
    )
}

/// Adds a person's signature (label `mdkg-person`) over `req`'s machine
/// signature, and `Mdkg-Person` naming the key (S41). Call it after
/// [`sign_machine`]; the person signature covers the machine signature's
/// whole base, nonce and boot id with it, so it can't be lifted onto
/// another request.
pub fn sign_person(
    req: &mut Outgoing,
    person: &dyn Signer,
    created: i64,
) -> Result<(), Box<Diagnostic>> {
    let alg = person.alg();
    if ![
        profile::ALG,
        profile::ALG_P256,
        ALG_FIDO2_ES256,
        ALG_FIDO2_ED25519,
    ]
    .contains(&alg)
    {
        return Err(refused(format!("{alg} isn't a person key algorithm")));
    }
    let label = format!("{}=", profile::LABEL);
    let machine_input = req
        .get(headers::SIGNATURE_INPUT)
        .and_then(|v| v.strip_prefix(&label))
        .filter(|v| !v.contains(", "))
        .ok_or_else(|| refused("sign the machine signature first, and only once"))?
        .to_owned();
    let machine_signature = req
        .get(headers::SIGNATURE)
        .and_then(|v| v.strip_prefix(&label))
        .filter(|v| !v.contains(", "))
        .ok_or_else(|| refused("sign the machine signature first, and only once"))?
        .to_owned();
    let params = format!(
        "{};created={created};keyid={};alg={};tag={}",
        person_components(),
        sf_string(person.key_id())?,
        sf_string(alg)?,
        sf_string(profile::TAG)?,
    );
    let base = person_base(&machine_signature, &machine_input, &params);
    let sig = person.sign(base.as_bytes())?;
    req.set(
        headers::SIGNATURE_INPUT,
        format!(
            "{}={machine_input}, {}={params}",
            profile::LABEL,
            profile::PERSON_LABEL
        ),
    );
    req.set(
        headers::SIGNATURE,
        format!(
            "{}={machine_signature}, {}=:{}:",
            profile::LABEL,
            profile::PERSON_LABEL,
            b64::encode(&sig.0)
        ),
    );
    if !header_ok(headers::PERSON, person.key_id()) {
        return Err(refused("the person key id can't be sent as a header"));
    }
    req.set(headers::PERSON, person.key_id().to_owned());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{Ed25519, MachineKey};
    use mdkg_ops::wire::Signature;

    type Tamper = Box<dyn Fn(&mut Outgoing)>;

    fn signed_write() -> (Outgoing, MachineKey) {
        let key = MachineKey::from_parts("018f0000-0000-7000-8000-000000000001", [9u8; 32]);
        let mut req = Outgoing::new("POST", "/v1/leases", None);
        req.header("Mdkg-Graph", "g-1").unwrap();
        req.header("Mdkg-Boot", "boot-1").unwrap();
        req.header("Mdkg-Actor", "agent-a").unwrap();
        req.header("Idempotency-Key", "018f0000-0000-7000-8000-0000000000aa")
            .unwrap();
        req.header("Content-Type", "application/json").unwrap();
        req.body(br#"{"scope":"src/a.rs"}"#.to_vec()).unwrap();
        sign_machine(&mut req, &key, 1_793_290_515, [1u8; 16]).unwrap();
        (req, key)
    }

    fn sig_bytes(req: &Outgoing, label: &str) -> Vec<u8> {
        let v = req.get("signature").unwrap();
        let member = v
            .split(", ")
            .find_map(|m| m.strip_prefix(&format!("{label}=")))
            .unwrap();
        b64::decode(member.trim_matches(':')).unwrap()
    }

    #[test]
    fn the_machine_base_is_the_profiles() {
        let (req, key) = signed_write();
        let input = req.get("signature-input").unwrap();
        let nonce = b64::encode_url(&[1u8; 16]);
        let params = format!(
            "(\"@method\" \"@path\" \"@query\" \"mdkg-graph\" \"mdkg-boot\" \"mdkg-actor\" \"content-digest\" \"idempotency-key\");created=1793290515;keyid=\"018f0000-0000-7000-8000-000000000001\";nonce=\"{nonce}\";alg=\"ed25519\";tag=\"mdkg-v1\""
        );
        assert_eq!(input, format!("mdkg={params}"));
        // The base, written out by hand.
        let expected = format!(
            "\"@method\": POST\n\"@path\": /v1/leases\n\"@query\": ?\n\"mdkg-graph\": g-1\n\"mdkg-boot\": boot-1\n\"mdkg-actor\": agent-a\n\"content-digest\": {}\n\"idempotency-key\": 018f0000-0000-7000-8000-0000000000aa\n\"@signature-params\": {params}",
            digest::content_digest(br#"{"scope":"src/a.rs"}"#)
        );
        let sig: [u8; 64] = sig_bytes(&req, "mdkg").try_into().unwrap();
        assert!(Ed25519.verify(&key.public_key(), expected.as_bytes(), &sig));
    }

    #[test]
    fn a_read_covers_only_what_it_carries() {
        let key = MachineKey::from_parts("m", [3u8; 32]);
        let mut req = Outgoing::new("GET", "/v1/events", Some("after=c-1".into()));
        req.header("Mdkg-Graph", "g").unwrap();
        req.header("Mdkg-Boot", "b").unwrap();
        sign_machine(&mut req, &key, 5, [2u8; 16]).unwrap();
        let input = req.get("signature-input").unwrap();
        assert!(input.starts_with(
            "mdkg=(\"@method\" \"@path\" \"@query\" \"mdkg-graph\" \"mdkg-boot\");created=5;"
        ));
        let params = input.strip_prefix("mdkg=").unwrap();
        let base = format!(
            "\"@method\": GET\n\"@path\": /v1/events\n\"@query\": ?after=c-1\n\"mdkg-graph\": g\n\"mdkg-boot\": b\n\"@signature-params\": {params}"
        );
        let sig: [u8; 64] = sig_bytes(&req, "mdkg").try_into().unwrap();
        assert!(Ed25519.verify(&key.public_key(), base.as_bytes(), &sig));
    }

    #[test]
    fn tampering_with_any_covered_part_breaks_it() {
        let (req, key) = signed_write();
        let sig: [u8; 64] = sig_bytes(&req, "mdkg").try_into().unwrap();
        let params = req
            .get("signature-input")
            .unwrap()
            .strip_prefix("mdkg=")
            .unwrap()
            .to_owned();
        let comps = covered(&req);
        let ok = signature_base(&req, &comps, &params).unwrap();
        assert!(Ed25519.verify(&key.public_key(), ok.as_bytes(), &sig));
        let tamper: Vec<Tamper> = vec![
            Box::new(|r| r.method = "PUT".into()),
            Box::new(|r| r.path = "/v1/leases/x".into()),
            Box::new(|r| r.query = Some("a=b".into())),
            Box::new(|r| r.set("Mdkg-Graph", "g-2".into())),
            Box::new(|r| r.set("Mdkg-Boot", "boot-2".into())),
            Box::new(|r| r.set("Mdkg-Actor", "agent-b".into())),
            Box::new(|r| r.set("Idempotency-Key", "k-2".into())),
            Box::new(|r| {
                r.set("Content-Digest", digest::content_digest(b"{}"));
            }),
        ];
        for t in tamper {
            let mut r = req.clone();
            t(&mut r);
            let base = signature_base(&r, &comps, &params).unwrap();
            assert!(!Ed25519.verify(&key.public_key(), base.as_bytes(), &sig));
        }
    }

    #[test]
    fn refusals() {
        let key = MachineKey::from_parts("m", [3u8; 32]);
        let mut r = Outgoing::new("GET", "/", None);
        r.header("Mdkg-Graph", "g").unwrap();
        assert!(
            sign_machine(&mut r.clone(), &key, 1, [0; 16]).is_err(),
            "no boot id"
        );
        r.header("Mdkg-Boot", "b").unwrap();
        assert!(r.header("mdkg-boot", "c").is_err(), "a second copy");
        assert!(r.header("X-Bad", "a\r\nInjected: 1").is_err());
        assert!(r.header("X Bad", "a").is_err());
        assert!(r.header("X-Pad", " a").is_err());
        let mut body = r.clone();
        body.body = b"{}".to_vec();
        assert!(
            sign_machine(&mut body, &key, 1, [0; 16]).is_err(),
            "no digest"
        );
        body.set("Content-Digest", digest::content_digest(b"[]"));
        assert!(
            sign_machine(&mut body, &key, 1, [0; 16]).is_err(),
            "wrong digest"
        );
        sign_machine(&mut r, &key, 1, [0; 16]).unwrap();
        assert!(
            sign_machine(&mut r, &key, 1, [0; 16]).is_err(),
            "signed twice"
        );
        assert!(sf_string("caf\u{e9}").is_err());
        assert_eq!(sf_string(r#"a"b\c"#).unwrap(), r#""a\"b\\c""#);
    }

    #[derive(Debug)]
    struct FilePerson(MachineKey);
    impl Signer for FilePerson {
        fn key_id(&self) -> &str {
            "pk-1"
        }
        fn alg(&self) -> &'static str {
            "ed25519"
        }
        fn sign(&self, base: &[u8]) -> Result<Signature, Box<Diagnostic>> {
            self.0.sign(base)
        }
    }

    #[test]
    fn the_person_signature_covers_the_machine_signature() {
        let (mut req, _) = signed_write();
        let machine_input = req
            .get("signature-input")
            .unwrap()
            .strip_prefix("mdkg=")
            .unwrap()
            .to_owned();
        let machine_sig = req
            .get("signature")
            .unwrap()
            .strip_prefix("mdkg=")
            .unwrap()
            .to_owned();
        let person = FilePerson(MachineKey::from_parts("x", [5u8; 32]));
        sign_person(&mut req, &person, 1_793_290_516).unwrap();
        assert_eq!(req.get("mdkg-person"), Some("pk-1"));
        let input = req.get("signature-input").unwrap();
        let person_params = "(\"signature\";key=\"mdkg\" \"signature-input\";key=\"mdkg\");created=1793290516;keyid=\"pk-1\";alg=\"ed25519\";tag=\"mdkg-v1\"";
        assert_eq!(
            input,
            format!("mdkg={machine_input}, mdkg-person={person_params}")
        );
        let base = format!(
            "\"signature\";key=\"mdkg\": {machine_sig}\n\"signature-input\";key=\"mdkg\": {machine_input}\n\"@signature-params\": {person_params}"
        );
        let sig: [u8; 64] = sig_bytes(&req, "mdkg-person").try_into().unwrap();
        assert!(Ed25519.verify(&person.0.public_key(), base.as_bytes(), &sig));
        // The machine member is unchanged.
        assert!(
            req.get("signature")
                .unwrap()
                .starts_with(&format!("mdkg={machine_sig}, "))
        );
        // Not twice.
        assert!(sign_person(&mut req, &person, 1).is_err());
    }

    #[test]
    fn rfc9421_ed25519_example() {
        // RFC 9421 appendix B.2.6, with the test key of B.1.4.
        let seed =
            b64::decode(concat!("n4Ni+HpISpVObnQMW0wOhCKROaIKqKtW", "/2ZYb2p9KcU=")).unwrap();
        let key = MachineKey::from_parts("test-key-ed25519", seed.try_into().unwrap());
        assert_eq!(
            b64::encode(&key.public_key()),
            "JrQLj5P/89iXES9+vFgrIy29clF9CC/oPPsw3c5D0bs="
        );
        let mut req = Outgoing::new("POST", "/foo", Some("param=Value&Pet=dog".into()));
        req.header("Host", "example.com").unwrap();
        req.header("Date", "Tue, 20 Apr 2021 02:07:55 GMT").unwrap();
        req.header("Content-Type", "application/json").unwrap();
        req.header("Content-Length", "18").unwrap();
        let comps = [
            "date",
            "@method",
            "@path",
            "@authority",
            "content-type",
            "content-length",
        ];
        let params = format!(
            "{};created=1618884473;keyid=\"test-key-ed25519\"",
            inner_list(&comps).unwrap()
        );
        let base = signature_base(&req, &comps, &params).unwrap();
        assert_eq!(
            base,
            "\"date\": Tue, 20 Apr 2021 02:07:55 GMT\n\"@method\": POST\n\"@path\": /foo\n\"@authority\": example.com\n\"content-type\": application/json\n\"content-length\": 18\n\"@signature-params\": (\"date\" \"@method\" \"@path\" \"@authority\" \"content-type\" \"content-length\");created=1618884473;keyid=\"test-key-ed25519\""
        );
        let sig = key.sign(base.as_bytes()).unwrap();
        assert_eq!(
            b64::encode(&sig.0),
            "wqcAqbmYJ2ji2glfAMaRy4gruYYnx2nEFN2HN6jrnDnQCK1u02Gb04v9EDgwUPiu4A0w6vuQv5lIp5WPpBKRCw=="
        );
    }
}
