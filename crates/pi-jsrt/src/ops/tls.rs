//! TLS for `node:tls`/`node:https` on rustls (ring provider), sans I/O: JS feeds ciphertext
//! read from any duplex (a `net.Socket`, or a user-supplied stream) and writes back what rustls
//! produces, so TLS works over every transport Node allows (`tls.connect({ socket })`).
//!
//! Certificate verification follows Node: the default trust store is Mozilla's (webpki-roots,
//! like Node's bundled CAs) plus the system store and `NODE_EXTRA_CA_CERTS`; a `ca` option
//! replaces it. With `rejectUnauthorized: false` verification still runs and its error is
//! reported as `authorizationError` instead of failing the handshake. Errors use OpenSSL's
//! codes (`DEPTH_ZERO_SELF_SIGNED_CERT`, `UNABLE_TO_VERIFY_LEAF_SIGNATURE`, `CERT_HAS_EXPIRED`,
//! `ERR_TLS_CERT_ALTNAME_INVALID`, ...).

use std::io::{Read, Write};
use std::sync::{Arc, Mutex, OnceLock};

use crate::js::{Context, JsArgs, JsResult, JsValue, js_string};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme};

use super::keys::{Imported, cert_fields, import, parse_pem_all};
use super::{bytes_arg, bytes_to_js, coded_error, native_object, object, with_native};
use crate::engine::{OpFn, js_str};

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    static P: OnceLock<Arc<rustls::crypto::CryptoProvider>> = OnceLock::new();
    P.get_or_init(|| Arc::new(rustls::crypto::ring::default_provider())).clone()
}

/// Mozilla roots + system roots + `NODE_EXTRA_CA_CERTS` (loaded once).
fn default_roots() -> Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let native = rustls_native_certs::load_native_certs();
            roots.add_parsable_certificates(native.certs);
            if let Some(path) = std::env::var_os("NODE_EXTRA_CA_CERTS")
                && let Ok(text) = std::fs::read_to_string(path)
            {
                roots.add_parsable_certificates(pem_certs(&text));
            }
            Arc::new(roots)
        })
        .clone()
}

/// A client config with the default trust store (Mozilla + system + `NODE_EXTRA_CA_CERTS`), for
/// native clients such as `WebSocket` and `http2.connect`.
pub(crate) fn default_client_config(alpn: Vec<Vec<u8>>) -> Result<Arc<rustls::ClientConfig>, String> {
    let mut config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(default_roots())
        .with_no_client_auth();
    config.alpn_protocols = alpn;
    Ok(Arc::new(config))
}

fn pem_certs(text: &str) -> Vec<CertificateDer<'static>> {
    parse_pem_all(text).into_iter().filter(|p| p.label.contains("CERTIFICATE")).map(|p| CertificateDer::from(p.der)).collect()
}

/// A verification failure in Node's terms: `(code, message)`.
type AuthError = (String, String);

fn is_self_signed(der: &[u8]) -> bool {
    cert_fields(der).is_some_and(|[_, issuer, _, subject, _]| issuer == subject)
}

