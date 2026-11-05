// node:http2 — sessions on the h2 crate, sans I/O over net/tls sockets (src/ops/http2.rs),
// core API (connect/request, createServer/createSecureServer, ServerHttp2Stream.respond*) and
// the compatibility API (Http2ServerRequest/Http2ServerResponse).

const constants = {
  NGHTTP2_ERR_FRAME_SIZE_ERROR: -522, NGHTTP2_SESSION_SERVER: 0, NGHTTP2_SESSION_CLIENT: 1,
  NGHTTP2_STREAM_STATE_IDLE: 1, NGHTTP2_STREAM_STATE_OPEN: 2, NGHTTP2_STREAM_STATE_RESERVED_LOCAL: 3,
  NGHTTP2_STREAM_STATE_RESERVED_REMOTE: 4, NGHTTP2_STREAM_STATE_HALF_CLOSED_LOCAL: 5,
  NGHTTP2_STREAM_STATE_HALF_CLOSED_REMOTE: 6, NGHTTP2_STREAM_STATE_CLOSED: 7, NGHTTP2_FLAG_NONE: 0,
  NGHTTP2_FLAG_END_STREAM: 1, NGHTTP2_FLAG_END_HEADERS: 4, NGHTTP2_FLAG_ACK: 1, NGHTTP2_FLAG_PADDED: 8,
  NGHTTP2_FLAG_PRIORITY: 32, DEFAULT_SETTINGS_HEADER_TABLE_SIZE: 4096, DEFAULT_SETTINGS_ENABLE_PUSH: 1,
  DEFAULT_SETTINGS_MAX_CONCURRENT_STREAMS: 4294967295, DEFAULT_SETTINGS_INITIAL_WINDOW_SIZE: 65535,
  DEFAULT_SETTINGS_MAX_FRAME_SIZE: 16384, DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE: 65535,
  DEFAULT_SETTINGS_ENABLE_CONNECT_PROTOCOL: 0, MAX_MAX_FRAME_SIZE: 16777215, MIN_MAX_FRAME_SIZE: 16384,
  MAX_INITIAL_WINDOW_SIZE: 2147483647, NGHTTP2_SETTINGS_HEADER_TABLE_SIZE: 1, NGHTTP2_SETTINGS_ENABLE_PUSH: 2,
  NGHTTP2_SETTINGS_MAX_CONCURRENT_STREAMS: 3, NGHTTP2_SETTINGS_INITIAL_WINDOW_SIZE: 4,
  NGHTTP2_SETTINGS_MAX_FRAME_SIZE: 5, NGHTTP2_SETTINGS_MAX_HEADER_LIST_SIZE: 6,
  NGHTTP2_SETTINGS_ENABLE_CONNECT_PROTOCOL: 8, PADDING_STRATEGY_NONE: 0, PADDING_STRATEGY_ALIGNED: 1,
  PADDING_STRATEGY_MAX: 2, PADDING_STRATEGY_CALLBACK: 1, NGHTTP2_NO_ERROR: 0, NGHTTP2_PROTOCOL_ERROR: 1,
  NGHTTP2_INTERNAL_ERROR: 2, NGHTTP2_FLOW_CONTROL_ERROR: 3, NGHTTP2_SETTINGS_TIMEOUT: 4,
  NGHTTP2_STREAM_CLOSED: 5, NGHTTP2_FRAME_SIZE_ERROR: 6, NGHTTP2_REFUSED_STREAM: 7, NGHTTP2_CANCEL: 8,
  NGHTTP2_COMPRESSION_ERROR: 9, NGHTTP2_CONNECT_ERROR: 10, NGHTTP2_ENHANCE_YOUR_CALM: 11,
  NGHTTP2_INADEQUATE_SECURITY: 12, NGHTTP2_HTTP_1_1_REQUIRED: 13, NGHTTP2_DEFAULT_WEIGHT: 16,
  HTTP2_HEADER_STATUS: ":status", HTTP2_HEADER_METHOD: ":method", HTTP2_HEADER_AUTHORITY: ":authority",
  HTTP2_HEADER_SCHEME: ":scheme", HTTP2_HEADER_PATH: ":path", HTTP2_HEADER_PROTOCOL: ":protocol",
  HTTP2_HEADER_ACCEPT_ENCODING: "accept-encoding", HTTP2_HEADER_ACCEPT_LANGUAGE: "accept-language",
  HTTP2_HEADER_ACCEPT_RANGES: "accept-ranges", HTTP2_HEADER_ACCEPT: "accept",
  HTTP2_HEADER_ACCESS_CONTROL_ALLOW_CREDENTIALS: "access-control-allow-credentials",
  HTTP2_HEADER_ACCESS_CONTROL_ALLOW_HEADERS: "access-control-allow-headers",
  HTTP2_HEADER_ACCESS_CONTROL_ALLOW_METHODS: "access-control-allow-methods",
  HTTP2_HEADER_ACCESS_CONTROL_ALLOW_ORIGIN: "access-control-allow-origin",
  HTTP2_HEADER_ACCESS_CONTROL_EXPOSE_HEADERS: "access-control-expose-headers",
  HTTP2_HEADER_ACCESS_CONTROL_REQUEST_HEADERS: "access-control-request-headers",
  HTTP2_HEADER_ACCESS_CONTROL_REQUEST_METHOD: "access-control-request-method", HTTP2_HEADER_AGE: "age",
  HTTP2_HEADER_AUTHORIZATION: "authorization", HTTP2_HEADER_CACHE_CONTROL: "cache-control",
  HTTP2_HEADER_CONNECTION: "connection", HTTP2_HEADER_CONTENT_DISPOSITION: "content-disposition",
  HTTP2_HEADER_CONTENT_ENCODING: "content-encoding", HTTP2_HEADER_CONTENT_LENGTH: "content-length",
  HTTP2_HEADER_CONTENT_TYPE: "content-type", HTTP2_HEADER_COOKIE: "cookie", HTTP2_HEADER_DATE: "date",
  HTTP2_HEADER_ETAG: "etag", HTTP2_HEADER_FORWARDED: "forwarded", HTTP2_HEADER_HOST: "host",
  HTTP2_HEADER_IF_MODIFIED_SINCE: "if-modified-since", HTTP2_HEADER_IF_NONE_MATCH: "if-none-match",
  HTTP2_HEADER_IF_RANGE: "if-range", HTTP2_HEADER_LAST_MODIFIED: "last-modified", HTTP2_HEADER_LINK: "link",
  HTTP2_HEADER_LOCATION: "location", HTTP2_HEADER_RANGE: "range", HTTP2_HEADER_REFERER: "referer",
  HTTP2_HEADER_SERVER: "server", HTTP2_HEADER_SET_COOKIE: "set-cookie",
  HTTP2_HEADER_STRICT_TRANSPORT_SECURITY: "strict-transport-security",
  HTTP2_HEADER_TRANSFER_ENCODING: "transfer-encoding", HTTP2_HEADER_TE: "te",
  HTTP2_HEADER_UPGRADE_INSECURE_REQUESTS: "upgrade-insecure-requests", HTTP2_HEADER_UPGRADE: "upgrade",
  HTTP2_HEADER_USER_AGENT: "user-agent", HTTP2_HEADER_VARY: "vary",
  HTTP2_HEADER_X_CONTENT_TYPE_OPTIONS: "x-content-type-options",
  HTTP2_HEADER_X_FRAME_OPTIONS: "x-frame-options", HTTP2_HEADER_KEEP_ALIVE: "keep-alive",
  HTTP2_HEADER_PROXY_CONNECTION: "proxy-connection", HTTP2_HEADER_X_XSS_PROTECTION: "x-xss-protection",
  HTTP2_HEADER_ALT_SVC: "alt-svc", HTTP2_HEADER_CONTENT_SECURITY_POLICY: "content-security-policy",
  HTTP2_HEADER_EARLY_DATA: "early-data", HTTP2_HEADER_EXPECT_CT: "expect-ct", HTTP2_HEADER_ORIGIN: "origin",
  HTTP2_HEADER_PURPOSE: "purpose", HTTP2_HEADER_TIMING_ALLOW_ORIGIN: "timing-allow-origin",
  HTTP2_HEADER_X_FORWARDED_FOR: "x-forwarded-for", HTTP2_HEADER_PRIORITY: "priority",
  HTTP2_HEADER_ACCEPT_CHARSET: "accept-charset", HTTP2_HEADER_ACCESS_CONTROL_MAX_AGE: "access-control-max-age",
  HTTP2_HEADER_ALLOW: "allow", HTTP2_HEADER_CONTENT_LANGUAGE: "content-language",
  HTTP2_HEADER_CONTENT_LOCATION: "content-location", HTTP2_HEADER_CONTENT_MD5: "content-md5",
  HTTP2_HEADER_CONTENT_RANGE: "content-range", HTTP2_HEADER_DNT: "dnt", HTTP2_HEADER_EXPECT: "expect",
  HTTP2_HEADER_EXPIRES: "expires", HTTP2_HEADER_FROM: "from", HTTP2_HEADER_IF_MATCH: "if-match",
  HTTP2_HEADER_IF_UNMODIFIED_SINCE: "if-unmodified-since", HTTP2_HEADER_MAX_FORWARDS: "max-forwards",
  HTTP2_HEADER_PREFER: "prefer", HTTP2_HEADER_PROXY_AUTHENTICATE: "proxy-authenticate",
  HTTP2_HEADER_PROXY_AUTHORIZATION: "proxy-authorization", HTTP2_HEADER_REFRESH: "refresh",
  HTTP2_HEADER_RETRY_AFTER: "retry-after", HTTP2_HEADER_TRAILER: "trailer", HTTP2_HEADER_TK: "tk",
  HTTP2_HEADER_VIA: "via", HTTP2_HEADER_WARNING: "warning", HTTP2_HEADER_WWW_AUTHENTICATE: "www-authenticate",
  HTTP2_HEADER_HTTP2_SETTINGS: "http2-settings", HTTP2_METHOD_ACL: "ACL",
  HTTP2_METHOD_BASELINE_CONTROL: "BASELINE-CONTROL", HTTP2_METHOD_BIND: "BIND",
  HTTP2_METHOD_CHECKIN: "CHECKIN", HTTP2_METHOD_CHECKOUT: "CHECKOUT", HTTP2_METHOD_CONNECT: "CONNECT",
  HTTP2_METHOD_COPY: "COPY", HTTP2_METHOD_DELETE: "DELETE", HTTP2_METHOD_GET: "GET", HTTP2_METHOD_HEAD: "HEAD",
  HTTP2_METHOD_LABEL: "LABEL", HTTP2_METHOD_LINK: "LINK", HTTP2_METHOD_LOCK: "LOCK",
  HTTP2_METHOD_MERGE: "MERGE", HTTP2_METHOD_MKACTIVITY: "MKACTIVITY", HTTP2_METHOD_MKCALENDAR: "MKCALENDAR",
  HTTP2_METHOD_MKCOL: "MKCOL", HTTP2_METHOD_MKREDIRECTREF: "MKREDIRECTREF",
  HTTP2_METHOD_MKWORKSPACE: "MKWORKSPACE", HTTP2_METHOD_MOVE: "MOVE", HTTP2_METHOD_OPTIONS: "OPTIONS",
  HTTP2_METHOD_ORDERPATCH: "ORDERPATCH", HTTP2_METHOD_PATCH: "PATCH", HTTP2_METHOD_POST: "POST",
  HTTP2_METHOD_PRI: "PRI", HTTP2_METHOD_PROPFIND: "PROPFIND", HTTP2_METHOD_PROPPATCH: "PROPPATCH",
  HTTP2_METHOD_PUT: "PUT", HTTP2_METHOD_REBIND: "REBIND", HTTP2_METHOD_REPORT: "REPORT",
  HTTP2_METHOD_SEARCH: "SEARCH", HTTP2_METHOD_TRACE: "TRACE", HTTP2_METHOD_UNBIND: "UNBIND",
  HTTP2_METHOD_UNCHECKOUT: "UNCHECKOUT", HTTP2_METHOD_UNLINK: "UNLINK", HTTP2_METHOD_UNLOCK: "UNLOCK",
  HTTP2_METHOD_UPDATE: "UPDATE", HTTP2_METHOD_UPDATEREDIRECTREF: "UPDATEREDIRECTREF",
  HTTP2_METHOD_VERSION_CONTROL: "VERSION-CONTROL", HTTP_STATUS_CONTINUE: 100,
  HTTP_STATUS_SWITCHING_PROTOCOLS: 101, HTTP_STATUS_PROCESSING: 102, HTTP_STATUS_EARLY_HINTS: 103,
  HTTP_STATUS_OK: 200, HTTP_STATUS_CREATED: 201, HTTP_STATUS_ACCEPTED: 202,
  HTTP_STATUS_NON_AUTHORITATIVE_INFORMATION: 203, HTTP_STATUS_NO_CONTENT: 204, HTTP_STATUS_RESET_CONTENT: 205,
  HTTP_STATUS_PARTIAL_CONTENT: 206, HTTP_STATUS_MULTI_STATUS: 207, HTTP_STATUS_ALREADY_REPORTED: 208,
  HTTP_STATUS_IM_USED: 226, HTTP_STATUS_MULTIPLE_CHOICES: 300, HTTP_STATUS_MOVED_PERMANENTLY: 301,
  HTTP_STATUS_FOUND: 302, HTTP_STATUS_SEE_OTHER: 303, HTTP_STATUS_NOT_MODIFIED: 304,
  HTTP_STATUS_USE_PROXY: 305, HTTP_STATUS_TEMPORARY_REDIRECT: 307, HTTP_STATUS_PERMANENT_REDIRECT: 308,
  HTTP_STATUS_BAD_REQUEST: 400, HTTP_STATUS_UNAUTHORIZED: 401, HTTP_STATUS_PAYMENT_REQUIRED: 402,
  HTTP_STATUS_FORBIDDEN: 403, HTTP_STATUS_NOT_FOUND: 404, HTTP_STATUS_METHOD_NOT_ALLOWED: 405,
  HTTP_STATUS_NOT_ACCEPTABLE: 406, HTTP_STATUS_PROXY_AUTHENTICATION_REQUIRED: 407,
  HTTP_STATUS_REQUEST_TIMEOUT: 408, HTTP_STATUS_CONFLICT: 409, HTTP_STATUS_GONE: 410,
  HTTP_STATUS_LENGTH_REQUIRED: 411, HTTP_STATUS_PRECONDITION_FAILED: 412, HTTP_STATUS_PAYLOAD_TOO_LARGE: 413,
  HTTP_STATUS_URI_TOO_LONG: 414, HTTP_STATUS_UNSUPPORTED_MEDIA_TYPE: 415,
  HTTP_STATUS_RANGE_NOT_SATISFIABLE: 416, HTTP_STATUS_EXPECTATION_FAILED: 417, HTTP_STATUS_TEAPOT: 418,
  HTTP_STATUS_MISDIRECTED_REQUEST: 421, HTTP_STATUS_UNPROCESSABLE_ENTITY: 422, HTTP_STATUS_LOCKED: 423,
  HTTP_STATUS_FAILED_DEPENDENCY: 424, HTTP_STATUS_TOO_EARLY: 425, HTTP_STATUS_UPGRADE_REQUIRED: 426,
  HTTP_STATUS_PRECONDITION_REQUIRED: 428, HTTP_STATUS_TOO_MANY_REQUESTS: 429,
  HTTP_STATUS_REQUEST_HEADER_FIELDS_TOO_LARGE: 431, HTTP_STATUS_UNAVAILABLE_FOR_LEGAL_REASONS: 451,
  HTTP_STATUS_INTERNAL_SERVER_ERROR: 500, HTTP_STATUS_NOT_IMPLEMENTED: 501, HTTP_STATUS_BAD_GATEWAY: 502,
  HTTP_STATUS_SERVICE_UNAVAILABLE: 503, HTTP_STATUS_GATEWAY_TIMEOUT: 504,
  HTTP_STATUS_HTTP_VERSION_NOT_SUPPORTED: 505, HTTP_STATUS_VARIANT_ALSO_NEGOTIATES: 506,
  HTTP_STATUS_INSUFFICIENT_STORAGE: 507, HTTP_STATUS_LOOP_DETECTED: 508,
  HTTP_STATUS_BANDWIDTH_LIMIT_EXCEEDED: 509, HTTP_STATUS_NOT_EXTENDED: 510,
  HTTP_STATUS_NETWORK_AUTHENTICATION_REQUIRED: 511,
};

