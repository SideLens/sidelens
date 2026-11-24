/**
 * 设备发现（demo 版：UDP 广播 + 手动 IP）
 *
 * 生产版要做 mDNS（_sidelens._tcp），但 UDP 广播足够 demo 用，
 * 且不依赖 Bonjour / avahi 的安装情况 —— 后者在 Windows 上经常缺失。
 */

import dgram from "node:dgram";
import os from "node:os";

export const DISCOVERY_PORT = 27101;
const MAGIC = "SIDELENS_DISCOVER";

/** 拿到本机所有可用的 IPv4 广播地址 */
export function localBroadcastAddresses() {
  const out = [];
  for (const [name, addrs] of Object.entries(os.networkInterfaces())) {
    for (const a of addrs ?? []) {
      if (a.family !== "IPv4" || a.internal) continue;
      const ip = a.address.split(".").map(Number);
      const mask = a.netmask.split(".").map(Number);
      const bcast = ip.map((o, i) => (o & mask[i]) | (~mask[i] & 255)).join(".");
      out.push({ iface: name, address: a.address, broadcast: bcast });
    }
  }
  return out;
}

/** 广播端：让局域网内的电脑能看到这台手机。 */
export class Beacon {
  constructor({ port = DISCOVERY_PORT, name = "SideLens", meta = {}, intervalMs = 1000 } = {}) {
    this.port = port;
    this.name = name;
    this.meta = meta;
    this.intervalMs = intervalMs;
    this.socket = null;
    this.timer = null;
  }

  start() {
    this.socket = dgram.createSocket({ type: "udp4", reuseAddr: true });
    this.socket.bind(() => {
      this.socket.setBroadcast(true);
      this.#tick();
      this.timer = setInterval(() => this.#tick(), this.intervalMs);
      this.timer.unref?.();
    });
    return this;
  }

  #tick() {
    const payload = Buffer.from(JSON.stringify({
      app: "sidelens",
      v: 0,
      name: this.name,
      port: this.port,
      ...this.meta,
    }));
    const packet = Buffer.concat([Buffer.from(MAGIC, "utf8"), payload]);
    for (const b of localBroadcastAddresses()) {
      this.socket.send(packet, DISCOVERY_PORT, b.broadcast, () => {});
    }
  }

  stop() {
    clearInterval(this.timer);
    this.socket?.close();
  }
}

/** 监听端：电脑侧收广播，维护在线设备表。 */
export class Discoverer {
  constructor({ port = DISCOVERY_PORT, ttlMs = 4000 } = {}) {
    this.port = port;
    this.ttlMs = ttlMs;
    /** @type {Map<string, {name:string, address:string, port:number, lastSeen:number, meta:object}>} */
    this.devices = new Map();
    this.socket = null;
  }

  async start() {
    this.socket = dgram.createSocket({ type: "udp4", reuseAddr: true });
    this.socket.on("message", (msg, rinfo) => {
      if (msg.subarray(0, MAGIC.length).toString("utf8") !== MAGIC) return;
      let info;
      try {
        info = JSON.parse(msg.subarray(MAGIC.length).toString("utf8"));
      } catch {
        return;
      }
      if (info?.app !== "sidelens") return;
      this.devices.set(rinfo.address, {
        name: info.name ?? "未命名设备",
        address: rinfo.address,
        port: info.port ?? 27100,
        lastSeen: Date.now(),
        meta: info,
      });
    });

    await new Promise((resolve, reject) => {
      this.socket.once("listening", resolve);
      this.socket.once("error", reject);
      this.socket.bind(this.port);
    });
    return this;
  }

  list() {
    const now = Date.now();
    for (const [addr, d] of this.devices) {
      if (now - d.lastSeen > this.ttlMs) this.devices.delete(addr);
    }
    return [...this.devices.values()].sort((a, b) => b.lastSeen - a.lastSeen);
  }

  stop() {
    this.socket?.close();
  }
}
