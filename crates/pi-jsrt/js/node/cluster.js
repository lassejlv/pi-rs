// node:cluster — Node's primary/worker protocol (lib/internal/cluster/*.js) over
// child_process.fork() and the IPC channel with descriptor passing. Workers are pi-jsrt runtimes
// in Node mode. The primary listens and passes accepted connections to workers round-robin
// (SCHED_RR, the default) or passes the listening socket itself (SCHED_NONE).

const EventEmitter = internal.EventEmitter;
const path = internal.requireBuiltin("path");

const SCHED_NONE = 1;
const SCHED_RR = 2;

// ---------------------------------------------------------------------------
// utils.js
// ---------------------------------------------------------------------------

const callbacks = new Map();
let seq = 0;

function sendHelper(proc, message, handle, cb) {
  if (!proc.connected) return false;
  message = { cmd: "NODE_CLUSTER", ...message, seq };
  if (typeof cb === "function") callbacks.set(seq, cb);
  seq += 1;
  return proc.send(message, handle);
}

function internalListener(worker, cb) {
  return function onInternalMessage(message, handle) {
    if (message.cmd !== "NODE_CLUSTER") return;
    let fn = cb;
    if (message.ack !== undefined) {
      const callback = callbacks.get(message.ack);
      if (callback !== undefined) {
        fn = callback;
        callbacks.delete(message.ack);
      }
    }
    fn.apply(worker, arguments);
  };
}

// ---------------------------------------------------------------------------
// worker.js
// ---------------------------------------------------------------------------

class Worker extends EventEmitter {
  constructor(options) {
    super();
    if (options === null || typeof options !== "object") options = {};
    this.exitedAfterDisconnect = undefined;
    this.state = options.state || "none";
    this.id = options.id | 0;
    if (options.process) {
      this.process = options.process;
      this.process.on("error", (code, signal) => this.emit("error", code, signal));
      this.process.on("message", (message, handle) => this.emit("message", message, handle));
    }
  }
  kill() {
    this.destroy.apply(this, arguments);
  }
  send() {
    return this.process.send.apply(this.process, arguments);
  }
  isDead() {
    return this.process.exitCode != null || this.process.signalCode != null;
  }
  isConnected() {
    return this.process.connected;
  }
}

const cluster = new EventEmitter();
cluster.Worker = Worker;
cluster.SCHED_NONE = SCHED_NONE;
cluster.SCHED_RR = SCHED_RR;

// ---------------------------------------------------------------------------
// primary.js
// ---------------------------------------------------------------------------