const EventEmitter = internal.EventEmitter;
const Buffer = internal.Buffer;
const { Duplex, Readable, Stream } = internal.requireBuiltin("stream");
const net = internal.requireBuiltin("net");

const sensitiveHeaders = Symbol("nodejs.http2.sensitiveHeaders");
const kNoPayloadMethods = new Set(["DELETE", "GET", "HEAD"]);
const kSingleValueHeaders = new Set([
  ":status", ":method", ":authority", ":scheme", ":path", ":protocol", "access-control-allow-credentials", "access-control-max-age",
  "access-control-request-method", "age", "authorization", "content-encoding", "content-language", "content-length", "content-location",
  "content-md5", "content-range", "content-type", "date", "dnt", "etag", "expires", "from", "host", "if-match", "if-modified-since",
  "if-none-match", "if-range", "if-unmodified-since", "last-modified", "location", "max-forwards", "proxy-authorization", "range",
  "referer", "retry-after", "tk", "upgrade-insecure-requests", "user-agent", "x-content-type-options",
]);
const kConnectionHeaders = new Set(["connection", "upgrade", "http2-settings", "te", "transfer-encoding", "keep-alive", "proxy-connection"]);
const kValidPseudoHeaders = new Set([":status", ":method", ":authority", ":scheme", ":path", ":protocol"]);
const nameForErrorCode = {};
for (const [k, v] of Object.entries(constants)) {
  if (k.startsWith("NGHTTP2_") && !k.startsWith("NGHTTP2_ERR_") && !k.startsWith("NGHTTP2_SESSION") && !k.startsWith("NGHTTP2_STREAM_STATE") &&
    !k.startsWith("NGHTTP2_FLAG") && !k.startsWith("NGHTTP2_SETTINGS") && !k.startsWith("NGHTTP2_DEFAULT") && !k.startsWith("NGHTTP2_HCAT") &&
    !k.startsWith("NGHTTP2_NV") && !k.startsWith("NGHTTP2_INITIAL") && !k.startsWith("NGHTTP2_MAX") && !k.startsWith("NGHTTP2_MIN") &&
    v >= 0 && v <= 13 && !(v in nameForErrorCode)) {
    nameForErrorCode[v] = k;
  }
}

