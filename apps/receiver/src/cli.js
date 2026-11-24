#!/usr/bin/env node
/**
 * sidelensview —— 电脑端预览
 *
 * 用法:
 *   node apps/receiver/src/cli.js --port 27100 --http 8080
 */

import { Receiver } from "./index.js";

function parseArgs(argv) {
  const out = { port: 27100, httpPort: 8080 };
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--port" || a === "-p") out.port = Number(argv[++i]);
    else if (a === "--http") out.httpPort = Number(argv[++i]);
    else if (a === "--help" || a === "-h") out.help = true;
  }
  return out;
}

const args = parseArgs(process.argv);
if (args.help) {
  console.log(`SideLens 预览端

  --port, -p   推流监听端口（默认 27100）
  --http       预览页端口（默认 8080）
  --help, -h   显示本帮助
`);
  process.exit(0);
}

const recv = new Receiver({ port: args.port, httpPort: args.httpPort });
await recv.start();

console.log(`\n  SideLens 预览已启动`);
console.log(`  打开 http://127.0.0.1:${args.httpPort}`);
console.log(`  等待手机推流到 :${args.port} ...\n`);

process.on("SIGINT", async () => { await recv.stop(); process.exit(0); });
