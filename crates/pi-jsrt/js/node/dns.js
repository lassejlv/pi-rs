// node:dns — lookup()/promises.lookup() through getaddrinfo (tokio); resolve*() and reverse() with
// a DNS stub resolver speaking the wire protocol (src/ops/dns.rs), like Node's c-ares;
// lookupService through getnameinfo.

const net = internal.requireBuiltin("net");

const ADDRCONFIG = 32;
const V4MAPPED = 8;
const ALL = 16;
const errorCodes = {
  NODATA: "ENODATA", FORMERR: "EFORMERR", SERVFAIL: "ESERVFAIL", NOTFOUND: "ENOTFOUND", NOTIMP: "ENOTIMP",
  REFUSED: "EREFUSED", BADQUERY: "EBADQUERY", BADNAME: "EBADNAME", BADFAMILY: "EBADFAMILY", BADRESP: "EBADRESP",
  CONNREFUSED: "ECONNREFUSED", TIMEOUT: "ETIMEOUT", EOF: "EOF", FILE: "EFILE", NOMEM: "ENOMEM",
  DESTRUCTION: "EDESTRUCTION", BADSTR: "EBADSTR", BADFLAGS: "EBADFLAGS", NONAME: "ENONAME", BADHINTS: "EBADHINTS",
  NOTINITIALIZED: "ENOTINITIALIZED", LOADIPHLPAPI: "ELOADIPHLPAPI", ADDRGETNETWORKPARAMS: "EADDRGETNETWORKPARAMS",
  CANCELLED: "ECANCELLED",
};

let defaultOrder = "verbatim";

function lookupError(err, hostname, syscall = "getaddrinfo") {
  const code = err?.code ?? "ENOTFOUND";
  return Object.assign(new Error(`${syscall} ${code} ${hostname}`), { code, errno: code === "ENOTFOUND" ? -3008 : undefined, syscall, hostname });
}

async function lookupAll(hostname, options) {
  const family = options.family === "IPv4" ? 4 : options.family === "IPv6" ? 6 : Number(options.family ?? 0);
  const ipFamily = net.isIP(hostname);
  let list;
  if (ipFamily) list = [{ address: hostname, family: ipFamily }];
  else {
    try {
      list = await ops.dnsLookup(String(hostname));
    } catch (err) {
      throw lookupError(err, hostname);
    }
  }
  if (family === 4 || family === 6) list = list.filter((a) => a.family === family);
  const order = options.order ?? (options.verbatim === false ? "ipv4first" : options.verbatim === true ? "verbatim" : defaultOrder);
  if (order === "ipv4first") list = [...list.filter((a) => a.family === 4), ...list.filter((a) => a.family === 6)];
  else if (order === "ipv6first") list = [...list.filter((a) => a.family === 6), ...list.filter((a) => a.family === 4)];
  if (list.length === 0) throw lookupError({ code: "ENOTFOUND" }, hostname);
  return list;
}

function lookup(hostname, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = {};
  } else if (typeof options === "number") {
    options = { family: options };
  }
  options = options ?? {};
  if (typeof callback !== "function") {
    throw Object.assign(new TypeError(`The "callback" argument must be of type function. Received ${typeof callback}`), { code: "ERR_INVALID_ARG_TYPE" });
  }
  if (!hostname) {
    process.nextTick(callback, null, options.all ? [] : null, options.all ? undefined : options.family === 6 ? 6 : 4);
    return {};
  }
  lookupAll(hostname, options).then(
    (list) => (options.all ? callback(null, list) : callback(null, list[0].address, list[0].family)),
    (err) => callback(err),
  );
  return {};
}

// ---------------------------------------------------------------------------
// Resolver: DNS queries through the stub resolver in src/ops/dns.rs (like c-ares)
// ---------------------------------------------------------------------------