function setupPrimary() {
  const handles = new Map();
  let ids = 0;
  let initialized = false;
  const intercom = new EventEmitter();
  const policyEnv = process.env.NODE_CLUSTER_SCHED_POLICY;
  let schedulingPolicy = policyEnv === "rr" ? SCHED_RR : policyEnv === "none" ? SCHED_NONE : process.platform === "win32" ? SCHED_NONE : SCHED_RR;

  cluster.isWorker = false;
  cluster.isMaster = true;
  cluster.isPrimary = true;
  cluster.workers = {};
  cluster.settings = {};
  cluster.schedulingPolicy = schedulingPolicy;

  cluster.setupPrimary = function setupPrimary(options) {
    const settings = {
      args: process.argv.slice(2),
      exec: process.argv[1],
      execArgv: process.execArgv,
      silent: false,
      ...cluster.settings,
      ...options,
    };
    cluster.settings = settings;
    if (initialized === true) return process.nextTick(() => cluster.emit("setup", settings));
    initialized = true;
    schedulingPolicy = cluster.schedulingPolicy;
    if (schedulingPolicy !== SCHED_NONE && schedulingPolicy !== SCHED_RR) {
      const err = new Error(`Bad cluster.schedulingPolicy: ${schedulingPolicy}`);
      err.code = "ERR_ASSERTION";
      throw err;
    }
    process.nextTick(() => cluster.emit("setup", settings));
  };
  cluster.setupMaster = cluster.setupPrimary;

  function createWorkerProcess(id, env) {
    const workerEnv = { ...process.env, ...env, NODE_UNIQUE_ID: `${id}` };
    return internal.requireBuiltin("child_process").fork(cluster.settings.exec, cluster.settings.args, {
      cwd: cluster.settings.cwd,
      env: workerEnv,
      serialization: cluster.settings.serialization,
      silent: cluster.settings.silent,
      windowsHide: cluster.settings.windowsHide,
      execArgv: [...cluster.settings.execArgv],
      stdio: cluster.settings.stdio,
      gid: cluster.settings.gid,
      uid: cluster.settings.uid,
    });
  }

  function removeWorker(worker) {
    delete cluster.workers[worker.id];
    if (Object.keys(cluster.workers).length === 0) intercom.emit("disconnect");
  }

  function removeHandlesForWorker(worker) {
    handles.forEach((handle, key) => {
      if (handle.remove(worker)) handles.delete(key);
    });
  }

  cluster.fork = function fork(env) {
    cluster.setupPrimary();
    const id = ++ids;
    const workerProcess = createWorkerProcess(id, env);
    const worker = new Worker({ id, process: workerProcess });
    worker.on("message", function (message, handle) {
      cluster.emit("message", this, message, handle);
    });
    worker.process.once("exit", (exitCode, signalCode) => {
      if (!worker.isConnected()) {
        removeHandlesForWorker(worker);
        removeWorker(worker);
      }
      worker.exitedAfterDisconnect = !!worker.exitedAfterDisconnect;
      worker.state = "dead";
      worker.emit("exit", exitCode, signalCode);
      cluster.emit("exit", worker, exitCode, signalCode);
    });
    worker.process.once("disconnect", () => {
      removeHandlesForWorker(worker);
      if (worker.isDead()) removeWorker(worker);
      worker.exitedAfterDisconnect = !!worker.exitedAfterDisconnect;
      worker.state = "disconnected";
      worker.emit("disconnect");
      cluster.emit("disconnect", worker);
    });
    worker.process.on("internalMessage", internalListener(worker, onmessage));
    process.nextTick(() => cluster.emit("fork", worker));
    cluster.workers[worker.id] = worker;
    return worker;
  };

  cluster.disconnect = function disconnect(cb) {
    const workers = Object.keys(cluster.workers);
    if (workers.length === 0) {
      process.nextTick(() => intercom.emit("disconnect"));
    } else {
      for (const worker of Object.values(cluster.workers)) {
        if (worker.isConnected()) worker.disconnect();
      }
    }
    if (typeof cb === "function") intercom.once("disconnect", cb);
  };

  const methodMessageMapping = { close, exitedAfterDisconnect, listening, online, queryServer };

  function onmessage(message, handle) {
    const fn = methodMessageMapping[message.act];
    if (typeof fn === "function") fn(this, message);
  }

  function online(worker) {
    worker.state = "online";
    worker.emit("online");
    cluster.emit("online", worker);
  }

  function exitedAfterDisconnect(worker, message) {
    worker.exitedAfterDisconnect = true;
    send(worker, { ack: message.seq });
  }

  function queryServer(worker, message) {
    if (worker.exitedAfterDisconnect) return;
    const key = `${message.address}:${message.port}:${message.addressType}:${message.fd}:${message.index}`;
    let handle = handles.get(key);
    if (handle === undefined) {
      const address = message.address;
      if (schedulingPolicy !== SCHED_RR || message.addressType === "udp4" || message.addressType === "udp6") {
        handle = new SharedHandle(key, address, message);
      } else {
        handle = new RoundRobinHandle(key, address, message);
      }
      handles.set(key, handle);
    }
    if (!handle.data) handle.data = message.data;
    handle.add(worker, (errno, reply, h) => {
      const { data } = handles.get(key) ?? {};
      if (errno) handles.delete(key);
      send(worker, { errno, key, ack: message.seq, data, ...reply }, h);
    });
  }

  function listening(worker, message) {
    const info = { addressType: message.addressType, address: message.address, port: message.port, fd: message.fd };
    worker.state = "listening";
    worker.emit("listening", info);
    cluster.emit("listening", worker, info);
  }

  function close(worker, message) {
    const key = message.key;
    const handle = handles.get(key);
    if (handle && handle.remove(worker)) handles.delete(key);
  }

  function send(worker, message, handle, cb) {
    return sendHelper(worker.process, message, handle, cb);
  }

  Worker.prototype.disconnect = function disconnect() {
    this.exitedAfterDisconnect = true;
    send(this, { act: "disconnect" });
    removeHandlesForWorker(this);
    removeWorker(this);
    return this;
  };

  Worker.prototype.destroy = function destroy(signo) {
    const proc = this.process;
    const signal = signo || "SIGTERM";
    if (this.isConnected()) {
      this.once("disconnect", () => proc.kill(signal));
      this.disconnect();
      return;
    }
    proc.kill(signal);
  };
}

