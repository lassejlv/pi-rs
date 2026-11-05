// node:http (HTTP/1.1 server and client over node:net) and node:https (the same over node:tls).

const EventEmitter = internal.EventEmitter;
const Buffer = internal.Buffer;
const { Readable, Writable } = internal.requireBuiltin("stream");
const net = internal.requireBuiltin("net");

const METHODS = [
  "ACL", "BIND", "CHECKOUT", "CONNECT", "COPY", "DELETE", "GET", "HEAD", "LINK", "LOCK", "M-SEARCH", "MERGE",
  "MKACTIVITY", "MKCALENDAR", "MKCOL", "MOVE", "NOTIFY", "OPTIONS", "PATCH", "POST", "PROPFIND", "PROPPATCH",
  "PURGE", "PUT", "QUERY", "REBIND", "REPORT", "SEARCH", "SOURCE", "SUBSCRIBE", "TRACE", "UNBIND", "UNLINK",
  "UNLOCK", "UNSUBSCRIBE",
];

const STATUS_CODES = {
  100: "Continue", 101: "Switching Protocols", 102: "Processing", 103: "Early Hints", 200: "OK", 201: "Created",
  202: "Accepted", 203: "Non-Authoritative Information", 204: "No Content", 205: "Reset Content",
  206: "Partial Content", 207: "Multi-Status", 208: "Already Reported", 226: "IM Used", 300: "Multiple Choices",
  301: "Moved Permanently", 302: "Found", 303: "See Other", 304: "Not Modified", 305: "Use Proxy",
  307: "Temporary Redirect", 308: "Permanent Redirect", 400: "Bad Request", 401: "Unauthorized",
  402: "Payment Required", 403: "Forbidden", 404: "Not Found", 405: "Method Not Allowed", 406: "Not Acceptable",
  407: "Proxy Authentication Required", 408: "Request Timeout", 409: "Conflict", 410: "Gone",
  411: "Length Required", 412: "Precondition Failed", 413: "Payload Too Large", 414: "URI Too Long",
  415: "Unsupported Media Type", 416: "Range Not Satisfiable", 417: "Expectation Failed", 418: "I'm a Teapot",
  421: "Misdirected Request", 422: "Unprocessable Entity", 423: "Locked", 424: "Failed Dependency",
  425: "Too Early", 426: "Upgrade Required", 428: "Precondition Required", 429: "Too Many Requests",
  431: "Request Header Fields Too Large", 451: "Unavailable For Legal Reasons", 500: "Internal Server Error",
  501: "Not Implemented", 502: "Bad Gateway", 503: "Service Unavailable", 504: "Gateway Timeout",
  505: "HTTP Version Not Supported", 506: "Variant Also Negotiates", 507: "Insufficient Storage",
  508: "Loop Detected", 509: "Bandwidth Limit Exceeded", 510: "Not Extended",
  511: "Network Authentication Required",
};

const CRLF = "\r\n";

// ---------------------------------------------------------------------------
// HTTP/1.x message parser
// ---------------------------------------------------------------------------

/**
 * Incremental parser for one direction of a connection. `kind` is "request" or "response".
 * Callbacks: onHead(head) -> { skipBody?: boolean, stop?: boolean }, onBody(chunk), onEnd().
 */
class HttpParser {
  constructor(kind, callbacks) {
    this.kind = kind;
    this.cb = callbacks;
    this.buffer = Buffer.alloc(0);
    this.state = "head";
    this.remaining = 0;
    this.stopped = false;
  }

  execute(chunk) {
    if (this.stopped) return;
    this.buffer = this.buffer.length ? Buffer.concat([this.buffer, chunk]) : chunk;
    this._run();
  }

  /** The connection ended: a body delimited by close ends now. */
  finish() {
    if (this.state === "until-close") {
      this.state = "head";
      this.cb.onEnd();
    }
  }

  /** Remaining unparsed bytes (after a CONNECT / upgrade head). */
  takeRest() {
    const rest = this.buffer;
    this.buffer = Buffer.alloc(0);
    return rest;
  }