function systemConfig() {
  const cfg = { servers: [], timeout: 5000, tries: 4 };
  try {
    const text = internal.requireBuiltin("fs").readFileSync("/etc/resolv.conf", "utf8");
    for (const line of text.split("\n")) {
      const ns = /^\s*nameserver\s+(\S+)/.exec(line)?.[1];
      if (ns) cfg.servers.push(ns);
      const opts = /^\s*options\s+(.*)$/.exec(line)?.[1];
      if (opts) {
        for (const o of opts.split(/\s+/)) {
          const [k, v] = o.split(":");
          if (k === "timeout" && Number(v) > 0) cfg.timeout = Number(v) * 1000;
          if (k === "attempts" && Number(v) > 0) cfg.tries = Number(v);
        }
      }
    }
  } catch {}
  return cfg;
}
let systemDefaults;
const defaults = () => (systemDefaults ??= systemConfig());

function invalidArg(name, value) {
  const err = new TypeError(`The argument '${name}' is invalid. Received ${internal.inspect(value)}`);
  err.code = "ERR_INVALID_ARG_VALUE";
  return err;
}

function invalidArgType(name, expected, value) {
  const err = new TypeError(`The "${name}" argument must be ${expected}. Received ${internal.describeReceived(value)}`);
  err.code = "ERR_INVALID_ARG_TYPE";
  return err;
}

// "ip", "ip:port" or "[ipv6]:port" -> [ip, port]
function parseServer(server) {
  if (typeof server !== "string") throw invalidArgType("servers[0]", "of type string", server);
  let ip = server;
  let port = 53;
  const v6 = /^\[([^\]]+)\](?::(\d+))?$/.exec(server);
  if (v6) {
    ip = v6[1];
    if (v6[2]) port = Number(v6[2]);
  } else if (net.isIPv4(server.split(":")[0]) && server.includes(":")) {
    const i = server.lastIndexOf(":");
    ip = server.slice(0, i);
    port = Number(server.slice(i + 1));
  }
  if (!net.isIP(ip)) {
    const err = new TypeError(`Invalid IP address: ${server}`);
    err.code = "ERR_INVALID_IP_ADDRESS";
    throw err;
  }
  return [ip, port];
}

function formatServer([ip, port]) {
  if (port === 53) return ip;
  return net.isIPv6(ip) ? `[${ip}]:${port}` : `${ip}:${port}`;
}

const syscallNames = {
  A: "queryA", AAAA: "queryAaaa", ANY: "queryAny", CAA: "queryCaa", CNAME: "queryCname", MX: "queryMx", NAPTR: "queryNaptr",
  NS: "queryNs", PTR: "queryPtr", SOA: "querySoa", SRV: "querySrv", TLSA: "queryTlsa", TXT: "queryTxt",
};

function dnsError(err) {
  const e = new Error(err?.message ?? String(err));
  if (err?.code) e.code = err.code;
  const m = /^(\S+) (\S+) (.*)$/.exec(e.message);
  if (m) {
    e.syscall = m[1];
    e.hostname = m[3];
  }
  return e;
}

function shape(rrtype, answers, options) {
  const of = (t) => answers.filter((a) => a.type === t);
  switch (rrtype) {
    case "A":
    case "AAAA":
      return of(rrtype).map((a) => (options?.ttl ? { address: a.address, ttl: a.ttl } : a.address));
    case "CNAME":
    case "NS":
    case "PTR":
      return of(rrtype).map((a) => a.value);
    case "MX":
      return of("MX").map((a) => ({ exchange: a.exchange, priority: a.priority }));
    case "TXT":
      return of("TXT").map((a) => a.entries);
    case "SRV":
      return of("SRV").map((a) => ({ name: a.name, port: a.port, priority: a.priority, weight: a.weight }));
    case "SOA": {
      const a = of("SOA")[0];
      return a && { nsname: a.nsname, hostmaster: a.hostmaster, serial: a.serial, refresh: a.refresh, retry: a.retry, expire: a.expire, minttl: a.minttl };
    }
    case "CAA":
      return of("CAA").map((a) => ({ critical: a.critical, [a.tag]: a.value }));
    case "NAPTR":
      return of("NAPTR").map((a) => ({ flags: a.flags, service: a.service, regexp: a.regexp, replacement: a.replacement, order: a.order, preference: a.preference }));
    case "TLSA":
      return of("TLSA").map((a) => ({ certUsage: a.certUsage, selector: a.selector, match: a.match, data: new Uint8Array(a.data).buffer }));
    case "ANY":
      return answers.map((a) => {
        const { type, ttl, ...rest } = a;
        switch (type) {
          case "A":
          case "AAAA":
            return { address: a.address, ttl, type };
          case "CNAME":
          case "NS":
          case "PTR":
            return { value: a.value, type };
          case "TXT":
            return { entries: a.entries, type };
          case "CAA":
            return { critical: a.critical, [a.tag]: a.value, type };
          default:
            return { ...rest, type };
        }
      });
  }
  return [];
}

