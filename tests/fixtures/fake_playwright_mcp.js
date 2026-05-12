#!/usr/bin/env node
'use strict';
// Echo all stdin lines back as a JSON-RPC response.
// Also emit a startup ping on stderr to verify stderr forwarding.
process.stderr.write('fake-playwright-mcp: started with args=' + JSON.stringify(process.argv.slice(2)) + '\n');

const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
    let req;
    try { req = JSON.parse(line); } catch (e) {
        process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:null,error:{code:-32700,message:String(e)}}) + '\n');
        return;
    }
    const resp = { jsonrpc: '2.0', id: req.id ?? null, result: { echo: req } };
    process.stdout.write(JSON.stringify(resp) + '\n');
});
rl.on('close', () => process.exit(0));