  _run() {
    while (!this.stopped) {
      if (this.state === "head") {
        const end = this.buffer.indexOf("\r\n\r\n");
        if (end === -1) {
          if (this.buffer.length > 80 * 1024) this._fail("Header overflow", "HPE_HEADER_OVERFLOW");
          return;
        }
        const text = this.buffer.subarray(0, end).toString("latin1");
        this.buffer = this.buffer.subarray(end + 4);
        // Tolerate empty lines before a request line.
        const lines = text.replace(/^(\r\n)+/, "").split(CRLF);
        const head = this._parseHead(lines);
        if (!head) return;
        const action = this.cb.onHead(head) ?? {};
        if (action.stop) {
          this.stopped = true;
          return;
        }
        const te = (head.headers["transfer-encoding"] ?? "").toLowerCase();
        if (action.skipBody) {
          this.cb.onEnd();
        } else if (te.split(",").map((s) => s.trim()).includes("chunked")) {
          this.state = "chunk-size";
        } else if (head.headers["content-length"] !== undefined) {
          this.remaining = Number.parseInt(head.headers["content-length"], 10) || 0;
          if (this.remaining === 0) this.cb.onEnd();
          else this.state = "length";
        } else if (this.kind === "response") {
          this.state = "until-close";
        } else {
          this.cb.onEnd();
        }
      } else if (this.state === "length") {
        if (this.buffer.length === 0) return;
        const take = Math.min(this.remaining, this.buffer.length);
        const piece = this.buffer.subarray(0, take);
        this.buffer = this.buffer.subarray(take);
        this.remaining -= take;
        this.cb.onBody(piece);
        if (this.remaining === 0) {
          this.state = "head";
          this.cb.onEnd();
        }
      } else if (this.state === "chunk-size") {
        const end = this.buffer.indexOf(CRLF);
        if (end === -1) return;
        const size = Number.parseInt(this.buffer.subarray(0, end).toString("latin1").split(";")[0].trim(), 16);
        this.buffer = this.buffer.subarray(end + 2);
        if (Number.isNaN(size)) return this._fail("Invalid character in chunk size", "HPE_INVALID_CHUNK_SIZE");
        if (size === 0) this.state = "trailers";
        else {
          this.remaining = size;
          this.state = "chunk-data";
        }
      } else if (this.state === "chunk-data") {
        if (this.remaining > 0) {
          if (this.buffer.length === 0) return;
          const take = Math.min(this.remaining, this.buffer.length);
          const piece = this.buffer.subarray(0, take);
          this.buffer = this.buffer.subarray(take);
          this.remaining -= take;
          this.cb.onBody(piece);
          if (this.remaining > 0) return;
        }
        if (this.buffer.length < 2) return;
        this.buffer = this.buffer.subarray(2);
        this.state = "chunk-size";
      } else if (this.state === "trailers") {
        const end = this.buffer.indexOf(CRLF);
        if (end === -1) return;
        const line = this.buffer.subarray(0, end).toString("latin1");
        this.buffer = this.buffer.subarray(end + 2);
        if (line === "") {
          this.state = "head";
          this.cb.onEnd();
        }
      } else if (this.state === "until-close") {
        if (this.buffer.length === 0) return;
        const piece = this.buffer;
        this.buffer = Buffer.alloc(0);
        this.cb.onBody(piece);
        return;
      } else {
        return;
      }
    }
  }

  _parseHead(lines) {
    const first = lines.shift() ?? "";
    const head = { headers: {}, rawHeaders: [] };
    if (this.kind === "request") {
      const m = /^([A-Z-]+) (\S+) HTTP\/(\d)\.(\d)$/.exec(first);
      if (!m) return this._fail("Invalid method encountered", "HPE_INVALID_METHOD");
      head.method = m[1];
      head.url = m[2];
      head.versionMajor = Number(m[3]);
      head.versionMinor = Number(m[4]);
    } else {
      const m = /^HTTP\/(\d)\.(\d) (\d{3})(?: (.*))?$/.exec(first);
      if (!m) return this._fail("Invalid HTTP status line", "HPE_INVALID_CONSTANT");
      head.versionMajor = Number(m[1]);
      head.versionMinor = Number(m[2]);
      head.statusCode = Number(m[3]);
      head.statusMessage = m[4] ?? "";
    }
    for (const line of lines) {
      const colon = line.indexOf(":");
      if (colon <= 0) continue;
      const name = line.slice(0, colon).trim();
      const value = line.slice(colon + 1).trim();
      head.rawHeaders.push(name, value);
      const key = name.toLowerCase();
      if (key === "set-cookie") (head.headers[key] ??= []).push(value);
      else if (head.headers[key] === undefined) head.headers[key] = value;
      else if (!["content-length", "content-type", "host", "user-agent", "authorization", "from", "referer", "location"].includes(key)) {
        head.headers[key] += (key === "cookie" ? "; " : ", ") + value;
      }
    }
    return head;
  }