fn auth_error(e: &TlsError, end_entity: &CertificateDer<'_>, intermediates: usize, host: &str) -> AuthError {
    let pair = |c: &str, m: &str| (c.to_string(), m.to_string());
    // OpenSSL reports a lone self-signed certificate before anything else (webpki may say
    // UnknownIssuer, or CaUsedAsEndEntity for a CA:TRUE certificate).
    let self_signed = intermediates == 0 && is_self_signed(end_entity);
    if self_signed && matches!(e, TlsError::InvalidCertificate(CertificateError::UnknownIssuer | CertificateError::Other(_))) {
        return pair("DEPTH_ZERO_SELF_SIGNED_CERT", "self-signed certificate");
    }
    match e {
        TlsError::InvalidCertificate(ce) => match ce {
            CertificateError::UnknownIssuer => {
                if intermediates == 0 && is_self_signed(end_entity) {
                    pair("DEPTH_ZERO_SELF_SIGNED_CERT", "self-signed certificate")
                } else if intermediates == 0 {
                    pair("UNABLE_TO_VERIFY_LEAF_SIGNATURE", "unable to verify the first certificate")
                } else {
                    pair("UNABLE_TO_GET_ISSUER_CERT_LOCALLY", "unable to get local issuer certificate")
                }
            }
            CertificateError::Expired | CertificateError::ExpiredContext { .. } => pair("CERT_HAS_EXPIRED", "certificate has expired"),
            CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => pair("CERT_NOT_YET_VALID", "certificate is not yet valid"),
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
                let names = super::keys::subject_alt_names(end_entity).unwrap_or_default();
                ("ERR_TLS_CERT_ALTNAME_INVALID".into(), format!("Hostname/IP does not match certificate's altnames: Host: {host}. is not in the cert's altnames: {names}"))
            }
            CertificateError::Revoked => pair("CERT_REVOKED", "certificate revoked"),
            CertificateError::BadSignature => pair("CERT_SIGNATURE_FAILURE", "certificate signature failure"),
            other => ("UNABLE_TO_VERIFY_LEAF_SIGNATURE".into(), format!("certificate verification failed: {other:?}")),
        },
        other => ("UNABLE_TO_VERIFY_LEAF_SIGNATURE".into(), other.to_string()),
    }
}

#[derive(Debug)]
struct NodeVerifier {
    inner: Arc<WebPkiServerVerifier>,
    reject: bool,
    skip_name: bool,
    host: String,
    result: Arc<Mutex<Option<AuthError>>>,
}

fn is_name_error(e: &TlsError) -> bool {
    matches!(e, TlsError::InvalidCertificate(CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. }))
}

impl ServerCertVerifier for NodeVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        match self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) {
            Ok(v) => Ok(v),
            Err(e) => {
                if self.skip_name && is_name_error(&e) {
                    return Ok(ServerCertVerified::assertion());
                }
                *self.result.lock().unwrap() = Some(auth_error(&e, end_entity, intermediates.len(), &self.host));
                if self.reject { Err(e) } else { Ok(ServerCertVerified::assertion()) }
            }
        }
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// A TLS endpoint driven by JS.
pub(crate) struct TlsConn {
    conn: rustls::Connection,
    auth: Arc<Mutex<Option<AuthError>>>,
    servername: Option<String>,
}

fn err(message: impl Into<String>, code: &str) -> (String, String) {
    (code.to_string(), message.into())
}

fn tls_error_info(e: &TlsError, auth: &Option<AuthError>) -> (String, String) {
    if let Some((code, message)) = auth
        && matches!(e, TlsError::InvalidCertificate(_))
    {
        return (code.clone(), message.clone());
    }
    match e {
        TlsError::AlertReceived(alert) => {
            let name = format!("{alert:?}");
            let snake: String = name.chars().enumerate().fold(String::new(), |mut s, (i, c)| {
                if c.is_uppercase() && i > 0 {
                    s.push('_');
                }
                s.push(c.to_ascii_uppercase());
                s
            });
            let prefix = if matches!(alert, rustls::AlertDescription::HandshakeFailure | rustls::AlertDescription::BadCertificate | rustls::AlertDescription::CertificateUnknown) { "SSLV3" } else { "TLSV1" };
            err(format!("{prefix} alert {}", name.to_lowercase()), &format!("ERR_SSL_{prefix}_ALERT_{snake}"))
        }
        TlsError::InvalidMessage(_) | TlsError::InappropriateMessage { .. } | TlsError::InappropriateHandshakeMessage { .. } => {
            err("wrong version number", "ERR_SSL_WRONG_VERSION_NUMBER")
        }
        TlsError::NoApplicationProtocol => err("no application protocol", "ERR_SSL_NO_APPLICATION_PROTOCOL"),
        other => err(other.to_string(), "ERR_SSL_HANDSHAKE_FAILURE"),
    }
}