// round_robin_handle.js: the primary accepts and hands connections to free workers.
class RoundRobinHandle {
  constructor(key, address, { port, fd }) {
    this.key = key;
    this.all = new Map();
    this.free = new Map();
    this.handles = [];
    this.listened = false;
    this.server = internal.requireBuiltin("net").createServer(() => {});
    this.server._rawMode = true;
    this.server.on("rawconnection", (fd) => this.distribute(0, { fd, _fdHandle: true, close: () => ops.closeFd(fd) }));
    if (fd >= 0) this.server.listen({ fd });
    else this.server.listen({ port, host: address ?? undefined });
    this.server.once("listening", () => {
      this.listened = true;
    });
  }
  add(worker, send) {
    this.all.set(worker.id, worker);
    const done = () => {
      const a = this.server.address();
      send(null, { sockname: a ? { address: a.address, family: a.family, port: a.port } : undefined }, null);
      this.handoff(worker);
    };
    if (this.listened) return done();
    this.server.once("listening", done);
    this.server.once("error", (err) => send(err.errno, null));
  }
  remove(worker) {
    const existed = this.all.delete(worker.id);
    if (!existed) return false;
    this.free.delete(worker.id);
    if (this.all.size !== 0) return false;
    for (const h of this.handles.splice(0)) h.close();
    this.server.close();
    return true;
  }
  distribute(err, handle) {
    if (err) return;
    this.handles.push(handle);
    const [workerEntry] = this.free;
    if (Array.isArray(workerEntry)) {
      const [workerId, worker] = workerEntry;
      this.free.delete(workerId);
      this.handoff(worker);
    }
  }
  handoff(worker) {
    if (!this.all.has(worker.id)) return;
    const handle = this.handles.shift();
    if (handle === undefined) {
      this.free.set(worker.id, worker);
      return;
    }
    const message = { act: "newconn", key: this.key };
    sendHelper(worker.process, message, handle, (reply) => {
      if (reply.accepted) handle.close();
      else this.distribute(0, handle);
      this.handoff(worker);
    });
  }
}

// shared_handle.js: workers listen on the primary's listening socket themselves.
class SharedHandle {
  constructor(key, address, { port, addressType, fd }) {
    this.key = key;
    this.workers = new Map();
    this.handle = null;
    this.errno = 0;
    if (addressType === "udp4" || addressType === "udp6") {
      this.errno = -97; // EAFNOSUPPORT: UDP sockets are bound by each worker.
      return;
    }
    const r = ops.netBindFd(address ?? "", port);
    if (r.error) this.errno = r.error.errno;
    else this.handle = { fd: r.fd, _fdHandle: true, close: () => ops.closeFd(r.fd) };
  }
  add(worker, send) {
    this.workers.set(worker.id, worker);
    send(this.errno, null, this.handle);
  }
  remove(worker) {
    if (!this.workers.has(worker.id)) return false;
    this.workers.delete(worker.id);
    if (this.workers.size !== 0) return false;
    this.handle?.close();
    this.handle = null;
    return true;
  }
}

// ---------------------------------------------------------------------------
// child.js
// ---------------------------------------------------------------------------

