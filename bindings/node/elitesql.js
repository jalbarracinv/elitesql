// EliteSQL Node client for the sidecar mode (`elitesql serve <db> <socket>`).
// Zero dependencies. Promise-based; requests are answered in order.
//
//   const { SidecarClient } = require('./elitesql');
//   const db = await SidecarClient.connect('/tmp/elitesql.sock');
//   await db.query("CREATE TABLE docs (title text NOT NULL)");
//   const { inserted } = await db.query("INSERT INTO docs (title) VALUES ('hola')");
//   const { columns, rows } = await db.query('SELECT * FROM docs');
//
// Values: scalars are native; date/timestamp arrive as Date objects (dates at
// UTC midnight; `timestampMicros(date)` recovers the exact microseconds a
// Date cannot hold), time as milliseconds since midnight, blobs as Buffer,
// vectors as number arrays. Int64 values outside Number's safe integer range
// arrive as BigInt (inside Number's range they are plain numbers: compare
// with `==` or normalize with BigInt() when a column may hold both). JSON
// column values keep large integers exact as BigInt.

'use strict';

const net = require('net');
const readline = require('readline');

const US_PER_DAY = 86_400_000_000n;

// Microseconds a Date cannot hold. `timestampMicros(date)` reads it back and
// `encodeParam` uses it, so a value read from the database round-trips
// exactly even though Date itself only resolves milliseconds.
const MICROS = Symbol.for('elitesql.micros');

/** Exact microseconds since the Unix epoch of a Date decoded by this client. */
function timestampMicros(date) {
  if (!(date instanceof Date)) throw new TypeError('timestampMicros expects a Date');
  return date[MICROS] !== undefined ? date[MICROS] : BigInt(date.getTime()) * 1000n;
}

function decodeFloatRepr(repr) {
  switch (repr) {
    case 'inf': return Infinity;
    case '-inf': return -Infinity;
    case 'NaN': return NaN;
    default: {
      const n = Number(repr);
      if (Number.isNaN(n)) throw new EliteSQLError(2, `unrecognized float64 representation ${repr}`);
      return n;
    }
  }
}

// JSON.parse rounds integers beyond 2^53 before a reviver can see them. The
// server sends JSON column values that contain such integers as exact text;
// this small parser turns those integers into BigInt and everything else into
// the ordinary JSON.parse result.
function parseJsonExact(text) {
  let i = 0;
  const fail = (what) => { throw new EliteSQLError(2, `invalid json text from sidecar: ${what} at ${i}`); };
  const ws = () => { while (i < text.length && ' \t\n\r'.includes(text[i])) i++; };
  const value = () => {
    ws();
    const c = text[i];
    if (c === '{') {
      i++; const out = {}; ws();
      if (text[i] === '}') { i++; return out; }
      for (;;) {
        ws(); if (text[i] !== '"') fail('expected key');
        const key = string(); ws();
        if (text[i] !== ':') fail('expected colon'); i++;
        out[key] = value(); ws();
        if (text[i] === ',') { i++; continue; }
        if (text[i] === '}') { i++; return out; }
        fail('expected , or }');
      }
    }
    if (c === '[') {
      i++; const out = []; ws();
      if (text[i] === ']') { i++; return out; }
      for (;;) {
        out.push(value()); ws();
        if (text[i] === ',') { i++; continue; }
        if (text[i] === ']') { i++; return out; }
        fail('expected , or ]');
      }
    }
    if (c === '"') return string();
    if (text.startsWith('true', i)) { i += 4; return true; }
    if (text.startsWith('false', i)) { i += 5; return false; }
    if (text.startsWith('null', i)) { i += 4; return null; }
    const match = /^-?\d+(\.\d+)?([eE][+-]?\d+)?/.exec(text.slice(i));
    if (!match) fail('unexpected token');
    i += match[0].length;
    if (match[1] === undefined && match[2] === undefined) {
      const big = BigInt(match[0]);
      return Number.isSafeInteger(Number(big)) ? Number(big) : big;
    }
    return Number(match[0]);
  };
  const string = () => {
    // Delegate escapes to JSON.parse on the exact string token.
    let j = i + 1;
    while (j < text.length && text[j] !== '"') { if (text[j] === '\\') j++; j++; }
    if (j >= text.length) fail('unterminated string');
    const out = JSON.parse(text.slice(i, j + 1));
    i = j + 1;
    return out;
  };
  const out = value();
  ws();
  if (i !== text.length) fail('trailing characters');
  return out;
}