impl TlsConn {
    /// Feed ciphertext. Returns (plaintext, ciphertext to send, peer closed, error).
    fn feed(&mut self, mut data: &[u8]) -> (Vec<u8>, Vec<u8>, bool, Option<TlsError>) {
        let mut plain = Vec::new();
        let mut out = Vec::new();
        let mut closed = false;
        let mut error = None;
        loop {
            if !data.is_empty() {
                match self.conn.read_tls(&mut data) {
                    Ok(_) => {}
                    Err(e) => {
                        error = Some(TlsError::General(e.to_string()));
                        break;
                    }
                }
            }
            if let Err(e) = self.conn.process_new_packets() {
                error = Some(e);
                break;
            }
            let mut buf = [0u8; 16384];
            loop {
                match self.conn.reader().read(&mut buf) {
                    Ok(0) => {
                        closed = true;
                        break;
                    }
                    Ok(n) => plain.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        closed = true;
                        break;
                    }
                }
            }
            if data.is_empty() {
                break;
            }
        }
        while self.conn.wants_write() {
            if self.conn.write_tls(&mut out).is_err() {
                break;
            }
        }
        (plain, out, closed, error)
    }

    fn write(&mut self, data: &[u8]) -> Vec<u8> {
        let _ = self.conn.writer().write_all(data);
        let mut out = Vec::new();
        while self.conn.wants_write() {
            if self.conn.write_tls(&mut out).is_err() {
                break;
            }
        }
        out
    }

    fn close(&mut self) -> Vec<u8> {
        self.conn.send_close_notify();
        let mut out = Vec::new();
        while self.conn.wants_write() {
            if self.conn.write_tls(&mut out).is_err() {
                break;
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Option parsing
// ---------------------------------------------------------------------------

fn get(ctx: &mut Context, o: &JsValue, k: &str) -> JsResult<JsValue> {
    match o.as_object() {
        Some(obj) => obj.get(js_str(k).as_string().unwrap(), ctx),
        None => Ok(JsValue::undefined()),
    }
}

/// PEM text items of an option that may be a string, Buffer or an array of them.
fn pem_list(ctx: &mut Context, v: &JsValue) -> JsResult<Vec<String>> {
    if v.is_null_or_undefined() {
        return Ok(Vec::new());
    }
    if let Some(obj) = v.as_object()
        && obj.is_array()
    {
        let len = obj.get(js_string!("length"), ctx)?.to_length(ctx)?;
        let mut out = Vec::new();
        for i in 0..len {
            let item = obj.get(i, ctx)?;
            out.extend(pem_list(ctx, &item)?);
        }
        return Ok(out);
    }
    Ok(vec![String::from_utf8_lossy(&bytes_arg(v, ctx)?).into_owned()])
}

fn alpn_list(ctx: &mut Context, v: &JsValue) -> JsResult<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    if let Some(obj) = v.as_object()
        && obj.is_array()
    {
        let len = obj.get(js_string!("length"), ctx)?.to_length(ctx)?;
        for i in 0..len {
            out.push(obj.get(i, ctx)?.to_string(ctx)?.to_std_string_lossy().into_bytes());
        }
    }
    Ok(out)
}

fn throw(ctx: &mut Context, code: &str, message: &str) -> crate::js::JsError {
    coded_error(ctx, "Error", message, &[("code", js_str(code))])
}

/// `(chain, key)` from `cert`/`key`/`passphrase` options.
fn identity(ctx: &mut Context, opts: &JsValue) -> JsResult<Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>> {
    let certs: Vec<CertificateDer<'static>> = { let v = get(ctx, opts, "cert")?; pem_list(ctx, &v)? }.iter().flat_map(|t| pem_certs(t)).collect();
    let keys = { let v = get(ctx, opts, "key")?; pem_list(ctx, &v)? };
    if certs.is_empty() || keys.is_empty() {
        return Ok(None);
    }
    let pass = get(ctx, opts, "passphrase")?;
    let pass = if pass.is_null_or_undefined() { None } else { Some(bytes_arg(&pass, ctx)?) };
    let key = match import(keys[0].as_bytes(), "pem", "", pass.as_deref(), true) {
        Ok(Imported::Private(k)) => PrivateKeyDer::Pkcs8(k.to_pkcs8().into()),
        Ok(_) => return Err(throw(ctx, "ERR_OSSL_UNSUPPORTED", "error:1E08010C:DECODER routines::unsupported")),
        Err(e) => return Err(e.throw(ctx)),
    };
    Ok(Some((certs, key)))
}

fn roots_from(ctx: &mut Context, opts: &JsValue) -> JsResult<Arc<RootCertStore>> {
    let ca = { let v = get(ctx, opts, "ca")?; pem_list(ctx, &v)? };
    if ca.is_empty() {
        return Ok(default_roots());
    }
    let mut roots = RootCertStore::empty();
    for text in &ca {
        roots.add_parsable_certificates(pem_certs(text));
    }
    Ok(Arc::new(roots))
}

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

/// `ops.tlsClient({ host, servername, ca, cert, key, passphrase, rejectUnauthorized, ALPNProtocols,
/// skipNameCheck }) -> handle`
fn op_tls_client(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let opts = args.get_or_undefined(0).clone();
    let host = get(ctx, &opts, "host")?;
    let host = if host.is_null_or_undefined() { "localhost".to_string() } else { host.to_string(ctx)?.to_std_string_lossy() };
    let sni = get(ctx, &opts, "servername")?;
    let sni = if sni.is_null_or_undefined() { None } else { Some(sni.to_string(ctx)?.to_std_string_lossy()) };
    let reject = get(ctx, &opts, "rejectUnauthorized")?;
    let reject = reject.is_undefined() || reject.to_boolean();
    let skip_name = get(ctx, &opts, "skipNameCheck")?.to_boolean();
    let roots = roots_from(ctx, &opts)?;
    let alpn = { let v = get(ctx, &opts, "ALPNProtocols")?; alpn_list(ctx, &v)? };
    let identity = identity(ctx, &opts)?;
    let name_for_verify = match &sni {
        Some(s) if !s.is_empty() => s.clone(),
        _ => host.clone(),
    };
    let server_name = ServerName::try_from(name_for_verify.trim_start_matches('[').trim_end_matches(']').to_string())
        .map_err(|_| throw(ctx, "ERR_INVALID_ARG_VALUE", &format!("Invalid server name: {name_for_verify}")))?;
    let auth = Arc::new(Mutex::new(None));
    let inner = WebPkiServerVerifier::builder_with_provider(roots, provider()).build().map_err(|e| throw(ctx, "ERR_TLS_INVALID_CONTEXT", &e.to_string()))?;
    let verifier = NodeVerifier { inner, reject, skip_name, host: name_for_verify.clone(), result: auth.clone() };
    let builder = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| throw(ctx, "ERR_TLS_INVALID_CONTEXT", &e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier));
    let mut config = match identity {
        Some((chain, key)) => builder.with_client_auth_cert(chain, key).map_err(|e| throw(ctx, "ERR_TLS_INVALID_CONTEXT", &e.to_string()))?,
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = alpn;
    // Node sends no SNI for IP addresses or an empty servername.
    config.enable_sni = !matches!(server_name, ServerName::IpAddress(_)) && sni.as_deref() != Some("");
    let conn = rustls::ClientConnection::new(Arc::new(config), server_name).map_err(|e| throw(ctx, "ERR_SSL_HANDSHAKE_FAILURE", &e.to_string()))?;
    Ok(native_object(TlsConn { conn: conn.into(), auth, servername: sni.or(Some(name_for_verify)) }))
}

/// `ops.tlsServerContext({ cert, key, passphrase, ca, requestCert, rejectUnauthorized,
/// ALPNProtocols }) -> context`
fn op_tls_server_context(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let opts = args.get_or_undefined(0).clone();
    let Some((chain, key)) = identity(ctx, &opts)? else {
        return Err(throw(ctx, "ERR_TLS_MISSING_CERT", "A TLS server needs `key` and `cert` (or `pfx`, which pi-jsrt does not support)"));
    };
    let request_cert = get(ctx, &opts, "requestCert")?.to_boolean();
    let reject = get(ctx, &opts, "rejectUnauthorized")?;
    let reject = reject.is_undefined() || reject.to_boolean();
    let builder = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| throw(ctx, "ERR_TLS_INVALID_CONTEXT", &e.to_string()))?;
    let builder = if request_cert {
        let roots = roots_from(ctx, &opts)?;
        let v = rustls::server::WebPkiClientVerifier::builder_with_provider(roots, provider());
        let v = if reject { v } else { v.allow_unauthenticated() };
        builder.with_client_cert_verifier(v.build().map_err(|e| throw(ctx, "ERR_TLS_INVALID_CONTEXT", &e.to_string()))?)
    } else {
        builder.with_no_client_auth()
    };
    let mut config = builder.with_single_cert(chain, key).map_err(|e| throw(ctx, "ERR_TLS_INVALID_CONTEXT", &e.to_string()))?;
    config.alpn_protocols = { let v = get(ctx, &opts, "ALPNProtocols")?; alpn_list(ctx, &v)? };
    Ok(native_object(Arc::new(config)))
}

/// `ops.tlsServer(context) -> handle`
fn op_tls_server(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let config = with_native::<Arc<rustls::ServerConfig>, _>(args.get_or_undefined(0), |c| c.clone())?;
    let conn = rustls::ServerConnection::new(config).map_err(|e| throw(ctx, "ERR_SSL_HANDSHAKE_FAILURE", &e.to_string()))?;
    Ok(native_object(TlsConn { conn: conn.into(), auth: Arc::new(Mutex::new(None)), servername: None }))
}

/// `ops.tlsFeed(handle, ciphertext) -> { plain, out, closed, secure, error? }`
fn op_tls_feed(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let data = bytes_arg(args.get_or_undefined(1), ctx)?;
    let (plain, out, closed, error, secure, auth) = with_native::<TlsConn, _>(args.get_or_undefined(0), |t| {
        let (plain, out, closed, error) = t.feed(&data);
        let auth = t.auth.lock().unwrap().clone();
        let info = error.as_ref().map(|e| tls_error_info(e, &auth));
        (plain, out, closed, info, !t.conn.is_handshaking(), auth)
    })?;
    let _ = auth;
    let plain = bytes_to_js(&plain, ctx)?;
    let out = bytes_to_js(&out, ctx)?;
    let mut fields = vec![("plain", plain), ("out", out), ("closed", JsValue::from(closed)), ("secure", JsValue::from(secure))];
    if let Some((code, message)) = error {
        let e = object(ctx, vec![("code", js_str(&code)), ("message", js_str(&message))])?;
        fields.push(("error", e));
    }
    object(ctx, fields)
}

/// `ops.tlsWrite(handle, plaintext) -> ciphertext`
fn op_tls_write(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let data = bytes_arg(args.get_or_undefined(1), ctx)?;
    let out = with_native::<TlsConn, _>(args.get_or_undefined(0), |t| t.write(&data))?;
    bytes_to_js(&out, ctx)
}

/// `ops.tlsClose(handle) -> ciphertext` (close_notify)
fn op_tls_close(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let out = with_native::<TlsConn, _>(args.get_or_undefined(0), |t| t.close())?;
    bytes_to_js(&out, ctx)
}

/// `ops.tlsInfo(handle) -> { protocol, cipher, alpnProtocol, authorizationError, peerCertificates, servername }`
fn op_tls_info(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    struct Info {
        protocol: Option<&'static str>,
        cipher: Option<String>,
        alpn: Option<String>,
        auth: Option<AuthError>,
        certs: Vec<Vec<u8>>,
        servername: Option<String>,
    }
    let info = with_native::<TlsConn, _>(args.get_or_undefined(0), |t| Info {
        protocol: t.conn.protocol_version().map(|v| match v {
            rustls::ProtocolVersion::TLSv1_3 => "TLSv1.3",
            rustls::ProtocolVersion::TLSv1_2 => "TLSv1.2",
            _ => "unknown",
        }),
        cipher: t.conn.negotiated_cipher_suite().map(|s| format!("{:?}", s.suite())),
        alpn: t.conn.alpn_protocol().map(|p| String::from_utf8_lossy(p).into_owned()),
        auth: t.auth.lock().unwrap().clone(),
        certs: t.conn.peer_certificates().map(|c| c.iter().map(|d| d.to_vec()).collect()).unwrap_or_default(),
        servername: match &t.conn {
            rustls::Connection::Server(s) => s.server_name().map(String::from),
            rustls::Connection::Client(_) => t.servername.clone(),
        },
    })?;
    let mut certs = Vec::new();
    for c in &info.certs {
        certs.push(bytes_to_js(c, ctx)?);
    }
    let certs = super::array(ctx, certs);
    let cipher = info.cipher.map(|c| {
        // rustls names TLS 1.3 suites TLS13_AES_128_GCM_SHA256; OpenSSL/IANA say TLS_AES_128_GCM_SHA256.
        let iana = c.replacen("TLS13_", "TLS_", 1);
        js_str(&iana)
    });
    let auth = match info.auth {
        Some((code, message)) => object(ctx, vec![("code", js_str(&code)), ("message", js_str(&message))])?,
        None => JsValue::null(),
    };
    object(
        ctx,
        vec![
            ("protocol", info.protocol.map(js_str).unwrap_or_default()),
            ("cipher", cipher.unwrap_or_default()),
            ("alpnProtocol", info.alpn.as_deref().map(js_str).unwrap_or(JsValue::from(false))),
            ("authorizationError", auth),
            ("peerCertificates", certs),
            ("servername", info.servername.as_deref().map(js_str).unwrap_or(JsValue::from(false))),
        ],
    )
}

/// `ops.tlsExportKeyingMaterial(handle, length, label, context?) -> bytes`
fn op_tls_ekm(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let len = args.get_or_undefined(1).to_number(ctx)? as usize;
    let label = bytes_arg(args.get_or_undefined(2), ctx)?;
    let context = args.get_or_undefined(3);
    let context = if context.is_null_or_undefined() { None } else { Some(bytes_arg(context, ctx)?) };
    let r = with_native::<TlsConn, _>(args.get_or_undefined(0), |t| t.conn.export_keying_material(vec![0u8; len], &label, context.as_deref()))?;
    match r {
        Ok(out) => bytes_to_js(&out, ctx),
        Err(e) => Err(throw(ctx, "ERR_TLS_INVALID_STATE", &e.to_string())),
    }
}

/// `ops.tlsRootCertificates() -> string[]` (PEM, for `tls.rootCertificates`): the system's CA
/// certificates (webpki-roots only carries trust anchors, not whole certificates).
fn op_tls_root_certificates(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    static PEMS: OnceLock<Vec<String>> = OnceLock::new();
    let pems = PEMS.get_or_init(|| rustls_native_certs::load_native_certs().certs.iter().map(|c| super::keys::to_pem("CERTIFICATE", c.as_ref())).collect());
    let items = pems.iter().map(|p| js_str(p)).collect();
    Ok(super::array(ctx, items))
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("tlsClient", 1, op_tls_client),
    ("tlsServerContext", 1, op_tls_server_context),
    ("tlsServer", 1, op_tls_server),
    ("tlsFeed", 2, op_tls_feed),
    ("tlsWrite", 2, op_tls_write),
    ("tlsClose", 1, op_tls_close),
    ("tlsInfo", 1, op_tls_info),
    ("tlsExportKeyingMaterial", 4, op_tls_ekm),
    ("tlsRootCertificates", 0, op_tls_root_certificates),
];