  _fail(message, code) {
    this.stopped = true;
    const err = Object.assign(new Error(`Parse Error: ${message}`), { code });
    this.cb.onError?.(err);
    return null;
  }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

class IncomingMessage extends Readable {
  constructor(socket) {
    super();
    this.socket = socket;
    this.connection = socket;
    this.headers = {};
    this.rawHeaders = [];
    this.trailers = {};
    this.rawTrailers = [];
    this.httpVersion = "1.1";
    this.httpVersionMajor = 1;
    this.httpVersionMinor = 1;
    this.method = null;
    this.url = "";
    this.statusCode = null;
    this.statusMessage = null;
    this.complete = false;
    this.aborted = false;
  }

  _applyHead(head) {
    this.headers = head.headers;
    this.rawHeaders = head.rawHeaders;
    this.httpVersionMajor = head.versionMajor;
    this.httpVersionMinor = head.versionMinor;
    this.httpVersion = `${head.versionMajor}.${head.versionMinor}`;
    if (head.method !== undefined) {
      this.method = head.method;
      this.url = head.url;
    } else {
      this.statusCode = head.statusCode;
      this.statusMessage = head.statusMessage;
    }
  }

  get headersDistinct() {
    const out = {};
    for (let i = 0; i < this.rawHeaders.length; i += 2) (out[this.rawHeaders[i].toLowerCase()] ??= []).push(this.rawHeaders[i + 1]);
    return out;
  }

  _read() {
    this.socket?.resume?.();
  }

  setTimeout(ms, callback) {
    this.socket?.setTimeout(ms, callback);
    return this;
  }
}

/** Header storage shared by ServerResponse and ClientRequest (OutgoingMessage). */
class OutgoingMessage extends Writable {
  constructor() {
    super();
    this._headers = new Map();
    this.headersSent = false;
    this.sendDate = false;
    this.finished = false;
    this.chunkedEncoding = false;
    this._hasBody = true;
  }

  setHeader(name, value) {
    if (this.headersSent) {
      throw Object.assign(new Error("Cannot set headers after they are sent to the client"), { code: "ERR_HTTP_HEADERS_SENT" });
    }
    this._headers.set(String(name).toLowerCase(), [String(name), value]);
    return this;
  }

  appendHeader(name, value) {
    const key = String(name).toLowerCase();
    const existing = this._headers.get(key);
    if (!existing) return this.setHeader(name, value);
    const values = [].concat(existing[1], value);
    this._headers.set(key, [existing[0], values]);
    return this;
  }

  getHeader(name) {
    return this._headers.get(String(name).toLowerCase())?.[1];
  }

  getHeaders() {
    const out = Object.create(null);
    for (const [key, [, value]] of this._headers) out[key] = value;
    return out;
  }

  getHeaderNames() {
    return [...this._headers.keys()];
  }

  getRawHeaderNames() {
    return [...this._headers.values()].map(([name]) => name);
  }

  hasHeader(name) {
    return this._headers.has(String(name).toLowerCase());
  }

  removeHeader(name) {
    if (this.headersSent) {
      throw Object.assign(new Error("Cannot remove headers after they are sent to the client"), { code: "ERR_HTTP_HEADERS_SENT" });
    }
    this._headers.delete(String(name).toLowerCase());
  }

  _headerLines() {
    let out = "";
    for (const [, [name, value]] of this._headers) {
      for (const v of [].concat(value)) out += `${name}: ${v}${CRLF}`;
    }
    return out;
  }

