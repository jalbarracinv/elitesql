'use strict';

const assert = require('node:assert/strict');
const { PassThrough } = require('node:stream');
const { SidecarClient, EliteSQLError } = require('./elitesql');

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
}

main().catch((error) => {
  process.stderr.write(`${error.stack || error}\n`);
  process.exitCode = 1;
});
