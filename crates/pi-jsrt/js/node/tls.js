// node:tls — TLSSocket/connect/createServer/createSecureContext over ops/tls.rs (rustls, sans
// I/O: ciphertext flows through the wrapped duplex, usually a net.Socket).

const Buffer = internal.Buffer;
const net = internal.requireBuiltin("net");
const EventEmitter = internal.EventEmitter;

const DEFAULT_ECDH_CURVE = "auto";
const DEFAULT_MIN_VERSION = "TLSv1.2";
const DEFAULT_MAX_VERSION = "TLSv1.3";

function tlsError(info) {
  const err = new Error(info.message);
  err.code = info.code;
  if (info.code === "ERR_TLS_CERT_ALTNAME_INVALID") err.reason = info.message.replace(/^Hostname\/IP does not match certificate's altnames: /, "");
  return err;
}

const toBuf = (u8) => Buffer.from(u8.buffer, u8.byteOffset, u8.byteLength);

class SecureContext {
  constructor(options = {}) {
    this.options = options;
    this.context = {};
    this._server = undefined;
  }
  /** Rust server config (built on first server use; clients build theirs per connection). */
  _serverConfig() {
    this._server ??= ops.tlsServerContext(this.options);
    return this._server;
  }
}

function createSecureContext(options = {}) {
  return new SecureContext(options);
}

function x509Legacy(der) {
  if (!der) return {};
  const { X509Certificate } = internal.requireBuiltin("crypto");
  const cert = new X509Certificate(toBuf(der));
  const legacy = cert.toLegacyObject();
  legacy.pubkey = undefined;
  return legacy;
}

class TLSSocket extends net.Socket {
  constructor(socket, options = {}) {
    super({ allowHalfOpen: options.allowHalfOpen === true });
    this.encrypted = true;
    this.authorized = false;
    this.authorizationError = null;
    this.alpnProtocol = null;
    this.servername = null;
    this._tlsOptions = options;
    this._secured = false;
    this._pendingPlain = [];
    this._handshakeStarted = false;
    this._parent = null;
    this._info = null;
    this._isServer = !!options.isServer;
    if (socket) this._wrap(socket);
  }

  _wrap(socket) {
    this._parent = socket;
    this.connecting = !!socket.connecting;
    const copyEndpoints = () => {
      for (const key of ["remoteAddress", "remotePort", "remoteFamily", "localAddress", "localPort", "localFamily"]) {
        if (socket[key] !== undefined) this[key] = socket[key];
      }
    };
    copyEndpoints();
    socket.on("data", (data) => this._onCipher(data));
    socket.on("end", () => {
      if (!this._secured) {
        this._fail(Object.assign(new Error("Client network socket disconnected before secure TLS connection was established"), { code: "ECONNRESET" }));
        return;
      }
      this.push(null);
      if (!this.allowHalfOpen && !this._writableState.ending) this.end();
    });
    socket.on("error", (err) => this.destroy(err));
    socket.on("close", (hadError) => {
      if (!this.destroyed) this.destroy();
    });
    socket.on("timeout", () => this.emit("timeout"));
    const start = () => {
      copyEndpoints();
      this.connecting = false;
      this.pending = false;
      if (!this._isServer) this.emit("connect");
      this._startHandshake();
    };
    if (socket.connecting) socket.once("connect", start);
    else process.nextTick(start);
  }

  _startHandshake() {
    if (this._handshakeStarted || this.destroyed) return;
    this._handshakeStarted = true;
    const o = this._tlsOptions;
    if (this._isServer) {
      this._tls = ops.tlsServer(o.secureContext._serverConfig());
      return;
    }
    const host = o.host ?? "localhost";
    const servername = o.servername ?? (net.isIP(host) ? undefined : host);
    this._tls = ops.tlsClient({
      host,
      servername,
      ca: o.ca ?? o.secureContext?.options?.ca,
      cert: o.cert ?? o.secureContext?.options?.cert,
      key: o.key ?? o.secureContext?.options?.key,
      passphrase: o.passphrase ?? o.secureContext?.options?.passphrase,
      rejectUnauthorized: o.rejectUnauthorized,
      ALPNProtocols: normalizeALPN(o.ALPNProtocols),
      skipNameCheck: typeof o.checkServerIdentity === "function",
    });
    // The ClientHello.
    this._onCipher(new Uint8Array(0));
  }

  _onCipher(data) {
    if (!this._tls) {
      if (this._isServer) this._startHandshake();
      else return;
    }
    let r;
    try {
      r = ops.tlsFeed(this._tls, data);
    } catch (err) {
      this._fail(err);
      return;
    }
    if (r.out.length && this._parent && !this._parent.destroyed) this._parent.write(toBuf(r.out));
    if (r.error) {
      this._fail(tlsError(r.error));
      return;
    }
    if (r.secure && !this._secured) this._onSecure();
    if (r.plain.length) {
      const chunk = toBuf(r.plain);
      this.bytesRead += chunk.length;
      this.push(chunk);
    }
    if (r.closed) {
      this.push(null);
      if (!this.allowHalfOpen && !this._writableState.ending) this.end();
    }
  }

  _onSecure() {
    this._secured = true;
    const info = ops.tlsInfo(this._tls);
    this._info = info;
    this.alpnProtocol = info.alpnProtocol;
    this.servername = info.servername;
    const o = this._tlsOptions;
    if (this._isServer) {
      const peer = info.peerCertificates.length > 0;
      this.authorized = peer && !info.authorizationError;
      if (o.requestCert && !peer) this.authorizationError = "UNABLE_TO_GET_ISSUER_CERT";
    } else {
      let authError = info.authorizationError ? tlsError(info.authorizationError) : null;
      if (!authError && typeof o.checkServerIdentity === "function") {
        const host = o.servername || o.host || "localhost";
        const cert = this.getPeerCertificate();
        const result = o.checkServerIdentity(host, cert);
        if (result instanceof Error) authError = result;
      }
      this.authorized = !authError;
      this.authorizationError = authError ? authError.code ?? authError.message : null;
      if (authError && o.rejectUnauthorized !== false) {
        this._fail(authError);
        return;
      }
    }
    for (const [chunk, cb] of this._pendingPlain.splice(0)) this._write(chunk, "buffer", cb);
    if (this._finalPending) {
      const cb = this._finalPending;
      this._finalPending = null;
      this._final(cb);
    }
    this.emit(this._isServer ? "secure" : "secureConnect");
  }

  _fail(err) {
    if (this.destroyed) return;
    this.destroy(err);
  }

  _read() {}

  _write(chunk, encoding, callback) {
    if (!this._secured) {
      this._pendingPlain.push([chunk, callback]);
      return;
    }
    const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, encoding);
    const out = ops.tlsWrite(this._tls, buf);
    this.bytesWritten += buf.length;
    if (!this._parent || this._parent.destroyed) {
      callback(Object.assign(new Error("This socket has been ended by the other party"), { code: "EPIPE" }));
      return;
    }
    this._parent.write(toBuf(out), (err) => callback(err ?? undefined));
  }

  _final(callback) {
    if (!this._secured) {
      this._finalPending = callback;
      return;
    }
    const out = ops.tlsClose(this._tls);
    if (this._parent && !this._parent.destroyed) this._parent.end(toBuf(out));
    callback();
  }

  _destroy(err, callback) {
    this._clearTimer?.();
    if (this._parent && !this._parent.destroyed) this._parent.destroy();
    callback(err);
  }

  connect() {
    throw new Error("tls.TLSSocket#connect is not supported in pi-jsrt; use tls.connect()");
  }

  address() {
    return this._parent?.address?.() ?? {};
  }
  setNoDelay(v) {
    this._parent?.setNoDelay?.(v);
    return this;
  }
  setKeepAlive(...args) {
    this._parent?.setKeepAlive?.(...args);
    return this;
  }
  setTimeout(ms, callback) {
    this._parent?.setTimeout?.(ms);
    if (callback) this.once("timeout", callback);
    return this;
  }
  ref() {
    this._parent?.ref?.();
    return this;
  }
  unref() {
    this._parent?.unref?.();
    return this;
  }

  getPeerCertificate(detailed) {
    const certs = this._info?.peerCertificates ?? [];
    if (!certs.length) return {};
    const out = x509Legacy(certs[0]);
    if (detailed) {
      let cur = out;
      for (let i = 1; i < certs.length; i++) {
        cur.issuerCertificate = x509Legacy(certs[i]);
        cur = cur.issuerCertificate;
      }
      cur.issuerCertificate = cur;
    }
    return out;
  }
  getX509Certificate() {
    const der = this._info?.peerCertificates?.[0];
    if (!der) return undefined;
    const { X509Certificate } = internal.requireBuiltin("crypto");
    return new X509Certificate(toBuf(der));
  }
  getPeerX509Certificate() {
    return this.getX509Certificate();
  }
  getCertificate() {
    return {};
  }
  getProtocol() {
    return this._info?.protocol ?? null;
  }
  getCipher() {
    const name = this._info?.cipher;
    if (!name) return {};
    return { name, standardName: name, version: this._info.protocol };
  }
  getEphemeralKeyInfo() {
    return {};
  }
  getSession() {
    return undefined;
  }
  getTLSTicket() {
    return undefined;
  }
  isSessionReused() {
    return false;
  }
  getFinished() {
    return undefined;
  }
  getPeerFinished() {
    return undefined;
  }
  getSharedSigalgs() {
    return [];
  }
  exportKeyingMaterial(length, label, context) {
    return toBuf(ops.tlsExportKeyingMaterial(this._tls, length, Buffer.from(String(label)), context));
  }
  setServername(name) {
    this._tlsOptions.servername = name;
  }
  setSession() {}
  setMaxSendFragment() {
    return true;
  }
  renegotiate(options, callback) {
    if (typeof callback === "function") process.nextTick(callback, new Error("TLS renegotiation is not supported"));
    return false;
  }
  disableRenegotiation() {}
  enableTrace() {}
}

