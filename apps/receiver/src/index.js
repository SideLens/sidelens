/**
 * SideLens 接收端（电脑侧预览）
 *
 * demo 用 HTTP 出一个 MJPEG 风格的预览页 + SSE 推统计数据。
 *
 * 为什么不是 WebRTC / WebSocket 视频：
 *   这里要验证的是"链路 + 抖动缓冲"是否正确，不是浏览器渲染性能。
 *   用 SSE 推统计 + 单独拉最新帧 PNG，能把链路指标看清楚，
 *   而且不引入任何前端构建链 —— 一个 HTML 文件就能跑。
 *
 * 真实产品里这一层会被替换成：解码 → 零拷贝上屏 + 虚拟摄像头注入。
 */

import http from "node:http";
import { SideLensDaemon } from "@sidelens/server";
import { encodePNG } from "@sidelens/proto/png";

export class Receiver {
  constructor({ port = 27100, httpPort = 8080 } = {}) {
    this.port = port;
    this.httpPort = httpPort;
    this.daemon = new SideLensDaemon({ port });
    this.latest = null;       // { png: Buffer, ptsUs: bigint, seq: number, at: number }
    this.server = null;
  }

  async start() {
    this.daemon.onFrame = (sessionId, frame) => {
      if (frame.type !== 2) return; // VIDEO
      // demo 里 payload 直接是 RGBA（未压缩），因为要证明的是链路延迟，
      // 不是编解码效率。真实实现这里是 H.264 码流。
      if (frame.payload.length !== 320 * 240 * 4) return;
      this.latest = {
        png: encodePNG(frame.payload, 320, 240),
        ptsUs: frame.ptsUs,
        seq: frame.seq,
        at: Date.now(),
      };
    };

    await this.daemon.start();

    // HTTP 端口同样做占用重试：8080 在很多开发机上早被别的东西占了
    this.server = http.createServer((req, res) => this.#route(req, res));
    let lastErr;
    for (let offset = 0; offset < 20; offset++) {
      const port = this.httpPort + offset;
      try {
        await new Promise((resolve, reject) => {
          const onError = (err) => { this.server.off("listening", onOk); reject(err); };
          const onOk = () => { this.server.off("error", onError); resolve(); };
          this.server.once("error", onError);
          this.server.once("listening", onOk);
          this.server.listen(port);
        });
        this.httpPort = port;
        return this;
      } catch (err) {
        lastErr = err;
        if (err?.code !== "EADDRINUSE") throw err;
      }
    }
    throw lastErr ?? new Error("找不到可用的 HTTP 端口");
  }

  #route(req, res) {
    const url = new URL(req.url, "http://localhost");

    if (url.pathname === "/") {
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      res.end(PAGE);
      return;
    }

    if (url.pathname === "/frame.png") {
      if (!this.latest) { res.writeHead(204); res.end(); return; }
      res.writeHead(200, {
        "content-type": "image/png",
        "cache-control": "no-store",
        "access-control-allow-origin": "*",
      });
      res.end(this.latest.png);
      return;
    }

    if (url.pathname === "/stats") {
      const s = this.daemon.summary();
      res.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
      res.end(JSON.stringify({ ...s, latest: this.latest ? { seq: this.latest.seq, at: this.latest.at } : null }));
      return;
    }

    if (url.pathname === "/stream") {
      // SSE：每秒推一次统计，前端画曲线
      res.writeHead(200, {
        "content-type": "text/event-stream",
        "cache-control": "no-store",
        connection: "keep-alive",
      });
      const t = setInterval(() => {
        const s = this.daemon.summary();
        res.write(`data: ${JSON.stringify(s)}\n\n`);
      }, 1000);
      req.on("close", () => clearInterval(t));
      return;
    }

    res.writeHead(404);
    res.end("not found");
  }

  async stop() {
    await new Promise((r) => this.server?.close(r));
    await this.daemon.stop();
  }
}