function decodeValue(v) {
  if (v && typeof v === 'object' && !Array.isArray(v) && '$t' in v) {
    switch (v.$t) {
      case 'int64':
        return BigInt(v.v);
      case 'date':
        return new Date(v.days * 86_400_000);
      case 'time': // milliseconds since midnight as a number
        return v.us / 1000;
      case 'timestamp': {
        // Date resolves milliseconds; keep the exact microseconds alongside
        // so a read-modify-write does not truncate the stored instant.
        const us = BigInt(v.us);
        const date = new Date(Number(us / 1000n) - (us < 0n && us % 1000n !== 0n ? 1 : 0));
        if (us % 1000n !== 0n) Object.defineProperty(date, MICROS, { value: us, enumerable: false });
        return date;
      }
      case 'blob':
        return Buffer.from(v.hex, 'hex');
      case 'vector':
        return v.v;
      case 'json':
        return v.text !== undefined ? parseJsonExact(v.text) : v.v;
      case 'float64':
        return decodeFloatRepr(v.repr);
      default:
        return v;
    }
  }
  return v;
}

function decodeResult(result) {
  if (result && result.identity) {
    return {
      ...result,
      identity: { ...result.identity, values: result.identity.values.map(decodeValue) },
      lastrowid: decodeValue(result.lastrowid),
    };
  }
  if (result && Array.isArray(result.rows) && Array.isArray(result.columns)) {
    return {
      ...result,
      rows: result.rows.map((row) => row.map(decodeValue)),
    };
  }
  return result;
}

function decodeRecord(record) {
  const out = {};
  for (const [k, v] of Object.entries(record)) out[k] = decodeValue(v);
  return out;
}

function jsonNative(value) {
  if (value === null || ['boolean', 'string'].includes(typeof value)) return value;
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) throw new TypeError('JSON parameters require finite numbers');
    return value;
  }
  if (Array.isArray(value)) return value.map(jsonNative);
  if (value && typeof value === 'object' && Object.getPrototypeOf(value) === Object.prototype) {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, jsonNative(item)]));
  }
  throw new TypeError(`unsupported nested JSON parameter type: ${typeof value}`);
}

function encodeParam(value) {
  if (value === null || ['boolean', 'string'].includes(typeof value)) return value;
  if (typeof value === 'number') {
    if (Number.isInteger(value) && !Number.isSafeInteger(value)) {
      throw new RangeError('unsafe integer SQL parameter; pass a BigInt instead');
    }
    if (Number.isFinite(value)) return value;
    return { $t: 'float64', repr: Number.isNaN(value) ? 'NaN' : (value > 0 ? 'inf' : '-inf') };
  }
  if (typeof value === 'bigint') {
    if (value < -(2n ** 63n) || value >= 2n ** 63n) {
      throw new RangeError('EliteSQL int64 parameter is out of range');
    }
    return { $t: 'int64', v: value.toString() };
  }
  if (Buffer.isBuffer(value) || value instanceof Uint8Array) {
    return { $t: 'blob', hex: Buffer.from(value).toString('hex') };
  }
  if (value instanceof Date) {
    if (Number.isNaN(value.getTime())) throw new TypeError('invalid Date parameter');
    return { $t: 'timestamp', us: Number(timestampMicros(value)) };
  }
  if (Array.isArray(value) || (value && Object.getPrototypeOf(value) === Object.prototype)) {
    return { $t: 'json', v: jsonNative(value) };
  }
  throw new TypeError(`unsupported EliteSQL parameter type: ${typeof value}`);
}