function setupWorker() {
  const handles = new Map();
  const indexes = new Map();

  cluster.isWorker = true;
  cluster.isMaster = false;
  cluster.isPrimary = false;
  cluster.worker = null;

  function send(message, cb) {
    return sendHelper(process, message, null, cb);
  }

  cluster._setupWorker = function _setupWorker() {
    const worker = new Worker({ id: +internal.clusterUniqueId | 0, process, state: "online" });
    cluster.worker = worker;
    process.once("disconnect", () => {
      worker.emit("disconnect");
      if (!worker.exitedAfterDisconnect) process.exit(0);
    });
    process.on("internalMessage", internalListener(worker, onmessage));
    send({ act: "online" });
    function onmessage(message, handle) {
      if (message.act === "newconn") onconnection(message, handle);
      else if (message.act === "disconnect") _disconnect.call(worker, true);
    }
  };

  cluster._getServer = function _getServer(obj, options, cb) {
    const address = options.address;
    const indexesKey = [address, options.port, options.addressType, options.fd].join(":");
    let indexSet = indexes.get(indexesKey);
    if (indexSet === undefined) {
      indexSet = { nextIndex: 0, set: new Set() };
      indexes.set(indexesKey, indexSet);
    }
    const index = indexSet.nextIndex++;
    indexSet.set.add(index);
    const message = { act: "queryServer", index, data: null, ...options };
    message.address = address;
    if (obj._getServerData) message.data = obj._getServerData();
    send(message, (reply, handle) => {
      if (typeof obj._setServerData === "function") obj._setServerData(reply.data);
      if (handle) shared(reply, { handle, indexesKey, index }, cb);
      else rr(reply, { indexesKey, index }, cb);
    });
    obj.once("listening", () => {
      if (!indexes.has(indexesKey)) return;
      cluster.worker.state = "listening";
      const a = obj.address();
      message.act = "listening";
      message.port = (a && a.port) || options.port;
      send(message);
    });
  };

  function removeIndexesKey(indexesKey, index) {
    const indexSet = indexes.get(indexesKey);
    if (!indexSet) return;
    indexSet.set.delete(index);
    if (indexSet.set.size === 0) indexes.delete(indexesKey);
  }

  function shared(message, { handle, indexesKey, index }, cb) {
    const key = message.key;
    // The listener adopts the passed descriptor, so closing the handle only notifies the primary.
    const close = handle._fdHandle ? () => {} : handle.close;
    handle.close = function () {
      send({ act: "close", key });
      handles.delete(key);
      removeIndexesKey(indexesKey, index);
      return close.apply(handle, arguments);
    };
    handles.set(key, handle);
    cb(message.errno, handle);
  }

  function rr(message, { indexesKey, index }, cb) {
    if (message.errno) return cb(message.errno, null);
    let key = message.key;
    let fakeHandle = null;
    function ref() {
      if (!fakeHandle) fakeHandle = setInterval(() => {}, 2 ** 31 - 1);
    }
    function unref() {
      if (fakeHandle) {
        clearInterval(fakeHandle);
        fakeHandle = null;
      }
    }
    function close() {
      if (key === undefined) return;
      unref();
      send({ act: "close", key });
      handles.delete(key);
      removeIndexesKey(indexesKey, index);
      key = undefined;
    }
    const handle = { close, ref, unref, rr: true, sockname: message.sockname, onconnection: null };
    handles.set(key, handle);
    ref();
    cb(0, handle);
  }

  function onconnection(message, handle) {
    const key = message.key;
    const server = handles.get(key);
    let accepted = server !== undefined;
    if (accepted && server.owner && server.owner.maxConnections != null && server.owner._connections.size >= server.owner.maxConnections) accepted = false;
    send({ ack: message.seq, accepted });
    if (accepted && server.onconnection) server.onconnection(handle);
    else handle?.close();
  }

  function _disconnect(primaryInitiated) {
    this.exitedAfterDisconnect = true;
    let waitingCount = 1;
    const checkWaitingCount = () => {
      waitingCount--;
      if (waitingCount === 0) {
        if (primaryInitiated) process.disconnect();
        else send({ act: "exitedAfterDisconnect" }, () => process.disconnect());
      }
    };
    handles.forEach((handle) => {
      if (handle.owner) {
        waitingCount++;
        handle.owner.close(checkWaitingCount);
      } else {
        handle.close();
      }
    });
    handles.clear();
    checkWaitingCount();
  }

  Worker.prototype.disconnect = function disconnect() {
    if (this.state !== "disconnecting" && this.state !== "destroying") {
      this.state = "disconnecting";
      _disconnect.call(this);
    }
    return this;
  };

  Worker.prototype.destroy = function destroy() {
    if (this.state === "destroying") return;
    this.exitedAfterDisconnect = true;
    if (!this.isConnected()) {
      process.exit(0);
    } else {
      this.state = "destroying";
      send({ act: "exitedAfterDisconnect" }, () => process.disconnect());
      process.once("disconnect", () => process.exit(0));
    }
  };

  // net.Server.listen() in a worker (see js/node/net.js): ask the primary for the server.
  internal.clusterListen = (server, options) => {
    cluster._getServer(server, options, (errno, handle) => {
      if (errno) {
        const err = new Error(`listen ${errno === -98 ? "EADDRINUSE" : "EADDRNOTAVAIL"}: address already in use ${options.address ?? "::"}:${options.port}`);
        err.code = errno === -98 ? "EADDRINUSE" : "EADDRNOTAVAIL";
        err.errno = errno;
        err.syscall = "listen";
        server.emit("error", err);
        return;
      }
      if (handle.rr) {
        handle.owner = server;
        handle.onconnection = (raw) => server._acceptFd(raw.fd);
        server._clusterHandle = handle;
        server._address = handle.sockname ? { ...handle.sockname } : null;
        server.listening = true;
        process.nextTick(() => server.emit("listening"));
      } else {
        handle.owner = server;
        server._clusterHandle = handle;
        server.listen({ fd: handle.fd });
      }
    });
    return true;
  };
}

if (internal.clusterUniqueId !== undefined) {
  setupWorker();
  cluster._setupWorker();
} else {
  setupPrimary();
}

internal.defineBuiltin("cluster", () => cluster);
