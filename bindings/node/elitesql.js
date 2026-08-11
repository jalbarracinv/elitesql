// EliteSQL Node client for the sidecar mode (`elitesql serve <db> <socket>`).
// Zero dependencies. Promise-based; requests are answered in order.
//
//   const { SidecarClient } = require('./elitesql');
//   const db = await SidecarClient.connect('/tmp/elitesql.sock');
//   await db.query("CREATE TABLE docs (title text NOT NULL)");
//   const { inserted } = await db.query("INSERT INTO docs (title) VALUES ('hola')");
//   const { columns, rows } = await db.query('SELECT * FROM docs');
//
// Values: scalars are native; date/time/timestamp arrive as Date objects
// (dates at UTC midnight), blobs as Buffer, vectors as number arrays.

'use strict';

const net = require('net');
const readline = require('readline');

const US_PER_DAY = 86_400_000_000n;

function decodeValue(v) {
  if (v && typeof v === 'object' && !Array.isArray(v) && '$t' in v) {
    switch (v.$t) {
      case 'date':
        return new Date(v.days * 86_400_000);
      case 'time': // milliseconds since midnight as a number
        return v.us / 1000;
      case 'timestamp':
        return new Date(Number(BigInt(v.us) / 1000n));
      case 'blob':
        return Buffer.from(v.hex, 'hex');
      case 'vector':
        return v.v;
      case 'json':
        return v.v;
      case 'float64':
        return Number(v.repr);
      default:
        return v;
    }
  }
  return v;
}

function decodeResult(result) {
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
    return { $t: 'timestamp', us: value.getTime() * 1000 };
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
EliteSQLError.CONFLICT_RETRY = 9;
EliteSQLError.COMMIT_UNKNOWN = 17;

class SidecarClient {
  constructor(socket) {
    this._socket = socket;
    this._pending = [];
    const rl = readline.createInterface({ input: socket });
    rl.on('line', (line) => {
      const waiter = this._pending.shift();
      if (!waiter) return;
      try {
        const response = JSON.parse(line);
        if (response.ok) waiter.resolve(response.result);
        else waiter.reject(new EliteSQLError(response.code ?? 1, response.error ?? 'unknown'));
      } catch (e) {
        waiter.reject(e);
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
    const pending = this._pending;
    this._pending = [];
    for (const waiter of pending) waiter.reject(err);
  }

  _call(request) {
    return new Promise((resolve, reject) => {
      this._pending.push({ resolve, reject });
      this._socket.write(JSON.stringify(request) + '\n');
    });
  }

  async ping() {
    return (await this._call({ op: 'ping' })) === 'pong';
  }

  async query(sql, params) {
    const request = { op: 'query', sql };
    if (params !== undefined) request.params = encodeParams(params);
    return decodeResult(await this._call(request));
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

  close() {
    this._socket.end();
  }
}

module.exports = { SidecarClient, EliteSQLError, decodeValue, encodeParam, encodeParams };
