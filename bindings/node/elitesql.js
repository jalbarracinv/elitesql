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

class EliteSQLError extends Error {
  constructor(code, message) {
    super(`[elitesql:${code}] ${message}`);
    this.code = code;
  }
}
EliteSQLError.CONFLICT_RETRY = 9;

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

  static connect(socketPath) {
    return new Promise((resolve, reject) => {
      const socket = net.createConnection(socketPath);
      socket.once('connect', () => resolve(new SidecarClient(socket)));
      socket.once('error', reject);
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

  async query(sql) {
    return decodeResult(await this._call({ op: 'query', sql }));
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

module.exports = { SidecarClient, EliteSQLError, decodeValue };