function normalizeALPN(list) {
  if (!list) return undefined;
  if (Array.isArray(list)) return list.map(String);
  // Node also accepts the wire format (length-prefixed buffer).
  const bytes = Buffer.from(list);
  const out = [];
  for (let i = 0; i < bytes.length; ) {
    const n = bytes[i];
    out.push(bytes.subarray(i + 1, i + 1 + n).toString());
    i += 1 + n;
  }
  return out;
}

function normalizeConnectArgs(args) {
  const [first, second, third, fourth] = args;
  let options = {};
  let callback;
  if (first !== null && typeof first === "object") {
    options = { ...first };
    callback = second;
  } else if (typeof first === "number" || (typeof first === "string" && /^\d+$/.test(first))) {
    options.port = Number(first);
    if (typeof second === "string") {
      options.host = second;
      if (third !== null && typeof third === "object") {
        options = { ...third, ...options };
        callback = fourth;
      } else callback = third;
    } else if (second !== null && typeof second === "object") {
      options = { ...second, ...options };
      callback = third;
    } else callback = second;
  }
  return { options, callback: typeof callback === "function" ? callback : undefined };
}

function connect(...args) {
  const { options, callback } = normalizeConnectArgs(args);
  if (options.rejectUnauthorized === undefined && process.env.NODE_TLS_REJECT_UNAUTHORIZED === "0") options.rejectUnauthorized = false;
  options.host ??= options.hostname ?? "localhost";
  const raw = options.socket ?? net.connect({ host: options.host, port: options.port, localAddress: options.localAddress });
  const socket = new TLSSocket(null, options);
  if (callback) socket.once("secureConnect", callback);
  if (options.timeout) socket.setTimeout(options.timeout);
  socket._wrap(raw);
  return socket;
}