function encodeParams(params) {
  if (Array.isArray(params)) return params.map(encodeParam);
  if (params && Object.getPrototypeOf(params) === Object.prototype) {
    return Object.fromEntries(Object.entries(params).map(([key, value]) => [key, encodeParam(value)]));
  }
  throw new TypeError('SQL params must be an array or object');
}

class EliteSQLError extends Error {
  constructor(code, message) {
    super(`[elitesql:${code}] ${message}`);
    this.code = code;
  }
}
// Stable status codes (see README "Error codes and retries").
EliteSQLError.IO = 1;                 // also a lost connection: a sent commit MAY be published
EliteSQLError.CORRUPT = 2;
EliteSQLError.CONFLICT_RETRY = 9;     // safe to retry the whole transaction
EliteSQLError.DATABASE_LOCKED = 10;
EliteSQLError.UNIQUE_VIOLATION = 11;
EliteSQLError.READ_ONLY = 13;
EliteSQLError.MEMORY_LIMIT = 16;
EliteSQLError.COMMIT_UNKNOWN = 17;    // the write IS published; crash durability unknown
EliteSQLError.QUERY_INTERRUPTED = 18;
EliteSQLError.AUTH = 20;
EliteSQLError.TRANSACTION_EXPIRED = 21; // rolled back at the sidecar deadline; retry as a whole
/** Nothing of the failed unit of work can have been published. */
Object.defineProperty(EliteSQLError.prototype, 'retrySafe', {
  get() { return this.code === EliteSQLError.CONFLICT_RETRY || this.code === EliteSQLError.TRANSACTION_EXPIRED; },
});
/** The write may already be visible despite the error: verify before retrying. */
Object.defineProperty(EliteSQLError.prototype, 'maybePublished', {
  get() { return this.code === EliteSQLError.COMMIT_UNKNOWN || this.code === EliteSQLError.IO; },
});

class SidecarClient {
  constructor(socket) {
    this._socket = socket;
    this._pending = [];
    this._pendingBytes = 0;
    this._closed = false;
    this._writeQueue = Promise.resolve();
    this._cursorActive = false;
    const rl = readline.createInterface({ input: socket });
    this._closing = false;
    this._drainWaiter = null;
    rl.on('line', (line) => {
      const waiter = this._pending.shift();
      if (!waiter) return;
      this._pendingBytes -= waiter.bytes;
      try {
        const response = JSON.parse(line);
        if (response.ok) waiter.resolve(response.result);
        else waiter.reject(new EliteSQLError(response.code ?? 1, response.error ?? 'unknown'));
      } catch (e) {
        waiter.reject(e);
      }
      if (this._pending.length === 0 && this._drainWaiter) {
        const drained = this._drainWaiter;
        this._drainWaiter = null;
        drained();
      }
    });
    socket.on('error', (e) => this._failAll(e));
    socket.on('close', () => this._failAll(new EliteSQLError(1, 'sidecar closed the connection')));
  }

  /**
   * Connects to `elitesql serve`, over either transport:
   *
   *   SidecarClient.connect('/tmp/elitesql.sock')            // Unix socket
   *   SidecarClient.connect({ host, port: 7070, token })     // TCP
   *
   * A Unix socket is authenticated by filesystem permissions. TCP is not, so
   * the token is required and is sent as the first request on the connection.
   * The protocol is not encrypted: reach another host through an SSH tunnel,
   * a VPN or a private network.
   */
  static connect(target) {
    const tcp = typeof target === 'object' && target !== null;
    if (tcp && !target.token) {
      return Promise.reject(new EliteSQLError(20, 'a TCP sidecar requires a token'));
    }
    return new Promise((resolve, reject) => {
      const socket = tcp
        ? net.createConnection({ host: target.host ?? '127.0.0.1', port: target.port })
        : net.createConnection(target);
      socket.once('error', reject);
      socket.once('connect', async () => {
        // Nagle would add latency to this protocol's small round trips.
        if (tcp) socket.setNoDelay(true);
        const client = new SidecarClient(socket);
        if (!tcp) return resolve(client);
        try {
          await client._call({ op: 'auth', token: target.token });
          resolve(client);
        } catch (e) {
          socket.destroy();
          reject(e);
        }
      });
    });
  }

