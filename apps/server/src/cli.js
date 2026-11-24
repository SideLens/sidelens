#!/usr/bin/env node
/**
 * sidelensd —— 电脑端常驻服务 CLI
 *
 * 用法:
 *   node apps/server/src/cli.js --port 27100
 *   vp run server
 */

import { SideLensDaemon } from "./index.js";

function parseArgs(argv) {
  const out = { port: 27100, name: "SideLens Host" };
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--port" || a === "-p") out.port = Number(argv[++i]);
    else if (a === "--name" || a === "-n") out.name = argv[++i];
    else if (a === "--help" || a === "-h") out.help = true;
  }
  return out;
}

const args = parseArgs(process.argv);
if (args.help) {
  console.log(`SideLens 常驻服务

  --port, -p   监听端口（默认 27100）
  --name, -n   设备显示名（默认 "SideLens Host"）
  --help, -h   显示本帮助
`);
  process.exit(0);
}

const daemon = new SideLensDaemon({ port: args.port, deviceName: args.name });
const { port } = await daemon.start();

console.log(`\n  SideLens 已启动`);
console.log(`  推流地址  ws://<本机IP>:${port}`);
console.log(`  自动发现  UDP 27101 广播中`);
console.log(`\n  等待手机端连接... (Ctrl+C 退出)\n`);

// 每 2 秒打一次统计，肉眼可见地证明链路在工作
const timer = setInterval(() => {
  const s = daemon.summary();
  if (!s.frames) return;
  console.log(
    `[stats] 帧=${s.frames} 延迟 p50=${s.latency.p50}ms p95=${s.latency.p95}ms ` +
    `可播放 p50=${s.playable.p50}ms 流量=${(s.bytes / 1024).toFixed(0)}KB`
  );
}, 2000);

const shutdown = async () => {
  clearInterval(timer);
  await daemon.stop();
  console.log("\n  已停止\n");
  process.exit(0);
};
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
