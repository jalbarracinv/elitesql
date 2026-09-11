'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const { SidecarClient } = require('./elitesql');

async function main() {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'elitesql-node-'));
  const socket = path.join(dir, 'sidecar.sock');
  const binary = process.env.ELITESQL_BIN || path.resolve(__dirname, '../../target/debug/elitesql');
  const server = spawn(binary, ['--create', 'serve', path.join(dir, 'db'), socket], { stdio: 'ignore' });
  let startupError;
  server.on('error', error => { startupError = error; });
  let client;
  try {
    const deadline = Date.now() + 10000;
    while (!client) {
      if (startupError) throw startupError;
      try { client = await SidecarClient.connect(socket); }
      catch (error) {
        if (Date.now() >= deadline || server.exitCode !== null) throw error;
        await new Promise(resolve => setTimeout(resolve, 20));
      }
    }
    await client.query('CREATE TABLE numbers(n int)');
    const values = [-(2n ** 63n), -9007199254740993n, 9007199254740991, 9007199254740993n, 2n ** 63n - 1n];
    for (const value of values) await client.query('INSERT INTO numbers(n) VALUES(?)', [value]);
    const result = await client.query('SELECT n FROM numbers');
    assert.deepEqual(result.rows, values.map(value => [value]));
    const cursor = await client.stream('SELECT n FROM numbers', undefined, { batchRows: 2 });
    const streamed = [];
    for await (const row of cursor) streamed.push(row);
    assert.deepEqual(streamed, result.rows);
    await client.query('CREATE TABLE identities(id int AUTO_INCREMENT PRIMARY KEY)');
    const inserted = await client.query('INSERT INTO identities(id) VALUES(?)', [9007199254740993n]);
    assert.equal(inserted.lastrowid, 9007199254740993n);
    assert.deepEqual(inserted.identity.values, [9007199254740993n]);
    const early = await client.stream('SELECT n FROM numbers', undefined, { batchRows: 1 });
    for await (const row of early) { assert.deepEqual(row, [values[0]]); break; }
    assert.equal(await client.ping(), true);
    await assert.rejects(
      client.query('SELECT n FROM numbers', undefined, { timeoutMs: 0 }),
      error => error.code === 18,
    );
    assert.equal(await client.ping(), true);
  } finally {
    if (client) client.close();
    if (server.exitCode === null && !startupError) {
      const exited = once(server, 'exit');
      server.kill();
      await exited;
    }
    await fs.rm(dir, { recursive: true, force: true });
  }
}

main().catch(error => { process.stderr.write(`${error.stack}\n`); process.exitCode = 1; });