  _writeBody(chunk, encoding) {
    if (!this._hasBody || chunk === undefined || chunk === null) return;
    const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, typeof encoding === "string" ? encoding : "utf8");
    if (buf.length === 0) return;
    if (this.chunkedEncoding) {
      this._send(Buffer.from(`${buf.length.toString(16)}${CRLF}`, "latin1"));
      this._send(buf);
      this._send(Buffer.from(CRLF, "latin1"));
    } else {
      this._send(buf);
    }
  }
}

class ServerResponse extends OutgoingMessage {
  constructor(req, socket, onFinished) {
    super();
    this.req = req;
    this.socket = socket;
    this.connection = socket;
    this.statusCode = 200;
    this.statusMessage = undefined;
    this.sendDate = true;
    this.strictContentLength = false;
    this._onFinished = onFinished;
    this._hasBody = req.method !== "HEAD";
    this._keepAlive = req.httpVersionMajor === 1 && req.httpVersionMinor >= 1
      ? (req.headers.connection ?? "").toLowerCase() !== "close"
      : (req.headers.connection ?? "").toLowerCase() === "keep-alive";
  }

  writeHead(statusCode, statusMessage, headers) {
    if (this.headersSent) {
      throw Object.assign(new Error("Cannot write headers after they are sent to the client"), { code: "ERR_HTTP_HEADERS_SENT" });
    }
    if (typeof statusMessage !== "string") {
      headers = statusMessage;
      statusMessage = undefined;
    }
    this.statusCode = statusCode;
    if (statusMessage !== undefined) this.statusMessage = statusMessage;
    if (Array.isArray(headers)) {
      for (let i = 0; i < headers.length; i += 2) this.appendHeader(headers[i], headers[i + 1]);
    } else if (headers) {
      for (const [name, value] of Object.entries(headers)) if (value !== undefined) this.setHeader(name, value);
    }
    this._sendHead();
    return this;
  }

  writeContinue() {
    this._send(Buffer.from(`HTTP/1.1 100 Continue${CRLF}${CRLF}`, "latin1"));
  }

  flushHeaders() {
    if (!this.headersSent) this._sendHead();
  }

  _send(buf) {
    if (!this.socket.destroyed) this.socket.write(buf);
  }

  _sendHead(contentLength) {
    if (this.headersSent) return;
    const code = this.statusCode;
    const noBody = code === 204 || code === 304 || (code >= 100 && code < 200);
    if (noBody) this._hasBody = false;
    if (this.sendDate && !this.hasHeader("date")) this.setHeader("Date", new Date().toUTCString());
    if (!this.hasHeader("connection")) this.setHeader("Connection", this._keepAlive ? "keep-alive" : "close");
    else if ((this.getHeader("connection") + "").toLowerCase() === "close") this._keepAlive = false;
    if (!noBody && !this.hasHeader("content-length") && !this.hasHeader("transfer-encoding")) {
      if (contentLength !== undefined) this.setHeader("Content-Length", contentLength);
      else if (this.req.httpVersionMajor === 1 && this.req.httpVersionMinor >= 1) {
        this.setHeader("Transfer-Encoding", "chunked");
      } else {
        this._keepAlive = false;
      }
    }
    this.chunkedEncoding = /chunked/i.test(String(this.getHeader("transfer-encoding") ?? ""));
    const message = this.statusMessage ?? STATUS_CODES[code] ?? "unknown";
    this.headersSent = true;
    this._send(Buffer.from(`HTTP/1.1 ${code} ${message}${CRLF}${this._headerLines()}${CRLF}`, "latin1"));
  }

  _write(chunk, encoding, callback) {
    if (!this.headersSent) this._sendHead();
    this._writeBody(chunk, encoding);
    callback();
  }

  end(chunk, encoding, callback) {
    if (typeof chunk === "function") {
      callback = chunk;
      chunk = undefined;
    } else if (typeof encoding === "function") {
      callback = encoding;
      encoding = undefined;
    }
    if (!this.headersSent) {
      const length = chunk === undefined || chunk === null ? 0 : Buffer.byteLength(chunk, typeof encoding === "string" ? encoding : "utf8");
      this._sendHead(this._hasBody ? length : undefined);
    }
    if (chunk !== undefined && chunk !== null) this.write(chunk, encoding);
    return super.end(callback);
  }

  _final(callback) {
    if (this.chunkedEncoding && this._hasBody) this._send(Buffer.from(`0${CRLF}${CRLF}`, "latin1"));
    this.finished = true;
    callback();
    this._onFinished?.(this._keepAlive);
  }

