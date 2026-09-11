'use strict';

const assert = require('node:assert/strict');
const { PassThrough } = require('node:stream');
const { SidecarClient, EliteSQLError, decodeValue, encodeParam } = require('./elitesql');

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
  client.close();

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
  earlyClient.close();
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
  slowClient.close();

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