function codeError(Ctor, code, message) {
  const err = new Ctor(message);
  err.code = code;
  return err;
}

function utcDate() {
  return new Date().toUTCString();
}

// Header object -> [name, value, ...] (pseudo-headers first), validated like Node's mapToHeaders.
function toHeaderPairs(headers) {
  const pseudo = [];
  const regular = [];
  const seenSingle = new Set();
  for (const key of Object.keys(headers)) {
    let value = headers[key];
    if (value === undefined || value === null) continue;
    const name = key.toLowerCase();
    if (name[0] === ":") {
      if (!kValidPseudoHeaders.has(name)) throw codeError(TypeError, "ERR_HTTP2_INVALID_PSEUDOHEADER", `"${name}" is an invalid pseudoheader or is used incorrectly`);
      if (Array.isArray(value)) {
        if (value.length > 1) throw codeError(TypeError, "ERR_HTTP2_HEADER_SINGLE_VALUE", `Header field "${name}" must only have a single value`);
        value = value[0];
      }
      pseudo.push(name, String(value));
      continue;
    }
    if (kConnectionHeaders.has(name) && !(name === "te" && String(value).toLowerCase() === "trailers")) {
      throw codeError(TypeError, "ERR_HTTP2_INVALID_CONNECTION_HEADERS", `HTTP/1 Connection specific headers are forbidden: "${name}"`);
    }
    if (Array.isArray(value)) {
      if (value.length > 1 && kSingleValueHeaders.has(name)) throw codeError(TypeError, "ERR_HTTP2_HEADER_SINGLE_VALUE", `Header field "${name}" must only have a single value`);
      for (const v of value) regular.push(name, String(v));
    } else {
      if (kSingleValueHeaders.has(name)) {
        if (seenSingle.has(name)) throw codeError(TypeError, "ERR_HTTP2_HEADER_SINGLE_VALUE", `Header field "${name}" must only have a single value`);
        seenSingle.add(name);
      }
      regular.push(name, String(value));
    }
  }
  return pseudo.concat(regular);
}