  _failAll(err) {
    this._closed = true;
    const pending = this._pending;
    this._pending = [];
    this._pendingBytes = 0;
    for (const waiter of pending) waiter.reject(err);
    if (this._drainWaiter) {
      const drained = this._drainWaiter;
      this._drainWaiter = null;
      drained();
    }
  }

  _call(request) {
    if (this._closed || this._closing || this._socket.destroyed) {
      return Promise.reject(new EliteSQLError(1, 'sidecar connection is closed'));
    }
    if (this._cursorActive && !['query_open', 'query_next', 'query_close'].includes(request.op)) {
      return Promise.reject(new EliteSQLError(
        8,
        'a streaming cursor owns this connection until it is exhausted or closed',
      ));
    }
    let payload;
    try {
      payload = JSON.stringify(request) + '\n';
      if (Buffer.byteLength(payload) > 8 * 1024 * 1024) throw new RangeError('sidecar request exceeds the 8 MiB frame limit');
    } catch (error) { return Promise.reject(error); }
    const bytes = Buffer.byteLength(payload);
    if (this._pending.length >= 128 || this._pendingBytes + bytes > 16 * 1024 * 1024) {
      return Promise.reject(new EliteSQLError(8, 'sidecar request queue is full; await outstanding requests'));
    }
    return new Promise((resolve, reject) => {
      this._pendingBytes += bytes;
      this._pending.push({ resolve, reject, bytes });
      this._writeQueue = this._writeQueue.then(async () => {
        if (this._closed || this._socket.destroyed) throw new EliteSQLError(1, 'sidecar connection is closed');
        if (!this._socket.write(payload)) {
          await new Promise((drained, failed) => {
            const cleanup = () => { this._socket.off('drain', onDrain); this._socket.off('error', onError); this._socket.off('close', onClose); };
            const onDrain = () => { cleanup(); drained(); };
            const onError = error => { cleanup(); failed(error); };
            const onClose = () => onError(new EliteSQLError(1, 'sidecar closed while waiting for drain'));
            this._socket.once('drain', onDrain);
            this._socket.once('error', onError);
            this._socket.once('close', onClose);
          });
        }
      }).catch(error => { this._failAll(error); this._socket.destroy(); });
    });
  }

  async ping() {
    return (await this._call({ op: 'ping' })) === 'pong';
  }