class Server extends net.Server {
  constructor(options, listener) {
    if (typeof options === "function") {
      listener = options;
      options = {};
    }
    options = options ?? {};
    super(options);
    this._tlsOptions = options;
    this._secureContext = options.secureContext ?? createSecureContext(options);
    this._secureContext._serverConfig();
    if (listener) this.on("secureConnection", listener);
    this.on("connection", (raw) => this._onRawConnection(raw));
  }

  _onRawConnection(raw) {
    const socket = new TLSSocket(null, { ...this._tlsOptions, isServer: true, secureContext: this._secureContext });
    socket.server = this;
    socket.once("secure", () => this.emit("secureConnection", socket));
    socket.on("error", (err) => {
      if (!socket._secured) {
        if (this.listenerCount("tlsClientError") > 0) this.emit("tlsClientError", err, socket);
      } else if (socket.listenerCount("error") === 1) {
        // Only our listener: Node would surface it as an uncaught error on the socket.
      }
    });
    socket._wrap(raw);
  }

  setSecureContext(options) {
    this._secureContext = createSecureContext(options);
    this._secureContext._serverConfig();
  }

  addContext() {}
  getTicketKeys() {
    return Buffer.alloc(48);
  }
  setTicketKeys() {}
}

function createServer(options, listener) {
  return new Server(options, listener);
}