// [name, value, ...] -> header object (Node's toHeaderObject).
function toHeaderObject(pairs) {
  const obj = Object.create(null);
  for (let i = 0; i < pairs.length; i += 2) {
    const name = pairs[i];
    let value = pairs[i + 1];
    if (name === ":status") value = Number(value);
    const existing = obj[name];
    if (existing === undefined) {
      obj[name] = name === "set-cookie" ? [value] : value;
    } else if (!kSingleValueHeaders.has(name)) {
      if (name === "cookie") obj[name] = `${existing}; ${value}`;
      else if (name === "set-cookie") existing.push(value);
      else obj[name] = `${existing}, ${value}`;
    }
  }
  return obj;
}

const sessions = new Map();
internal.h2Event = (id, kind, ...args) => sessions.get(id)?._event(kind, ...args);

let pingIds = 0;
const defaultSettings = () => ({
  headerTableSize: 4096,
  enablePush: true,
  initialWindowSize: 65535,
  maxFrameSize: 16384,
  maxConcurrentStreams: 4294967295,
  maxHeaderSize: 65535,
  maxHeaderListSize: 65535,
  enableConnectProtocol: false,
});

class Http2Session extends EventEmitter {
  constructor(type, options, socket) {
    super();
    this._type = type;
    this._options = options ?? {};
    this.socket = socket;
    this._id = ops.h2Session(type === constants.NGHTTP2_SESSION_SERVER ? "server" : "client", this._options.settings ?? {});
    sessions.set(this._id, this);
    this._streams = new Map();
    this._pings = new Map();
    this.closed = false;
    this.destroyed = false;
    this.connecting = true;
    this.encrypted = Boolean(socket.encrypted);
    this.alpnProtocol = undefined;
    this.localSettings = { ...defaultSettings(), ...(this._options.settings ?? {}) };
    this.remoteSettings = defaultSettings();
    this.pendingSettingsAck = false;
    this._onData = (d) => ops.h2Feed(this._id, d);
    this._onEnd = () => ops.h2FeedEnd(this._id);
    this._onError = (err) => this.destroy(err);
    this._onSocketClose = () => {
      if (!this.destroyed) this.destroy();
    };
    socket.on("data", this._onData);
    socket.on("end", this._onEnd);
    socket.on("error", this._onError);
    socket.on("close", this._onSocketClose);
  }
  get type() {
    return this._type;
  }
  get originSet() {
    return this.encrypted ? [`https://${this._authority ?? ""}`] : undefined;
  }
  get state() {
    return {
      effectiveLocalWindowSize: 65535, effectiveRecvDataLength: 0, nextStreamID: 1, localWindowSize: 65535, lastProcStreamID: 0,
      remoteWindowSize: 65535, outboundQueueSize: 1, deflateDynamicTableSize: 0, inflateDynamicTableSize: 0,
    };
  }
  _event(kind, a, b, c, d) {
    switch (kind) {
      case "out":
        if (!this.socket.destroyed) this.socket.write(Buffer.from(a.buffer, a.byteOffset, a.byteLength));
        break;
      case "ready": {
        const stream = this._streams.get(a);
        if (stream) {
          stream.id = b;
          stream._pending = false;
          stream.emit("ready");
        }
        break;
      }
      case "response":
        this._streams.get(a)?._onResponse(toHeaderObject(b), b, c);
        break;
      case "stream":
        this._onStream(a, b, c, d);
        break;
      case "data":
        this._streams.get(a)?._onData(b);
        break;
      case "trailers":
        this._streams.get(a)?._onTrailers(toHeaderObject(b), b);
        break;
      case "end":
        this._streams.get(a)?._onEnd();
        break;
      case "streamError":
        this._streams.get(a)?._onStreamError(b, c);
        break;
      case "ping": {
        const cb = this._pings.get(a);
        this._pings.delete(a);
        if (cb) cb(b ? null : codeError(Error, "ERR_HTTP2_PING_CANCEL", "HTTP2 ping cancelled"), c, Buffer.from(d.buffer, d.byteOffset, d.byteLength));
        break;
      }
      case "error":
        this.destroy(codeError(Error, "ERR_HTTP2_ERROR", String(a)));
        break;
      case "close":
        if (c) this.emit("goaway", a, 0, Buffer.alloc(0));
        for (const stream of this._streams.values()) {
          if (!stream.destroyed) stream._onStreamError(a === 0 ? constants.NGHTTP2_CANCEL : a, b);
        }
        if (a !== 0 && !this.destroyed && !c) this.destroy(codeError(Error, "ERR_HTTP2_SESSION_ERROR", `Session closed with error code ${a}`));
        else this.destroy();
        break;
    }
  }
  _maybeEnd() {
    if (this.closed && this._streams.size === 0 && !this.destroyed) this.destroy();
  }
  close(callback) {
    if (this.closed || this.destroyed) return;
    this.closed = true;
    if (typeof callback === "function") this.once("close", callback);
    ops.h2Close(this._id);
    if (this._streams.size === 0) process.nextTick(() => this._maybeEnd());
  }
  destroy(error = constants.NGHTTP2_NO_ERROR, code) {
    if (this.destroyed) return;
    if (typeof error === "number") {
      code = error;
      error = code !== constants.NGHTTP2_NO_ERROR ? codeError(Error, "ERR_HTTP2_SESSION_ERROR", `Session closed with error code ${code}`) : undefined;
    }
    this.destroyed = true;
    this.closed = true;
    for (const stream of this._streams.values()) stream.destroy(error);
    ops.h2Destroy(this._id);
    sessions.delete(this._id);
    this.socket.off("data", this._onData);
    this.socket.off("end", this._onEnd);
    if (!this.socket.destroyed) this.socket.destroy();
    process.nextTick(() => {
      if (error) this.emit("error", error);
      this.emit("close");
    });
  }
  goaway(code = constants.NGHTTP2_NO_ERROR) {
    this.close();
  }
  ping(payload, callback) {
    if (typeof payload === "function") {
      callback = payload;
      payload = undefined;
    }
    if (payload !== undefined && !(ArrayBuffer.isView(payload) || payload instanceof ArrayBuffer)) {
      throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "payload" argument must be an instance of Buffer, TypedArray, or DataView. Received ${internal.describeReceived(payload)}`);
    }
    if (payload !== undefined && payload.byteLength !== 8) throw codeError(RangeError, "ERR_HTTP2_PING_LENGTH", "HTTP2 ping payload must be 8 bytes");
    if (typeof callback !== "function") throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "callback" argument must be of type function. Received ${internal.describeReceived(callback)}`);
    const id = ++pingIds;
    this._pings.set(id, callback);
    return ops.h2Ping(this._id, payload ?? Buffer.alloc(8), id);
  }
  settings(settings, callback) {
    Object.assign(this.localSettings, settings ?? {});
    if (typeof callback === "function") process.nextTick(() => callback(null, this.localSettings, 0));
    process.nextTick(() => this.emit("localSettings", this.localSettings));
  }
  setLocalWindowSize() {}
  setTimeout(ms, callback) {
    this.socket.setTimeout(ms, callback ? () => callback.call(this) : undefined);
    return this;
  }
  ref() {
    this.socket.ref?.();
  }
  unref() {
    this.socket.unref?.();
  }
  [Symbol.asyncDispose]() {
    return new Promise((resolve) => this.close(resolve));
  }
}