  setTimeout(ms, callback) {
    this.socket?.setTimeout(ms, callback);
    return this;
  }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

class Server extends net.Server {
  constructor(options, requestListener) {
    if (typeof options === "function") {
      requestListener = options;
      options = {};
    }
    super(options ?? {});
    this.timeout = 0;
    this.keepAliveTimeout = 5000;
    this.headersTimeout = 60000;
    this.requestTimeout = 300000;
    this.maxHeadersCount = null;
    if (requestListener) this.on("request", requestListener);
    this.on("connection", (socket) => this._handleConnection(socket));
  }

  setTimeout(ms, callback) {
    this.timeout = ms;
    if (callback) this.on("timeout", callback);
    return this;
  }

  _handleConnection(socket) {
    let req = null;
    let busy = false;
    const queue = [];
    const parser = new HttpParser("request", {
      onHead: (head) => {
        req = new IncomingMessage(socket);
        req._applyHead(head);
        if (head.method === "CONNECT" || (head.headers.upgrade && this.listenerCount("upgrade") > 0)) {
          const event = head.method === "CONNECT" ? "connect" : "upgrade";
          socket.removeListener("data", onData);
          socket.removeListener("end", onEnd);
          const rest = parser.takeRest();
          if (this.listenerCount(event) > 0) this.emit(event, req, socket, rest);
          else socket.destroy();
          return { stop: true };
        }
        const current = req;
        const res = new ServerResponse(current, socket, (keepAlive) => {
          busy = false;
          if (!keepAlive) socket.end();
          else if (queue.length) queue.shift()();
        });
        const dispatch = () => {
          busy = true;
          if (internal.dcPublish) {
            internal.dcPublish("http.server.request.start", { request: current, response: res, socket, server: this });
            res.once("finish", () => internal.dcPublish("http.server.response.finish", { request: current, response: res, socket, server: this }));
          }
          this.emit("request", current, res);
        };
        if (busy) queue.push(dispatch);
        else dispatch();
        return {};
      },
      onBody: (chunk) => req?.push(chunk),
      onEnd: () => {
        if (!req) return;
        req.complete = true;
        req.push(null);
      },
      onError: (err) => {
        if (this.listenerCount("clientError") > 0) this.emit("clientError", err, socket);
        else socket.end(`HTTP/1.1 400 Bad Request${CRLF}Connection: close${CRLF}${CRLF}`);
      },
    });
    const onData = (chunk) => parser.execute(chunk);
    const onEnd = () => parser.finish();
    socket.on("data", onData);
    socket.on("end", onEnd);
    socket.on("error", () => {});
    if (this.timeout) socket.setTimeout(this.timeout, () => this.emit("timeout", socket));
  }