function checkServerIdentity(hostname, cert) {
  const names = [];
  const alt = cert?.subjectaltname;
  if (alt) for (const part of alt.split(/,\s*/)) if (part.startsWith("DNS:")) names.push(part.slice(4));
  if (!names.length && cert?.subject?.CN) names.push(cert.subject.CN);
  const matches = names.some((n) => {
    const re = new RegExp(`^${n.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/^\\\*|^\*/, "[^.]+")}$`, "i");
    return re.test(hostname);
  });
  if (matches) return undefined;
  const reason = `Host: ${hostname}. is not in the cert's altnames: ${alt ?? `DNS:${cert?.subject?.CN ?? ""}`}`;
  return Object.assign(new Error(`Hostname/IP does not match certificate's altnames: ${reason}`), { code: "ERR_TLS_CERT_ALTNAME_INVALID", reason, host: hostname, cert });
}

const tls = {
  CLIENT_RENEG_LIMIT: 3,
  CLIENT_RENEG_WINDOW: 600,
  DEFAULT_CIPHERS: "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384",
  DEFAULT_ECDH_CURVE,
  DEFAULT_MIN_VERSION,
  DEFAULT_MAX_VERSION,
  get rootCertificates() {
    const value = Object.freeze(ops.tlsRootCertificates());
    Object.defineProperty(tls, "rootCertificates", { value, enumerable: true });
    return value;
  },
  getCiphers: () => ["tls_aes_128_gcm_sha256", "tls_aes_256_gcm_sha384", "tls_chacha20_poly1305_sha256", "ecdhe-ecdsa-aes128-gcm-sha256", "ecdhe-ecdsa-aes256-gcm-sha384", "ecdhe-ecdsa-chacha20-poly1305", "ecdhe-rsa-aes128-gcm-sha256", "ecdhe-rsa-aes256-gcm-sha384", "ecdhe-rsa-chacha20-poly1305"],
  getCACertificates: (type = "default") => (type === "extra" ? [] : [...tls.rootCertificates]),
  setDefaultCACertificates() {},
  checkServerIdentity,
  connect,
  createServer,
  createSecureContext,
  SecureContext,
  Server,
  TLSSocket,
  convertALPNProtocols(protocols, out) {
    const list = normalizeALPN(protocols) ?? [];
    out.ALPNProtocols = Buffer.concat(list.map((p) => Buffer.concat([Buffer.from([Buffer.byteLength(p)]), Buffer.from(p)])));
  },
};

internal.tls = tls;
internal.defineBuiltin("tls", () => tls);
internal.defineBuiltin("_tls_wrap", () => ({ TLSSocket, Server, connect }));
internal.defineBuiltin("_tls_common", () => ({ SecureContext, createSecureContext }));