class ClientHttp2Session extends Http2Session {
  constructor(options, socket, authority, scheme) {
    super(constants.NGHTTP2_SESSION_CLIENT, options, socket);
    this._authority = authority;
    this._scheme = scheme;
  }
  request(headers = {}, options = {}) {
    if (this.destroyed) throw codeError(Error, "ERR_HTTP2_INVALID_SESSION", "The session has been destroyed");
    if (this.closed) throw codeError(Error, "ERR_HTTP2_GOAWAY_SESSION", "New streams cannot be created after receiving a GOAWAY");
    if (headers === null || typeof headers !== "object") throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "headers" argument must be of type object. Received ${internal.describeReceived(headers)}`);
    headers = Object.assign(Object.create(null), headers);
    if (headers[":method"] === undefined) headers[":method"] = "GET";
    const method = String(headers[":method"]);
    const connect = method === "CONNECT";
    if (!connect || headers[":protocol"] !== undefined) {
      if (headers[":authority"] === undefined && headers.host === undefined) headers[":authority"] = this._authority;
      if (headers[":scheme"] === undefined) headers[":scheme"] = this._scheme;
      if (headers[":path"] === undefined) headers[":path"] = "/";
    } else if (headers[":authority"] === undefined) {
      throw codeError(TypeError, "ERR_HTTP2_CONNECT_AUTHORITY", ":authority header is required for CONNECT requests");
    }
    options = { ...options };
    if (options.endStream === undefined) options.endStream = kNoPayloadMethods.has(method);
    const pairs = toHeaderPairs(headers);
    const stream = new ClientHttp2Stream(this, options);
    stream.sentHeaders = headers;
    stream._sid = ops.h2Request(this._id, pairs, Boolean(options.endStream) && !options.waitForTrailers);
    if (options.endStream && !options.waitForTrailers) stream._endSent = true;
    this._streams.set(stream._sid, stream);
    if (options.signal) {
      const abort = () => stream.destroy(Object.assign(new internal.DOMException("The operation was aborted", "AbortError"), { code: "ABORT_ERR", cause: options.signal.reason }));
      if (options.signal.aborted) process.nextTick(abort);
      else options.signal.addEventListener("abort", abort, { once: true });
    }
    if (options.endStream) stream.end();
    return stream;
  }
}

class ServerHttp2Session extends Http2Session {
  constructor(options, socket, server) {
    super(constants.NGHTTP2_SESSION_SERVER, options, socket);
    this.server = server;
  }
  _onStream(sid, pairs, endStream, h2id) {
    const headers = toHeaderObject(pairs);
    const stream = new ServerHttp2Stream(this, sid, h2id, headers);
    this._streams.set(sid, stream);
    const flags = constants.NGHTTP2_FLAG_END_HEADERS | (endStream ? constants.NGHTTP2_FLAG_END_STREAM : 0);
    this.emit("stream", stream, headers, flags, pairs);
  }
  altsvc() {}
  origin() {}
}

class Http2Stream extends Duplex {
  constructor(session, options = {}) {
    super({ allowHalfOpen: true, decodeStrings: false, ...options, objectMode: false });
    this.session = session;
    this.id = undefined;
    this._pending = true;
    this.rstCode = undefined;
    this.sentHeaders = undefined;
    this.sentTrailers = undefined;
    this.sentInfoHeaders = undefined;
    this.aborted = false;
    this._streamClosed = false;
    this._waitForTrailers = Boolean(options.waitForTrailers);
    this.on("close", () => this._cleanup());
  }
  get pending() {
    return this._pending;
  }
  get closed() {
    return this._streamClosed;
  }
  get bufferSize() {
    return 0;
  }
  get endAfterHeaders() {
    return Boolean(this._endAfterHeaders);
  }
  get state() {
    return { localWindowSize: 65535, state: this._streamClosed ? constants.NGHTTP2_STREAM_STATE_CLOSED : constants.NGHTTP2_STREAM_STATE_OPEN, localClose: 0, remoteClose: 0, sumDependencyWeight: 0, weight: 16 };
  }
  _cleanup() {
    if (this._cleaned) return;
    this._cleaned = true;
    this._streamClosed = true;
    if (this.rstCode === undefined) this.rstCode = constants.NGHTTP2_NO_ERROR;
    ops.h2StreamDone(this._sid);
    this.session._streams.delete(this._sid);
    this.session._maybeEnd?.();
  }
  _read() {}
  _write(chunk, encoding, callback) {
    this._beforeWrite?.();
    const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, encoding);
    ops.h2Write(this._sid, buf, false);
    callback();
  }
  _final(callback) {
    this._beforeWrite?.();
    if (this._waitForTrailers) {
      ops.h2Write(this._sid, Buffer.alloc(0), false);
      process.nextTick(() => this.emit("wantTrailers"));
    } else if (!this._endSent) {
      this._endSent = true;
      ops.h2Write(this._sid, Buffer.alloc(0), true);
    }
    callback();
  }
  sendTrailers(headers) {
    if (this.destroyed || this._streamClosed) throw codeError(Error, "ERR_HTTP2_INVALID_STREAM", "The stream has been destroyed");
    if (this.sentTrailers) throw codeError(Error, "ERR_HTTP2_TRAILERS_ALREADY_SENT", "Trailing headers have already been sent");
    if (!this._waitForTrailers) throw codeError(Error, "ERR_HTTP2_TRAILERS_NOT_READY", "Trailing headers cannot be sent until after the wantTrailers event is emitted");
    this.sentTrailers = headers;
    this._endSent = true;
    ops.h2Trailers(this._sid, toHeaderPairs(headers ?? {}));
  }
  _onData(u8) {
    this.push(Buffer.from(u8.buffer, u8.byteOffset, u8.byteLength));
  }
  _onTrailers(headers, raw) {
    this.emit("trailers", headers, constants.NGHTTP2_FLAG_END_STREAM | constants.NGHTTP2_FLAG_END_HEADERS, raw);
  }
  _onEnd() {
    this.push(null);
    if (this.writableFinished || this._writableState?.finished) this._streamClosed = true;
  }
  _onStreamError(code, message) {
    if (this.destroyed) return;
    this.rstCode = code;
    this._streamClosed = true;
    if (code === constants.NGHTTP2_NO_ERROR) {
      this.destroy();
      return;
    }
    if (code === constants.NGHTTP2_CANCEL || code === constants.NGHTTP2_REFUSED_STREAM) this.aborted = true;
    if (this.aborted) this.emit("aborted");
    this.destroy(codeError(Error, "ERR_HTTP2_STREAM_ERROR", `Stream closed with error code ${nameForErrorCode[code] ?? code}`));
  }
  close(code = constants.NGHTTP2_NO_ERROR, callback) {
    if (typeof code !== "number" || code < 0 || code > 4294967295) {
      throw codeError(RangeError, "ERR_OUT_OF_RANGE", `The value of "code" is out of range. It must be >= 0 && <= 4294967295. Received ${code}`);
    }
    if (callback !== undefined && typeof callback !== "function") throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "callback" argument must be of type function. Received ${internal.describeReceived(callback)}`);
    if (this._streamClosed) return;
    if (callback) this.once("close", callback);
    this.rstCode = code;
    this._streamClosed = true;
    ops.h2Reset(this._sid, code);
    if (code !== constants.NGHTTP2_NO_ERROR) {
      this.destroy(codeError(Error, "ERR_HTTP2_STREAM_ERROR", `Stream closed with error code ${nameForErrorCode[code] ?? code}`));
    } else {
      this.push(null);
      this.destroy();
    }
  }
  destroy(err, callback) {
    if (!this._streamClosed && !this.destroyed && this._sid !== undefined) {
      this._streamClosed = true;
      if (this.rstCode === undefined) this.rstCode = constants.NGHTTP2_CANCEL;
      ops.h2Reset(this._sid, constants.NGHTTP2_CANCEL);
    }
    return super.destroy(err, callback);
  }
  priority() {}
  setTimeout(ms, callback) {
    if (this._timer) clearTimeout(this._timer);
    if (callback) this.once("timeout", callback);
    if (ms > 0) {
      this._timer = setTimeout(() => this.emit("timeout"), ms);
      this._timer.unref();
    }
    return this;
  }
}