  async query(sql, params, { timeoutMs } = {}) {
    const request = { op: 'query', sql };
    if (timeoutMs !== undefined) {
      if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 0) throw new RangeError('timeoutMs must be a nonnegative safe integer');
      request.timeout_ms = timeoutMs;
    }
    if (params !== undefined) request.params = encodeParams(params);
    return decodeResult(await this._call(request));
  }

  async stream(sql, params, { batchRows = 512, timeoutMs } = {}) {
    if (!Number.isInteger(batchRows) || batchRows < 1 || batchRows > 4096) {
      throw new RangeError('batchRows must be between 1 and 4096');
    }
    if (this._cursorActive) {
      throw new EliteSQLError(8, 'a streaming cursor is already active');
    }
    this._cursorActive = true;
    const request = { op: 'query_open', sql };
    if (timeoutMs !== undefined) {
      if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 0) throw new RangeError('timeoutMs must be a nonnegative safe integer');
      request.timeout_ms = timeoutMs;
    }
    if (params !== undefined) request.params = encodeParams(params);
    try {
      const opened = await this._call(request);
      return new SidecarQueryCursor(this, opened.columns, batchRows);
    } catch (error) {
      this._cursorActive = false;
      throw error;
    }
  }

  async searchVector(table, column, vector, { topK = 10, efSearch, filter } = {}) {
    const request = { op: 'search_vector', table, column, vector, top_k: topK };
    if (efSearch !== undefined) request.ef_search = efSearch;
    if (filter !== undefined) request.filter = filter;
    const { hits } = await this._call(request);
    return hits.map((h) => ({ ...h, record: decodeRecord(h.record) }));
  }

  createVectorIndex(table, column, { metric = 'cosine', mode = 'sync', m, efConstruction, quantized } = {}) {
    const request = { op: 'create_vector_index', table, column, metric, mode };
    if (m !== undefined) request.m = m;
    if (efConstruction !== undefined) request.ef_construction = efConstruction;
    if (quantized) request.quantized = true;
    return this._call(request);
  }

  createTextIndex(table, column) {
    return this._call({ op: 'create_text_index', table, column });
  }

  async searchText(table, column, query, { topK = 10, filter } = {}) {
    const request = { op: 'search_text', table, column, query, top_k: topK };
    if (filter !== undefined) request.filter = filter;
    const { hits } = await this._call(request);
    return hits.map((h) => ({ ...h, record: decodeRecord(h.record) }));
  }

  async searchHybrid(table, { text, vector, topK = 10, efSearch, filter } = {}) {
    const request = { op: 'search_hybrid', table, top_k: topK };
    if (text) request.text = { column: text[0], query: text[1] };
    if (vector) request.vector = { column: vector[0], vector: vector[1] };
    if (efSearch !== undefined) request.ef_search = efSearch;
    if (filter !== undefined) request.filter = filter;
    const { hits } = await this._call(request);
    return hits.map((h) => ({ ...h, record: decodeRecord(h.record) }));
  }

  checkpoint() {
    return this._call({ op: 'checkpoint' });
  }

  compact() {
    return this._call({ op: 'compact' });
  }

  /**
   * Stops accepting requests, waits for every request already sent to be
   * answered (the server executes them regardless), then ends the socket.
   * Rejecting them early would report as failed a write the server commits.
   */
  async close() {
    if (this._closed) return;
    this._closing = true;
    if (this._pending.length > 0) {
      await new Promise((resolve) => {
        this._drainWaiter = resolve;
        // A dying socket still resolves this through _failAll.
      });
    }
    this._closed = true;
    this._socket.end();
  }
}

class SidecarQueryCursor {
  constructor(client, columns, batchRows) {
    this.client = client;
    this.columns = columns;
    this.batchRows = batchRows;
    this.done = false;
  }

  async nextBatch(maxRows = this.batchRows) {
    if (this.done) return [];
    if (!Number.isInteger(maxRows) || maxRows < 1 || maxRows > 4096) {
      throw new RangeError('maxRows must be between 1 and 4096');
    }
    const result = await this.client._call({ op: 'query_next', max_rows: maxRows });
    const rows = result.rows.map((row) => row.map(decodeValue));
    if (result.done) {
      this.done = true;
      this.client._cursorActive = false;
    }
    return rows;
  }

  async close() {
    if (this.done) return;
    try {
      await this.client._call({ op: 'query_close' });
    } finally {
      this.done = true;
      this.client._cursorActive = false;
    }
  }

  async *[Symbol.asyncIterator]() {
    try {
      while (!this.done) {
        const rows = await this.nextBatch();
        for (const row of rows) yield row;
      }
    } finally {
      await this.close();
    }
  }
}

module.exports = {
  SidecarClient,
  SidecarQueryCursor,
  EliteSQLError,
  decodeValue,
  encodeParam,
  encodeParams,
  timestampMicros,
  parseJsonExact,
};