class ResolverBase {
  constructor(options = {}) {
    if (options === null || typeof options !== "object") throw invalidArgType("options", "of type object", options);
    const { timeout = -1, tries = 4 } = options;
    if (typeof timeout !== "number" || !Number.isInteger(timeout) || timeout < -1) {
      const err = new RangeError(`The value of "options.timeout" is out of range. It must be >= -1 && <= 2147483647. Received ${timeout}`);
      err.code = "ERR_OUT_OF_RANGE";
      throw err;
    }
    this._timeout = timeout;
    this._tries = tries;
    this._servers = undefined;
    Object.defineProperty(this, "_timeout", { enumerable: false });
    Object.defineProperty(this, "_tries", { enumerable: false });
    Object.defineProperty(this, "_servers", { enumerable: false, writable: true });
  }
  getServers() {
    return (this._servers ?? defaults().servers.map((s) => [s, 53])).map(formatServer);
  }
  setServers(servers) {
    if (!Array.isArray(servers)) throw invalidArgType("servers", "an instance of Array", servers);
    this._servers = servers.map(parseServer);
  }
  setLocalAddress() {}
  cancel() {}
  _query(hostname, rrtype) {
    if (typeof hostname !== "string") return Promise.reject(invalidArgType("name", "of type string", hostname));
    const servers = (this._servers ?? defaults().servers.map((s) => [s, 53])).map(([ip, port]) => (net.isIPv6(ip) ? `[${ip}]:${port}` : `${ip}:${port}`));
    const timeout = this._timeout > 0 ? this._timeout : defaults().timeout;
    const tries = this._timeout > 0 || this._tries !== 4 ? this._tries : defaults().tries;
    return ops.dnsQuery(hostname, rrtype, servers, timeout, tries, syscallNames[rrtype]).catch((err) => {
      throw dnsError(err);
    });
  }
  _resolve(hostname, rrtype, options) {
    return this._query(hostname, rrtype).then((answers) => shape(rrtype, answers, options));
  }
}

const resolverMethods = {
  resolve4(hostname, options) {
    return this._resolve(hostname, "A", options);
  },
  resolve6(hostname, options) {
    return this._resolve(hostname, "AAAA", options);
  },
  resolveAny(hostname) {
    return this._resolve(hostname, "ANY");
  },
  resolveCaa(hostname) {
    return this._resolve(hostname, "CAA");
  },
  resolveCname(hostname) {
    return this._resolve(hostname, "CNAME");
  },
  resolveMx(hostname) {
    return this._resolve(hostname, "MX");
  },
  resolveNaptr(hostname) {
    return this._resolve(hostname, "NAPTR");
  },
  resolveNs(hostname) {
    return this._resolve(hostname, "NS");
  },
  resolvePtr(hostname) {
    return this._resolve(hostname, "PTR");
  },
  resolveSoa(hostname) {
    return this._resolve(hostname, "SOA");
  },
  resolveSrv(hostname) {
    return this._resolve(hostname, "SRV");
  },
  resolveTlsa(hostname) {
    return this._resolve(hostname, "TLSA");
  },
  resolveTxt(hostname) {
    return this._resolve(hostname, "TXT");
  },
  resolve(hostname, rrtype = "A") {
    if (typeof rrtype !== "string") return Promise.reject(invalidArgType("rrtype", "of type string", rrtype));
    if (!(rrtype in syscallNames)) return Promise.reject(invalidArg("rrtype", rrtype));
    return this._resolve(hostname, rrtype);
  },
  reverse(ip) {
    let name;
    if (net.isIPv4(ip)) name = `${ip.split(".").reverse().join(".")}.in-addr.arpa`;
    else if (net.isIPv6(ip)) {
      const full = expandIPv6(ip);
      name = `${full.replace(/:/g, "").split("").reverse().join(".")}.ip6.arpa`;
    } else {
      return Promise.reject(Object.assign(new Error(`getHostByAddr EINVAL ${ip}`), { code: "EINVAL", syscall: "getHostByAddr", hostname: ip }));
    }
    return this._query(name, "PTR").then(
      (answers) => shape("PTR", answers),
      (err) => {
        err.message = err.message.replace(/^queryPtr (\S+) .*$/, `getHostByAddr $1 ${ip}`);
        err.syscall = "getHostByAddr";
        err.hostname = ip;
        throw err;
      },
    );
  },
};