class ClientHttp2Stream extends Http2Stream {
  constructor(session, options) {
    super(session, options);
  }
  _onResponse(headers, raw, endStream) {
    const status = headers[":status"];
    if (status >= 100 && status < 200) {
      this.emit("headers", headers, constants.NGHTTP2_FLAG_END_HEADERS, raw);
      return;
    }
    this._endAfterHeaders = endStream;
    this.emit("response", headers, constants.NGHTTP2_FLAG_END_HEADERS | (endStream ? constants.NGHTTP2_FLAG_END_STREAM : 0), raw);
  }
}

class ServerHttp2Stream extends Http2Stream {
  constructor(session, sid, h2id, headers) {
    super(session, {});
    this._sid = sid;
    this.id = h2id;
    this._pending = false;
    this._requestHeaders = headers;
    this.headersSent = false;
    this._beforeWrite = () => {
      if (!this.headersSent) this.respond();
    };
  }
  get pushAllowed() {
    return false;
  }
  respond(headers = {}, options = {}) {
    if (this.destroyed || this._streamClosed) throw codeError(Error, "ERR_HTTP2_INVALID_STREAM", "The stream has been destroyed");
    if (this.headersSent) throw codeError(Error, "ERR_HTTP2_HEADERS_SENT", "Response has already been initiated.");
    headers = Object.assign(Object.create(null), headers);
    if (headers[":status"] === undefined) headers[":status"] = 200;
    const status = Number(headers[":status"]);
    if (!Number.isInteger(status) || status < 200 || status > 599) {
      if (status === 101) throw codeError(Error, "ERR_HTTP2_STATUS_101", "HTTP status code 101 (Switching Protocols) is forbidden in HTTP/2");
      if (status < 100 || status > 599 || !Number.isInteger(status)) throw codeError(RangeError, "ERR_HTTP2_STATUS_INVALID", `Invalid status code: ${headers[":status"]}`);
      throw codeError(RangeError, "ERR_HTTP2_INVALID_INFO_STATUS", `Invalid informational status code: ${status}`);
    }
    if (headers.date === undefined && headers.Date === undefined) headers.date = utcDate();
    let endStream = Boolean(options.endStream);
    if (this._requestHeaders[":method"] === "HEAD" || status === 204 || status === 304) endStream = true;
    this._waitForTrailers = Boolean(options.waitForTrailers);
    this.headersSent = true;
    this.sentHeaders = headers;
    ops.h2Respond(this._sid, toHeaderPairs(headers), endStream && !this._waitForTrailers);
    if (endStream) {
      this._endSent = !this._waitForTrailers;
      this.end();
    }
  }
  additionalHeaders(headers) {
    if (this.headersSent) throw codeError(Error, "ERR_HTTP2_HEADERS_AFTER_RESPOND", "Cannot specify additional headers after response initiated");
    this.sentInfoHeaders = [...(this.sentInfoHeaders ?? []), headers];
  }
  pushStream(headers, options, callback) {
    if (typeof options === "function") callback = options;
    if (typeof callback !== "function") throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "callback" argument must be of type function. Received ${internal.describeReceived(callback)}`);
    process.nextTick(() => callback(codeError(Error, "ERR_HTTP2_PUSH_DISABLED", "HTTP/2 client has disabled push streams")));
  }
  respondWithFD(fd, headers = {}, options = {}) {
    const fs = internal.requireBuiltin("fs");
    const fdNum = typeof fd === "number" ? fd : fd.fd;
    const stat = fs.fstatSync(fdNum);
    const offset = options.offset ?? 0;
    const length = options.length !== undefined && options.length >= 0 ? options.length : stat.size - offset;
    const data = Buffer.alloc(Math.max(0, length));
    fs.readSync(fdNum, data, 0, data.length, offset);
    this._respondWithData(data, headers, options, stat);
  }
  respondWithFile(path, headers = {}, options = {}) {
    const fs = internal.requireBuiltin("fs");
    let stat;
    let data;
    try {
      stat = fs.statSync(path);
      if (stat.isDirectory()) throw codeError(Error, "ERR_HTTP2_SEND_FILE", "Directories cannot be sent");
      const all = fs.readFileSync(path);
      const offset = options.offset ?? 0;
      const length = options.length !== undefined && options.length >= 0 ? options.length : all.length - offset;
      data = all.subarray(offset, offset + length);
    } catch (err) {
      if (typeof options.onError === "function") return options.onError(err);
      throw err;
    }
    this._respondWithData(data, headers, options, stat);
  }
  _respondWithData(data, headers, options, stat) {
    headers = { ...headers };
    if (typeof options.statCheck === "function" && options.statCheck(stat, headers, { offset: options.offset ?? 0, length: data.length }) === false) return;
    if (headers["content-length"] === undefined) headers["content-length"] = data.length;
    this.respond(headers, { waitForTrailers: options.waitForTrailers });
    this.end(data);
  }
}

// ---------------------------------------------------------------------------
// Compatibility API (Http2ServerRequest / Http2ServerResponse)
// ---------------------------------------------------------------------------

class Http2ServerRequest extends Readable {
  constructor(stream, headers, options, rawHeaders) {
    super({ autoDestroy: false, ...options });
    this.stream = stream;
    this.headers = headers;
    this.rawHeaders = rawHeaders;
    this.trailers = {};
    this.rawTrailers = [];
    this.aborted = false;
    this.complete = false;
    stream.on("data", (chunk) => {
      if (!this.push(chunk)) stream.pause();
    });
    stream.on("end", () => {
      this.complete = true;
      this.push(null);
    });
    stream.on("trailers", (trailers, flags, raw) => {
      this.trailers = trailers;
      this.rawTrailers = raw;
    });
    stream.on("aborted", () => {
      this.aborted = true;
      this.emit("aborted");
    });
    stream.on("close", () => this.emit("close"));
    stream.on("error", (err) => this.emit("error", err));
  }
  _read() {
    this.stream.resume();
  }
  get method() {
    return this.headers[":method"];
  }
  get authority() {
    return this.headers[":authority"];
  }
  get scheme() {
    return this.headers[":scheme"];
  }
  get url() {
    return this.headers[":path"];
  }
  set url(url) {
    this.headers[":path"] = url;
  }
  get httpVersion() {
    return "2.0";
  }
  get httpVersionMajor() {
    return 2;
  }
  get httpVersionMinor() {
    return 0;
  }
  get socket() {
    return this.stream.session?.socket;
  }
  get connection() {
    return this.socket;
  }
  setTimeout(ms, callback) {
    this.stream.setTimeout(ms, callback);
    return this;
  }
}

class Http2ServerResponse extends Stream {
  constructor(stream, options) {
    super(options);
    this.stream = stream;
    this._headers = Object.create(null);
    this._names = Object.create(null);
    this._trailers = Object.create(null);
    this.statusCode = 200;
    this.sendDate = true;
    this.finished = false;
    this.writable = true;
    stream.on("finish", () => this.emit("finish"));
    stream.on("close", () => this.emit("close"));
    stream.on("wantTrailers", () => stream.sendTrailers(this._trailers));
    stream.on("drain", () => this.emit("drain"));
  }
  get headersSent() {
    return this.stream.headersSent;
  }
  get statusMessage() {
    return "";
  }
  set statusMessage(v) {}
  get writableEnded() {
    return this.finished;
  }
  get writableFinished() {
    return this.stream.writableFinished;
  }
  get socket() {
    return this.stream.session?.socket;
  }
  get connection() {
    return this.socket;
  }
  get req() {
    return this._req;
  }
  setHeader(name, value) {
    if (this.headersSent) throw codeError(Error, "ERR_HTTP2_HEADERS_SENT", "Response has already been initiated.");
    const key = String(name).toLowerCase();
    this._headers[key] = value;
    this._names[key] = name;
    return this;
  }
  appendHeader(name, value) {
    const key = String(name).toLowerCase();
    const existing = this._headers[key];
    if (existing === undefined) return this.setHeader(name, value);
    this._headers[key] = [].concat(existing, value);
    return this;
  }
  getHeader(name) {
    return this._headers[String(name).toLowerCase()];
  }
  getHeaders() {
    return Object.assign(Object.create(null), this._headers);
  }
  getHeaderNames() {
    return Object.keys(this._headers);
  }
  hasHeader(name) {
    return String(name).toLowerCase() in this._headers;
  }
  removeHeader(name) {
    if (this.headersSent) throw codeError(Error, "ERR_HTTP2_HEADERS_SENT", "Response has already been initiated.");
    delete this._headers[String(name).toLowerCase()];
  }
  addTrailers(headers) {
    for (const [k, v] of Object.entries(headers)) this._trailers[k.toLowerCase()] = v;
  }
  writeHead(statusCode, statusMessage, headers) {
    if (typeof statusMessage === "object") headers = statusMessage;
    if (this.headersSent) throw codeError(Error, "ERR_HTTP2_HEADERS_SENT", "Response has already been initiated.");
    if (headers) {
      if (Array.isArray(headers)) {
        for (let i = 0; i < headers.length; i += 2) this.setHeader(headers[i], headers[i + 1]);
      } else {
        for (const [k, v] of Object.entries(headers)) this.setHeader(k, v);
      }
    }
    this.statusCode = statusCode;
    this._respond();
    return this;
  }
  _respond() {
    if (this.stream.headersSent) return;
    const headers = { ...this._headers, ":status": this.statusCode };
    if (!this.sendDate) headers.date = headers.date ?? undefined;
    this.stream.respond(headers, { waitForTrailers: true, endStream: false });
  }
  write(chunk, encoding, cb) {
    if (typeof encoding === "function") {
      cb = encoding;
      encoding = "utf8";
    }
    if (this.finished) {
      const err = codeError(Error, "ERR_STREAM_WRITE_AFTER_END", "write after end");
      if (typeof cb === "function") process.nextTick(cb, err);
      else process.nextTick(() => this.emit("error", err));
      return false;
    }
    this._respond();
    return this.stream.write(chunk, encoding, cb);
  }
  end(chunk, encoding, cb) {
    if (typeof chunk === "function") {
      cb = chunk;
      chunk = undefined;
    } else if (typeof encoding === "function") {
      cb = encoding;
      encoding = "utf8";
    }
    if (this.finished) return this;
    if (chunk !== undefined && chunk !== null) this.write(chunk, encoding);
    this.finished = true;
    this._respond();
    if (typeof cb === "function") this.stream.once("finish", cb);
    this.stream.end();
    return this;
  }
  setTimeout(ms, callback) {
    this.stream.setTimeout(ms, callback);
    return this;
  }
  createPushResponse(headers, callback) {
    process.nextTick(() => callback(codeError(Error, "ERR_HTTP2_PUSH_DISABLED", "HTTP/2 client has disabled push streams")));
  }
  writeContinue() {}
  writeEarlyHints() {}
  flushHeaders() {
    this._respond();
  }
}

// ---------------------------------------------------------------------------
// Servers
// ---------------------------------------------------------------------------

function onServerStream(server, stream, headers, flags, rawHeaders) {
  if (server.listenerCount("request") === 0) return;
  const req = new Http2ServerRequest(stream, headers, undefined, rawHeaders);
  const res = new Http2ServerResponse(stream);
  res._req = req;
  if (headers.expect !== undefined && server.listenerCount("checkContinue") > 0) {
    server.emit("checkContinue", req, res);
    return;
  }
  server.emit("request", req, res);
}

function attachServerSession(server, socket, options) {
  const session = new ServerHttp2Session(options, socket, server);
  session.alpnProtocol = socket.encrypted ? "h2" : "h2c";
  session.connecting = false;
  session.on("stream", (stream, headers, flags, raw) => {
    server.emit("stream", stream, headers, flags, raw);
    onServerStream(server, stream, headers, flags, raw);
  });
  session.on("error", (err) => {
    if (server.listenerCount("sessionError") > 0) server.emit("sessionError", err, session);
  });
  server._sessions.add(session);
  session.on("close", () => {
    server._sessions.delete(session);
    server._maybeClosed?.();
  });
  if (options.timeout) session.setTimeout(options.timeout);
  server.emit("session", session);
  return session;
}

function setupServer(server, options, onRequestHandler) {
  server._h2Options = options;
  server._sessions = new Set();
  if (typeof onRequestHandler === "function") server.on("request", onRequestHandler);
  server.timeout = 0;
  server.setTimeout = function setTimeout(ms, callback) {
    this.timeout = ms;
    if (callback) this.on("timeout", callback);
    return this;
  };
  const close = server.close;
  server.close = function (callback) {
    for (const session of this._sessions) session.close();
    return close.call(this, callback);
  };
}

class Http2Server extends net.Server {
  constructor(options, onRequestHandler) {
    if (typeof options === "function") {
      onRequestHandler = options;
      options = {};
    }
    options = { ...(options ?? {}) };
    super(options);
    setupServer(this, options, onRequestHandler);
    this.on("connection", (socket) => attachServerSession(this, socket, options));
  }
  updateSettings(settings) {
    Object.assign(this._h2Options.settings ?? (this._h2Options.settings = {}), settings);
  }
}

function createServer(options, onRequestHandler) {
  if (typeof options === "function") {
    onRequestHandler = options;
    options = {};
  }
  if (options !== undefined && (options === null || typeof options !== "object")) {
    throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options" argument must be of type object. Received ${internal.describeReceived(options)}`);
  }
  return new Http2Server(options, onRequestHandler);
}