  closeIdleConnections() {}
}

function createServer(options, requestListener) {
  return new Server(options, requestListener);
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

class Agent extends EventEmitter {
  constructor(options = {}) {
    super();
    this.options = { path: null, ...options };
    this.keepAlive = !!options.keepAlive;
    this.maxSockets = options.maxSockets ?? Infinity;
    this.maxFreeSockets = options.maxFreeSockets ?? 256;
    this.sockets = {};
    this.freeSockets = {};
    this.requests = {};
    this.defaultPort = 80;
    this.protocol = "http:";
  }
  destroy() {}
  getName(options = {}) {
    return `${options.host ?? "localhost"}:${options.port ?? ""}:${options.localAddress ?? ""}`;
  }
  createConnection(options, callback) {
    const socket = net.connect({ ...options, host: options.host ?? "localhost", port: options.port ?? this.defaultPort });
    if (typeof callback === "function") socket.once("connect", () => callback(null, socket));
    return socket;
  }
}

const globalAgent = new Agent();

function requestArgs(input, options, callback, defaultProtocol) {
  if (typeof options === "function") {
    callback = options;
    options = undefined;
  }
  let opts = {};
  if (typeof input === "string" || input instanceof URL) {
    const url = new URL(String(input));
    opts = {
      protocol: url.protocol,
      hostname: url.hostname.replace(/^\[|\]$/g, ""),
      port: url.port,
      path: `${url.pathname}${url.search}`,
      auth: url.username ? `${decodeURIComponent(url.username)}:${decodeURIComponent(url.password)}` : undefined,
    };
    opts = { ...opts, ...(options ?? {}) };
  } else {
    opts = { ...(input ?? {}) };
    if (typeof options === "object" && options) opts = { ...opts, ...options };
  }
  opts.protocol ??= defaultProtocol;
  return { opts, callback };
}

class ClientRequest extends OutgoingMessage {
  constructor(opts, callback) {
    super();
    this.method = String(opts.method ?? "GET").toUpperCase();
    this.path = opts.path ?? "/";
    this.host = opts.hostname ?? opts.host?.replace(/:\d+$/, "") ?? "localhost";
    this.protocol = opts.protocol ?? "http:";
    this.port = Number(opts.port) || (this.protocol === "https:" ? 443 : 80);
    this.agent = opts.agent;
    this.aborted = false;
    this.reusedSocket = false;
    this.maxHeadersCount = null;
    this._opts = opts;
    this._hasBody = true;
    this._timeoutMs = opts.timeout;
    if (callback) this.once("response", callback);
    // Requests over net sockets publish once their socket exists (see NetClientRequest).
    if (internal.dcPublish && !(this instanceof NetClientRequest)) process.nextTick(() => internal.dcPublish("http.client.request.start", { request: this }));
    const headers = opts.headers ?? {};
    if (Array.isArray(headers)) for (let i = 0; i < headers.length; i += 2) this.appendHeader(headers[i], headers[i + 1]);
    else for (const [name, value] of Object.entries(headers)) if (value !== undefined) this.setHeader(name, value);
    if (!this.hasHeader("host")) {
      const defaultPort = this.protocol === "https:" ? 443 : 80;
      const hostname = this.host.includes(":") ? `[${this.host}]` : this.host;
      this.setHeader("Host", this.port === defaultPort ? hostname : `${hostname}:${this.port}`);
    }
    if (opts.auth && !this.hasHeader("authorization")) {
      this.setHeader("Authorization", `Basic ${Buffer.from(opts.auth).toString("base64")}`);
    }
    if (opts.signal) {
      if (opts.signal.aborted) process.nextTick(() => this.destroy(abortError()));
      else opts.signal.addEventListener("abort", () => this.destroy(abortError()), { once: true });
    }
  }

  setTimeout(ms, callback) {
    this._timeoutMs = ms;
    if (callback) this.once("timeout", callback);
    this.socket?.setTimeout(ms);
    return this;
  }

  setNoDelay() {}
  setSocketKeepAlive() {}

  abort() {
    if (this.aborted) return;
    this.aborted = true;
    process.nextTick(() => this.emit("abort"));
    this.destroy();
  }

  flushHeaders() {
    this._connect();
    this._sendHead();
  }

  // Unlike a plain Writable, a request does not close when its body is finished: 'close' comes
  // when the exchange is over (the socket closes) or the request is destroyed.
  _emitCloseW() {
    if (!this.destroyed && !this._exchangeDone) return;
    if (this._closeEmitted) return;
    this._closeEmitted = true;
    this._writableState.closed = true;
    this.emit("close");
  }
}

function abortError() {
  return Object.assign(new Error("The operation was aborted"), { name: "AbortError", code: "ABORT_ERR" });
}

/** HTTP/1.1 client request over a TCP socket. */
class NetClientRequest extends ClientRequest {
  constructor(opts, callback) {
    super(opts, callback);
    if (!this.hasHeader("connection")) this.setHeader("Connection", "close");
    this._pending = [];
    process.nextTick(() => this._connect());
  }

  _connect() {
    if (this.socket || this.destroyed) return;
    let agent = this.agent;
    if (agent === undefined) agent = this.protocol === "https:" ? https.globalAgent : globalAgent;
    const create = this._opts.createConnection ?? (agent ? agent.createConnection?.bind(agent) : undefined);
    // `path` is the request path, not an IPC path; `socketPath` selects IPC.
    const connectOpts = { ...this._opts, host: this.host, port: this.port, path: this._opts.socketPath };
    if (this.protocol === "https:" && connectOpts.servername === undefined && !net.isIP(this.host)) {
      connectOpts.servername = this._headers.get("host")?.[1]?.replace(/:\d+$/, "") ?? this.host;
    }
    const socket = this._createSocket(create, connectOpts);
    internal.dcPublish?.("http.client.request.start", { request: this });
    this._useSocket(socket);
  }

  _createSocket(create, connectOpts) {
    return create
      ? create(connectOpts)
      : this.protocol === "https:"
        ? httpsConnect(connectOpts)
        : net.connect({ host: this.host, port: this.port });
  }

  _useSocket(socket) {
    this.socket = socket;
    this.connection = socket;
    if (this._timeoutMs) socket.setTimeout(this._timeoutMs, () => this.emit("timeout"));
    let res = null;
    const parser = new HttpParser("response", {
      onHead: (head) => {
        if (head.statusCode >= 100 && head.statusCode < 200 && head.statusCode !== 101) {
          this.emit("information", { statusCode: head.statusCode, statusMessage: head.statusMessage, headers: head.headers });
          return {};
        }
        res = new IncomingMessage(socket);
        res._applyHead(head);
        res.req = this;
        if (head.statusCode === 101 || (this.method === "CONNECT" && head.statusCode >= 200 && head.statusCode < 300)) {
          const rest = parser.takeRest();
          socket.removeListener("data", onData);
          this.emit(head.statusCode === 101 ? "upgrade" : "connect", res, socket, rest);
          return { stop: true };
        }
        this.res = res;
        internal.dcPublish?.("http.client.response.finish", { request: this, response: res });
        this.emit("response", res);
        return { skipBody: this.method === "HEAD" || head.statusCode === 204 || head.statusCode === 304 };
      },
      onBody: (chunk) => res?.push(chunk),
      onEnd: () => {
        if (!res) return;
        res.complete = true;
        res.push(null);
        socket.end();
      },
      onError: (err) => this.destroy(err),
    });
    const onData = (chunk) => parser.execute(chunk);
    socket.on("data", onData);
    socket.on("end", () => parser.finish());
    socket.on("error", (err) => {
      if (res && !res.complete) res.destroy(err);
      if (!this.destroyed) this.destroy(err);
    });
    socket.on("close", () => {
      if (res && !res.complete) {
        res.aborted = true;
        res.destroy(Object.assign(new Error("aborted"), { code: "ECONNRESET" }));
      }
      this._exchangeDone = true;
      // A destroyed request emits 'error' and then 'close' itself (on the next tick).
      if (!this.destroyed) this._emitCloseW();
    });
    socket.on("connect", () => this.emit("socket", socket));
    for (const buf of this._pending.splice(0)) socket.write(buf);
  }

  _send(buf) {
    if (this.socket) this.socket.write(buf);
    else this._pending.push(buf);
  }

  _sendHead(contentLength) {
    if (this.headersSent) return;
    if (contentLength !== undefined && !this.hasHeader("content-length") && !this.hasHeader("transfer-encoding")) {
      if (contentLength > 0 || !["GET", "HEAD", "DELETE", "OPTIONS", "CONNECT"].includes(this.method)) {
        this.setHeader("Content-Length", contentLength);
      }
    } else if (contentLength === undefined && !this.hasHeader("content-length") && !this.hasHeader("transfer-encoding")) {
      this.setHeader("Transfer-Encoding", "chunked");
    }
    this.chunkedEncoding = /chunked/i.test(String(this.getHeader("transfer-encoding") ?? ""));
    this.headersSent = true;
    this._send(Buffer.from(`${this.method} ${this.path} HTTP/1.1${CRLF}${this._headerLines()}${CRLF}`, "latin1"));
  }

  _write(chunk, encoding, callback) {
    if (!this.headersSent) this._sendHead();
    this._writeBody(chunk, encoding);
    callback();
  }

  end(chunk, encoding, callback) {
    if (typeof chunk === "function") {
      callback = chunk;
      chunk = undefined;
    } else if (typeof encoding === "function") {
      callback = encoding;
      encoding = undefined;
    }
    if (!this.headersSent) {
      const length = chunk === undefined || chunk === null ? 0 : Buffer.byteLength(chunk, typeof encoding === "string" ? encoding : "utf8");
      this._sendHead(length);
    }
    if (chunk !== undefined && chunk !== null) this.write(chunk, encoding);
    return super.end(callback);
  }

  _final(callback) {
    if (this.chunkedEncoding) this._send(Buffer.from(`0${CRLF}${CRLF}`, "latin1"));
    this.finished = true;
    callback();
  }

  _destroy(err, callback) {
    this.socket?.destroy();
    callback(err);
  }
}

function request(input, options, callback) {
  const args = requestArgs(input, options, callback, "http:");
  return new NetClientRequest(args.opts, args.callback);
}

function get(input, options, callback) {
  const req = request(input, options, callback);
  req.end();
  return req;
}

function validateHeaderName(name) {
  if (typeof name !== "string" || !/^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(name)) {
    throw Object.assign(new TypeError(`Header name must be a valid HTTP token ["${name}"]`), { code: "ERR_INVALID_HTTP_TOKEN" });
  }
}

function validateHeaderValue(name, value) {
  if (value === undefined) {
    throw Object.assign(new TypeError(`Invalid value "${value}" for header "${name}"`), { code: "ERR_HTTP_INVALID_HEADER_VALUE" });
  }
  if (/[^\t\x20-\x7e\x80-\xff]/.test(String(value))) {
    throw Object.assign(new TypeError(`Invalid character in header content ["${name}"]`), { code: "ERR_INVALID_CHAR" });
  }
}

const http = {
  METHODS,
  STATUS_CODES,
  Agent,
  globalAgent,
  ClientRequest,
  IncomingMessage,
  OutgoingMessage,
  Server,
  ServerResponse,
  createServer,
  request,
  get,
  validateHeaderName,
  validateHeaderValue,
  maxHeaderSize: 16384,
  setMaxIdleHTTPParsers() {},
};

/** TLS connection for https requests (agent and request TLS options merged, like Node). */
function httpsConnect(options) {
  const tls = internal.requireBuiltin("tls");
  const o = { ...options };
  if (!o.ALPNProtocols) o.ALPNProtocols = ["http/1.1"];
  return tls.connect(o);
}

class HttpsAgent extends Agent {
  constructor(options) {
    super(options);
    this.defaultPort = 443;
    this.protocol = "https:";
  }
  createConnection(options, callback) {
    const socket = httpsConnect({ ...this.options, ...options });
    if (typeof callback === "function") socket.once("secureConnect", () => callback(null, socket));
    return socket;
  }
  getName(options = {}) {
    let name = super.getName(options);
    for (const key of ["ca", "cert", "key", "servername", "rejectUnauthorized"]) name += `:${options[key] ?? ""}`;
    return name;
  }
}

/** https.Server: a tls.Server whose secure connections are served as HTTP/1.1. */
function makeHttpsServer() {
  const tls = internal.requireBuiltin("tls");
  class HttpsServer extends tls.Server {
    constructor(options, requestListener) {
      if (typeof options === "function") {
        requestListener = options;
        options = {};
      }
      super({ ALPNProtocols: ["http/1.1"], ...(options ?? {}) });
      this.timeout = 0;
      this.keepAliveTimeout = 5000;
      this.headersTimeout = 60000;
      this.requestTimeout = 300000;
      this.maxHeadersCount = null;
      if (requestListener) this.on("request", requestListener);
      this.on("secureConnection", (socket) => this._handleConnection(socket));
    }
  }
  for (const name of ["_handleConnection", "setTimeout", "closeIdleConnections"]) HttpsServer.prototype[name] = Server.prototype[name];
  return HttpsServer;
}
let HttpsServerClass;

const https = {
  Agent: HttpsAgent,
  globalAgent: new HttpsAgent(),
  get Server() {
    HttpsServerClass ??= makeHttpsServer();
    return HttpsServerClass;
  },
  createServer(options, requestListener) {
    HttpsServerClass ??= makeHttpsServer();
    return new HttpsServerClass(options, requestListener);
  },
  request(input, options, callback) {
    const args = requestArgs(input, options, callback, "https:");
    return new NetClientRequest(args.opts, args.callback);
  },
  get(input, options, callback) {
    const req = https.request(input, options, callback);
    req.end();
    return req;
  },
};

internal.defineBuiltin("http", () => http);
internal.defineBuiltin("https", () => https);
