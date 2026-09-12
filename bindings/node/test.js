'use strict';

const assert = require('node:assert/strict');
const { PassThrough } = require('node:stream');
const { SidecarClient, EliteSQLError, decodeValue, encodeParam, timestampMicros, parseJsonExact } = require('./elitesql');

class FakeSocket extends PassThrough {
  constructor() {
    super();
    this.requests = [];
    this.batches = [
      { rows: [[1], [2]], done: false },
      { rows: [[3]], done: true },
    ];
  }

  write(payload) {
    const request = JSON.parse(String(payload).trim());
    this.requests.push(request);
    let result;
    switch (request.op) {
      case 'query_open':
        result = { columns: ['n'] };
        break;
      case 'query_next':
        result = this.batches.shift();
        break;
      case 'ping':
        result = 'pong';
        break;
      case 'query_close':
        result = { ok: true };
        break;
      default:
        throw new Error(`unexpected request: ${request.op}`);
    }
    queueMicrotask(() => this.push(`${JSON.stringify({ ok: true, result })}\n`));
    return true;
  }

  end() {
    this.push(null);
  }
}

async function main() {
  for (const n of [-(2n ** 63n), 2n ** 63n - 1n, 9007199254740993n]) {
    assert.equal(decodeValue(JSON.parse(JSON.stringify(encodeParam(n)))), n);
  }
  assert.equal(decodeValue(9007199254740991), 9007199254740991);
  // Non-finite floats round-trip instead of collapsing to NaN.
  assert.equal(decodeValue({ $t: 'float64', repr: 'inf' }), Infinity);
  assert.equal(decodeValue({ $t: 'float64', repr: '-inf' }), -Infinity);
  assert.ok(Number.isNaN(decodeValue({ $t: 'float64', repr: 'NaN' })));
  assert.deepEqual(encodeParam(-Infinity), { $t: 'float64', repr: '-inf' });
  // Timestamps keep their microseconds through Date and back.
  const stamp = decodeValue({ $t: 'timestamp', us: 1_700_000_000_123_456 });
  assert.ok(stamp instanceof Date);
  assert.equal(stamp.getTime(), 1_700_000_000_123);
  assert.equal(timestampMicros(stamp), 1_700_000_000_123_456n);
  assert.deepEqual(encodeParam(stamp), { $t: 'timestamp', us: 1_700_000_000_123_456 });
  assert.equal(timestampMicros(new Date(1000)), 1_000_000n);
  // JSON columns with integers beyond 2^53 stay exact.
  const exact = decodeValue({ $t: 'json', text: '{"id": 9007199254740993, "tags": ["a", 1.5, -2], "ok": true, "n": null}' });
  assert.equal(exact.id, 9007199254740993n);
  assert.deepEqual(exact.tags, ['a', 1.5, -2]);
  assert.equal(exact.ok, true);
  assert.equal(exact.n, null);
  assert.deepEqual(parseJsonExact('[1, "x\\"y", {"k": []}]'), [1, 'x"y', { k: [] }]);
  assert.deepEqual(decodeValue({ $t: 'json', v: { a: 1 } }), { a: 1 });
  assert.equal(new EliteSQLError(17, 'x').maybePublished, true);
  assert.equal(new EliteSQLError(21, 'x').retrySafe, true);
  assert.equal(new EliteSQLError(11, 'x').retrySafe, false);
  const socket = new FakeSocket();
  const client = new SidecarClient(socket);
  const cursor = await client.stream('SELECT n FROM docs', undefined, { batchRows: 2 });
  assert.deepEqual(cursor.columns, ['n']);
  await assert.rejects(client.ping(), (error) => (
    error instanceof EliteSQLError && error.code === 8
  ));

  const rows = [];
  for await (const row of cursor) rows.push(row);
  assert.deepEqual(rows, [[1], [2], [3]]);
  assert.equal(await client.ping(), true);
  assert.deepEqual(
    socket.requests.map((request) => request.op),
    ['query_open', 'query_next', 'query_next', 'ping'],
  );
  await client.close();

  const earlySocket = new FakeSocket();
  const earlyClient = new SidecarClient(earlySocket);
  const earlyCursor = await earlyClient.stream('SELECT n FROM docs');
  for await (const row of earlyCursor) {
    assert.deepEqual(row, [1]);
    break;
  }
  assert.equal(earlyCursor.done, true);
  assert.equal(await earlyClient.ping(), true);
  assert.deepEqual(earlySocket.requests.map(r => r.op), ['query_open', 'query_next', 'query_close', 'ping']);
  await earlyClient.close();
  await assert.rejects(earlyClient.ping(), /closed/);

  // A transport that refuses further writes must hold the next request until
  // drain, including when the preceding response has already arrived.
  const slowSocket = new FakeSocket();
  const normalWrite = slowSocket.write.bind(slowSocket);
  slowSocket.write = payload => { normalWrite(payload); return false; };
  const slowClient = new SidecarClient(slowSocket);
  const first = slowClient.ping();
  const second = slowClient.ping();
  assert.equal(await first, true);
  assert.equal(slowSocket.requests.length, 1);
  slowSocket.emit('drain');
  assert.equal(await second, true);
  assert.equal(slowSocket.requests.length, 2);
  slowSocket.emit('drain');
  // close() waits for an in-flight request instead of failing it: the server
  // executes what it already received.
  const inFlight = slowClient.ping();
  const closing = slowClient.close();
  await assert.rejects(slowClient.ping(), /closed/);
  slowSocket.emit('drain');
  assert.equal(await inFlight, true);
  await closing;

  const stalledSocket = new FakeSocket();
  stalledSocket.write = () => false;
  const stalledClient = new SidecarClient(stalledSocket);
  const pending = Array.from({ length: 128 }, () => stalledClient.ping());
  const rejected = Promise.allSettled(pending);
  await assert.rejects(stalledClient.ping(), /queue is full/);
  stalledSocket.destroy();
  assert.ok((await rejected).every(result => result.status === 'rejected'));
  assert.equal(stalledClient._pendingBytes, 0);
}

main().catch((error) => {
  process.stderr.write(`${error.stack || error}\n`);
  process.exitCode = 1;
});
