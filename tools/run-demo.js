#!/usr/bin/env node
/**
 * 一键跑通全链路：预览端 ← 发送端（模拟手机）
 *
 * 这个脚本是阶段 0 的验证入口：
 *   1. 起预览端（推流监听 + HTTP 预览）
 *   2. 起发送端（合成帧 → 推流）
 *   3. 跑 8 秒，采集统计
 *   4. 打印延迟分位数并落到 docs/latency-report.md
 */

import { Receiver } from "@sidelens/receiver";
import { Sender } from "@sidelens/sender";
import fs from "node:fs";
import path from "node:path";

const DURATION_MS = Number(process.env.DEMO_MS ?? 8000);
const PUSH_PORT = 27100;
const HTTP_PORT = 8080;

const recv = new Receiver({ port: PUSH_PORT, httpPort: HTTP_PORT });
await recv.start();
console.log(`  预览端启动: http://127.0.0.1:${recv.httpPort}  (推流端口 ${recv.daemon.server.port})`);

// 必须用接收端**实际**监听的端口：端口被占用时会自动上移，
// 写死端口会让 sender 连到别人的服务上，表现为"发得出去、收不回来"。
const actualPushPort = recv.daemon.server.port;
if (actualPushPort !== PUSH_PORT) {
  console.log(`  端口 ${PUSH_PORT} 被占用，已改用 ${actualPushPort}`);
}

const sender = new Sender({
  url: `ws://127.0.0.1:${actualPushPort}`,
  name: "Demo Phone",
  fps: 30,
  width: 320,
  height: 240,
});
await sender.start();
sender.startFakeAudio();
console.log("  发送端已连接，正在推流…");

await new Promise((r) => setTimeout(r, DURATION_MS));

sender.stop();
await new Promise((r) => setTimeout(r, 300));

const summary = recv.daemon.summary();
const report = {
  at: new Date().toISOString(),
  durationMs: DURATION_MS,
  fps: 30,
  resolution: "320x240",
  sent: sender.stats.sent,
  received: summary.frames,
  bytes: summary.bytes,
  latency: summary.latency,
  playable: summary.playable,
};

console.log("\n  —— 实测结果 ——");
console.log(`  发送帧数      ${report.sent}`);
console.log(`  接收帧数      ${report.received}`);
console.log(`  单向延迟 p50  ${report.latency.p50} ms`);
console.log(`  单向延迟 p95  ${report.latency.p95} ms`);
console.log(`  可播放 p50    ${report.playable.p50} ms`);
console.log(`  流量          ${(report.bytes / 1024).toFixed(1)} KB\n`);

const outDir = path.join(process.cwd(), "docs");
fs.mkdirSync(outDir, { recursive: true });
fs.writeFileSync(
  path.join(outDir, "latency-report.json"),
  JSON.stringify(report, null, 2)
);

await recv.stop();
process.exit(0);