function createSecureServer(options, onRequestHandler) {
  if (typeof options === "function") {
    onRequestHandler = options;
    options = {};
  }
  options = { ...(options ?? {}) };
  const tls = internal.requireBuiltin("tls");
  const alpn = options.allowHTTP1 ? ["h2", "http/1.1"] : ["h2"];
  const server = new tls.Server({ ...options, ALPNProtocols: alpn });
  setupServer(server, options, onRequestHandler);
  let http1;
  server.on("secureConnection", (socket) => {
    if (socket.alpnProtocol === "h2") {
      attachServerSession(server, socket, options);
      return;
    }
    if (options.allowHTTP1) {
      if (!http1) {
        http1 = internal.requireBuiltin("http").createServer((req, res) => server.emit("request", req, res));
      }
      http1.emit("connection", socket);
      return;
    }
    if (server.listenerCount("unknownProtocol") > 0) server.emit("unknownProtocol", socket);
    else socket.destroy();
  });
  return server;
}

function connect(authority, options, listener) {
  if (typeof options === "function") {
    listener = options;
    options = undefined;
  }
  options = { ...(options ?? {}) };
  let url;
  if (typeof authority === "string") url = new internal.URL(authority);
  else if (authority && typeof authority === "object" && typeof authority.protocol === "string") url = authority instanceof internal.URL ? authority : new internal.URL(`${authority.protocol}//${authority.host ?? authority.hostname}${authority.port ? `:${authority.port}` : ""}`);
  else throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "authority" argument must be of type string or an instance of URL. Received ${internal.describeReceived(authority)}`);
  const protocol = url.protocol;
  if (protocol !== "http:" && protocol !== "https:" && typeof options.createConnection !== "function") {
    throw codeError(Error, "ERR_HTTP2_UNSUPPORTED_PROTOCOL", `protocol "${protocol}" is unsupported.`);
  }
  const host = url.hostname.replace(/^\[|\]$/g, "");
  const port = url.port ? Number(url.port) : protocol === "https:" ? 443 : 80;
  let socket;
  if (typeof options.createConnection === "function") {
    socket = options.createConnection(url, options);
  } else if (protocol === "http:") {
    socket = net.connect({ port, host: options.host ?? host, localAddress: options.localAddress });
  } else {
    const tls = internal.requireBuiltin("tls");
    socket = tls.connect({ ...options, port, host: options.host ?? host, servername: options.servername ?? (net.isIP(host) ? undefined : host), ALPNProtocols: ["h2"] });
  }
  const scheme = protocol === "https:" || socket.encrypted ? "https" : "http";
  const defaultPort = scheme === "https" ? 443 : 80;
  const authorityValue = url.port && Number(url.port) !== defaultPort ? `${url.hostname}:${url.port}` : url.hostname;
  const session = new ClientHttp2Session(options, socket, authorityValue, scheme);
  if (typeof listener === "function") session.once("connect", listener);
  const onConnect = () => {
    session.connecting = false;
    session.encrypted = Boolean(socket.encrypted);
    session.alpnProtocol = socket.encrypted ? socket.alpnProtocol || "h2" : "h2c";
    if (socket.encrypted && socket.alpnProtocol !== "h2") {
      session.destroy(codeError(Error, "ERR_HTTP2_ERROR", "Protocol error"));
      return;
    }
    session.emit("connect", session, socket);
  };
  if (socket.encrypted !== undefined && typeof socket.alpnProtocol !== "undefined" && protocol === "https:") socket.once("secureConnect", onConnect);
  else if (socket.connecting === false && !socket.pending) process.nextTick(onConnect);
  else socket.once("connect", onConnect);
  return session;
}

function getPackedSettings(settings = {}) {
  const ids = { headerTableSize: 1, enablePush: 2, maxConcurrentStreams: 3, initialWindowSize: 4, maxFrameSize: 5, maxHeaderListSize: 6, maxHeaderSize: 6, enableConnectProtocol: 8 };
  const out = [];
  for (const [key, id] of [["headerTableSize", 1], ["enablePush", 2], ["initialWindowSize", 4], ["maxFrameSize", 5], ["maxConcurrentStreams", 3], ["maxHeaderListSize", 6], ["enableConnectProtocol", 8]]) {
    let value = settings[key];
    if (key === "maxHeaderListSize" && value === undefined) value = settings.maxHeaderSize;
    if (value === undefined) continue;
    const b = Buffer.alloc(6);
    b.writeUInt16BE(id, 0);
    b.writeUInt32BE(typeof value === "boolean" ? Number(value) : value >>> 0, 2);
    out.push(b);
  }
  void ids;
  return Buffer.concat(out);
}

function getUnpackedSettings(buf, options = {}) {
  if (!ArrayBuffer.isView(buf)) throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "buf" argument must be an instance of Buffer or TypedArray. Received ${internal.describeReceived(buf)}`);
  if (buf.length % 6 !== 0) throw codeError(RangeError, "ERR_HTTP2_INVALID_PACKED_SETTINGS_LENGTH", "Packed settings length must be a multiple of six");
  const b = Buffer.from(buf.buffer, buf.byteOffset, buf.byteLength);
  const out = {};
  for (let i = 0; i < b.length; i += 6) {
    const id = b.readUInt16BE(i);
    const value = b.readUInt32BE(i + 2);
    switch (id) {
      case 1: out.headerTableSize = value; break;
      case 2: out.enablePush = value !== 0; break;
      case 3: out.maxConcurrentStreams = value; break;
      case 4: out.initialWindowSize = value; break;
      case 5: out.maxFrameSize = value; break;
      case 6: out.maxHeaderListSize = out.maxHeaderSize = value; break;
      case 8: out.enableConnectProtocol = value !== 0; break;
    }
  }
  return out;
}

function performServerHandshake(socket, options = {}) {
  return new ServerHttp2Session(options, socket, undefined);
}

const http2 = {
  connect,
  constants,
  createServer,
  createSecureServer,
  getDefaultSettings: defaultSettings,
  getPackedSettings,
  getUnpackedSettings,
  performServerHandshake,
  sensitiveHeaders,
  Http2ServerRequest,
  Http2ServerResponse,
};
internal.defineBuiltin("http2", () => http2);