function expandIPv6(ip) {
  let [head, tail] = ip.split("::");
  const h = head ? head.split(":") : [];
  const t = tail !== undefined ? (tail ? tail.split(":") : []) : [];
  const fill = tail !== undefined ? Array(8 - h.length - t.length).fill("0") : [];
  return [...h, ...fill, ...t].map((g) => g.padStart(4, "0")).join(":");
}

class PromisesResolver extends ResolverBase {}
Object.assign(PromisesResolver.prototype, resolverMethods);

const callbackify = (fn) =>
  function (...args) {
    const cb = args.pop();
    if (typeof cb !== "function") throw invalidArgType("callback", "of type function", cb);
    fn.apply(this, args).then(
      (v) => cb(null, v),
      (err) => cb(err),
    );
    return {};
  };

class Resolver extends ResolverBase {}
for (const [name, fn] of Object.entries(resolverMethods)) Resolver.prototype[name] = callbackify(fn);

const defaultResolver = new PromisesResolver();

function lookupServicePromise(address, port) {
  if (!net.isIP(address)) return Promise.reject(invalidArg("address", address));
  if (typeof port !== "number" && typeof port !== "string") return Promise.reject(invalidArgType("port", "of type number", port));
  port = Number(port);
  if (!(port >= 0 && port <= 65535 && Number.isInteger(port))) {
    const err = new RangeError(`Port should be >= 0 and < 65536. Received type number (${port}).`);
    err.code = "ERR_SOCKET_BAD_PORT";
    return Promise.reject(err);
  }
  return ops.dnsLookupService(address, port).then(
    ([hostname, service]) => ({ hostname, service }),
    (err) => {
      throw dnsError(err);
    },
  );
}

const promiseResolvers = {};
for (const name of Object.keys(resolverMethods)) promiseResolvers[name] = (...args) => defaultResolver[name](...args);
promiseResolvers.lookupService = lookupServicePromise;

function servers() {
  return defaultResolver.getServers();
}

const promises = {
  ...promiseResolvers,
  lookup: (hostname, options = {}) => {
    if (typeof options === "number") options = { family: options };
    return lookupAll(hostname, options).then((list) => (options.all ? list : list[0]));
  },
  Resolver: PromisesResolver,
  getServers: servers,
  setServers: (list) => defaultResolver.setServers(list),
  getDefaultResultOrder: () => defaultOrder,
  setDefaultResultOrder: (order) => {
    defaultOrder = order;
  },
  ...errorCodes,
};

const dns = {
  lookup,
  lookupService(address, port, callback) {
    if (typeof callback !== "function") throw invalidArgType("callback", "of type function", callback);
    lookupServicePromise(address, port).then(
      ({ hostname, service }) => callback(null, hostname, service),
      (err) => callback(err),
    );
    return {};
  },
  Resolver,
  getDefaultResultOrder: () => defaultOrder,
  setDefaultResultOrder: (order) => {
    defaultOrder = order;
  },
  setServers: (list) => defaultResolver.setServers(list),
  getServers: servers,
  ADDRCONFIG,
  ALL,
  V4MAPPED,
  ...errorCodes,
  promises,
};
for (const [name, fn] of Object.entries(promiseResolvers)) if (!(name in dns)) dns[name] = callbackify(fn);

internal.defineBuiltin("dns", () => dns);
internal.defineBuiltin("dns/promises", () => promises);