const PAGE = `<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<title>SideLens 预览</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body {
    margin: 0; min-height: 100vh; display: grid; place-items: center;
    background: #0b0f14; color: #e6edf3;
    font: 14px/1.6 ui-sans-serif, -apple-system, "Segoe UI", "PingFang SC", sans-serif;
  }
  .wrap { width: min(880px, 92vw); padding: 32px 0; }
  h1 { margin: 0 0 4px; font-size: 20px; font-weight: 650; letter-spacing: .3px; }
  .sub { color: #8b949e; font-size: 13px; margin-bottom: 24px; }
  .grid { display: grid; grid-template-columns: 320px 1fr; gap: 20px; align-items: start; }
  .screen {
    width: 320px; height: 240px; border-radius: 10px; overflow: hidden;
    background: #161b22; border: 1px solid #232b36; position: relative;
  }
  .screen img { width: 100%; height: 100%; display: block; object-fit: cover; }
  .screen .hint {
    position: absolute; inset: 0; display: grid; place-items: center;
    color: #54606d; font-size: 13px;
  }
  .metrics { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; }
  .card {
    background: #11161d; border: 1px solid #232b36; border-radius: 10px; padding: 14px 16px;
  }
  .card .k { color: #8b949e; font-size: 12px; margin-bottom: 6px; }
  .card .v { font-size: 22px; font-weight: 600; font-variant-numeric: tabular-nums; }
  .card .v small { font-size: 12px; color: #8b949e; font-weight: 400; margin-left: 3px; }
  .badge { display: inline-block; padding: 2px 8px; border-radius: 999px; font-size: 11px; }
  .ok { background: #12351f; color: #3fb950; }
  .idle { background: #2a2f36; color: #8b949e; }
  canvas { width: 100%; height: 90px; display: block; margin-top: 12px; }
</style>
</head>
<body>
<div class="wrap">
  <h1>SideLens</h1>
  <div class="sub">手机摄像头跨设备共享 · 链路预览</div>
  <div class="grid">
    <div>
      <div class="screen">
        <div class="hint" id="hint">等待手机连接…</div>
        <img id="frame" alt="" style="display:none">
      </div>
      <canvas id="spark" width="640" height="180"></canvas>
    </div>
    <div class="metrics">
      <div class="card"><div class="k">单向延迟 p50</div><div class="v" id="p50">–<small>ms</small></div></div>
      <div class="card"><div class="k">单向延迟 p95</div><div class="v" id="p95">–<small>ms</small></div></div>
      <div class="card"><div class="k">可播放延迟 p50</div><div class="v" id="play">–<small>ms</small></div></div>
      <div class="card"><div class="k">累计帧数</div><div class="v" id="frames">0</div></div>
      <div class="card"><div class="k">连接状态</div><div class="v"><span class="badge idle" id="state">空闲</span></div></div>
      <div class="card"><div class="k">流量</div><div class="v" id="bytes">0<small>KB</small></div></div>
    </div>
  </div>
</div>
<script>
const $ = (id) => document.getElementById(id);
const spark = $("spark");
const ctx = spark.getContext("2d");
const history = [];
let lastSeq = -1;

function drawSpark() {
  const w = spark.width, h = spark.height;
  ctx.clearRect(0, 0, w, h);
  if (history.length < 2) return;
  const max = Math.max(20, ...history);
  ctx.strokeStyle = "#3fb950";
  ctx.lineWidth = 2;
  ctx.beginPath();
  history.forEach((v, i) => {
    const x = (i / (history.length - 1)) * w;
    const y = h - (v / max) * (h - 8) - 4;
    i ? ctx.lineTo(x, y) : ctx.moveTo(x, y);
  });
  ctx.stroke();
}

async function tick() {
  try {
    const r = await fetch("/stats", { cache: "no-store" });
    const s = await r.json();
    $("p50").innerHTML = (s.latency.p50 || "–") + "<small>ms</small>";
    $("p95").innerHTML = (s.latency.p95 || "–") + "<small>ms</small>";
    $("play").innerHTML = (s.playable.p50 || "–") + "<small>ms</small>";
    $("frames").textContent = s.frames;
    $("bytes").innerHTML = (s.bytes / 1024).toFixed(0) + "<small>KB</small>";

    const live = s.clients > 0 && s.latest && Date.now() - s.latest.at < 2000;
    $("state").className = "badge " + (live ? "ok" : "idle");
    $("state").textContent = live ? "在线" : "空闲";

    if (live) {
      $("hint").style.display = "none";
      const img = $("frame");
      img.style.display = "block";
      if (s.latest.seq !== lastSeq) {
        lastSeq = s.latest.seq;
        img.src = "/frame.png?t=" + Date.now();
      }
    }

    if (s.latency.p50) {
      history.push(s.latency.p50);
      if (history.length > 120) history.shift();
      drawSpark();
    }
  } catch {}
  setTimeout(tick, 500);
}
tick();
</script>
</body>
</html>`;
