#!/usr/bin/env node
/**
 * sidelenssend —— 发送端 CLI（模拟手机）
 *
 * 用法:
 *   node apps/sender/src/cli.js --host 127.0.0.1 --port 27100 --fps 30
 */

import { Sender } from "./index.js";

function parseArgs(argv) {
  const out = { host: "127.0.0.1", port: 27100, fps: 30, width: 320, height: 240, name: "SideLens Device", audio: false };
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--host" || a === "-H") out.host = argv[++i];
    else if (a === "--port" || a === "-p") out.port = Number(argv[++i]);
    else if (a === "--fps" || a === "-f") out.fps = Number(argv[++i]);
    else if (a === "--size") { const [w, h] = argv[++i].split("x").map(Number); out.width = w; out.height = h; }
    else if (a === "--name" || a === "-n") out.name = argv[++i];
    else if (a === "--audio") out.audio = true;
    else if (a === "--help" || a === "-h") out.help = true;
  }
  return out;
}

const args = parseArgs(process.argv);
if (args.help) {
  console.log(`SideLens 发送端（模拟手机）

  --host, -H   电脑地址（默认 127.0.0.1）
  --port, -p   电脑端口（默认 27100）
  --fps, -f    帧率（默认 30）
  --size       分辨率，如 640x480（默认 320x240）
  --name, -n   设备显示名
  --audio      同时发送模拟音频
  --help, -h   显示本帮助
`);
  process.exit(0);
}

const sender = new Sender({
  url: `ws://${args.host}:${args.port}`,
  name: args.name,
  fps: args.fps,
  width: args.width,
  height: args.height,
});
await sender.start();
if (args.audio) sender.startFakeAudio();

console.log(`\n  SideLens 发送端已连接 ${args.host}:${args.port}`);
console.log(`  ${args.width}x${args.height} @ ${args.fps}fps${args.audio ? " + 音频" : ""}`);
console.log(`  UDP 广播已开，电脑可自动发现\n`);

setInterval(() => {
  const s = sender.stats;
  console.log(`[sender] 已发=${s.sent} 关键帧=${s.keyframes} 流量=${(s.bytes / 1024).toFixed(0)}KB`);
}, 3000);

process.on("SIGINT", () => { sender.stop(); console.log("\n  已停止\n"); process.exit(0); });
