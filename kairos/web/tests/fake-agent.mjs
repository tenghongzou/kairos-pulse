// 測試用的假 agent：以 NDJSON 協定（crates/kairos-proto/PROTOCOL.md）連上 Kairos 的 agent 連接埠，
// 送出 v2 hello（metrics、durable）、補送過去 2 小時的回報（每 4 分鐘一筆），之後每 5 秒一筆。
// f4: true 時模擬 Pulse 0.8.0（crates/kairos-proto/METRICS.md）：另宣告 inventory、processes，hello_ack 後送出資產，
// 回報帶新的標準指標（負載、置換空間、掛載點的使用率、磁碟讀寫、網路封包、行程數、運作時間），每 4 筆附上行程排行。
// agentVersion／install 指定時模擬 Pulse 0.9（遠端管理，F8）：hello 帶 install（安裝方式與遠端升級、集中設定的本機狀態）。
// 未啟用 mTLS 的連線不協商 config／upgrade（API.md 第 17 節），資格與狀態只依 hello 的版本與 install。
// （Kairos 依序確認每一筆 durable 回報；補送量太大時即時的回報會排在後面，測試期間看不到最近的資料）
// 只用 Node.js 內建模組；Kairos 以 loopback、未啟用 mTLS 的設定執行（tests/run.sh）。

import tls from 'node:tls';
import { randomUUID } from 'node:crypto';

export class FakeAgent {
  constructor({ port, agentId, hostname, tags = [], os = { name: 'Ubuntu', version: '24.04', arch: 'x86_64' }, backfillMinutes = 120, f4 = false, agentVersion = null, install = null }) {
    this.f4 = f4;
    this.agentVersion = agentVersion;
    this.install = install;
    this.bootMs = Date.now() - 3 * 86400000;
    this.port = port;
    this.agentId = agentId;
    this.hostname = hostname;
    this.tags = tags;
    this.os = os;
    this.backfillMinutes = backfillMinutes;
    this.seq = 0;
    this.reportSeq = 0;
    this.bootId = randomUUID();
    this.sock = null;
    this.timer = null;
    this.acked = 0;
  }

  send(obj) {
    this.seq += 1;
    this.sock.write(`${JSON.stringify({ ...obj, seq: this.seq })}\n`);
  }

  report(tsMs) {
    const cpu = 20 + 15 * Math.sin(tsMs / 600000) + (this.hostname.length % 7) * 5;
    const total = 8 * 1024 ** 3;
    const used = Math.round(total * (0.4 + 0.1 * Math.cos(tsMs / 900000)));
    this.reportSeq += 1;
    const metrics = [
      { name: 'system.cpu.usage', value: Math.round(cpu * 10) / 10, unit: '%' },
      { name: 'system.memory.total', value: total, unit: 'By' },
      { name: 'system.memory.used', value: used, unit: 'By' },
    ];
    const extra = {};
    if (this.f4) {
      const secs = Math.floor((tsMs - this.bootMs) / 1000);
      const fs = (mp, fstype, tot, usedPct) => {
        const usedB = Math.round(tot * usedPct);
        const avail = Math.max(0, tot - usedB - Math.round(tot * 0.02));
        const labels = { mountpoint: mp, fstype };
        return [
          { name: 'system.filesystem.total', value: tot, unit: 'By', labels },
          { name: 'system.filesystem.available', value: avail, unit: 'By', labels },
          { name: 'system.filesystem.used', value: usedB, unit: 'By', labels },
          { name: 'system.filesystem.usage', value: Math.round((usedB / (usedB + avail)) * 10000) / 100, unit: '%', labels },
        ];
      };
      const io = (dev, k) => [
        { name: 'system.disk.read', value: Math.round(secs * 120000 * k), unit: 'By', labels: { device: dev } },
        { name: 'system.disk.written', value: Math.round(secs * 450000 * k), unit: 'By', labels: { device: dev } },
        { name: 'system.disk.read_ops', value: Math.round(secs * 12 * k), unit: '{operations}', labels: { device: dev } },
        { name: 'system.disk.write_ops', value: Math.round(secs * 40 * k), unit: '{operations}', labels: { device: dev } },
        { name: 'system.disk.io_time', value: Math.round(secs * 80 * k), unit: 'ms', labels: { device: dev } },
      ];
      metrics.push(
        { name: 'system.cpu.load_1m', value: 1.25, unit: '1' },
        { name: 'system.cpu.load_5m', value: 0.98, unit: '1' },
        { name: 'system.cpu.load_15m', value: 0.75, unit: '1' },
        { name: 'system.memory.available', value: total - used, unit: 'By' },
        { name: 'system.swap.total', value: 2 * 1024 ** 3, unit: 'By' },
        { name: 'system.swap.used', value: 128 * 1024 ** 2, unit: 'By' },
        { name: 'system.process.count', value: 214, unit: '{processes}' },
        { name: 'system.uptime', value: secs, unit: 's' },
        ...fs('/', 'ext4', 100e9, 0.42),
        ...fs('/data', 'xfs', 2000e9, 0.7),
        ...fs('/var/log', 'xfs', 50e9, 0.96),
        ...fs('/boot', 'vfat', 1e9, 0.2),
        ...io('vda1', 1),
        ...io('vdb1', 0.4),
        { name: 'system.network.received', value: secs * 250000, unit: 'By', labels: { device: 'eth0' } },
        { name: 'system.network.transmitted', value: secs * 90000, unit: 'By', labels: { device: 'eth0' } },
        { name: 'system.network.packets_received', value: secs * 300, unit: '{packets}', labels: { device: 'eth0' } },
        { name: 'system.network.packets_transmitted', value: secs * 200, unit: '{packets}', labels: { device: 'eth0' } },
        { name: 'system.network.errors_received', value: 0, unit: '{errors}', labels: { device: 'eth0' } },
        { name: 'system.network.errors_transmitted', value: 0, unit: '{errors}', labels: { device: 'eth0' } },
      );
      if (this.reportSeq % 4 === 1) {
        extra.processes = {
          top_cpu: [
            { pid: 2211, name: 'postgres', cpu_usage: 9.41, memory: 4 * 1024 ** 3, command: '/usr/pgsql-16/bin/postgres -D /var/lib/pgsql/16/data -c config_file=/etc/postgresql.conf' },
            { pid: 1873, name: 'java', cpu_usage: 3.2, memory: 8.3 * 1024 ** 3 },
          ],
          top_memory: [
            { pid: 1873, name: 'java', cpu_usage: 3.2, memory: Math.round(8.3 * 1024 ** 3) },
            { pid: 2211, name: 'postgres', cpu_usage: 9.41, memory: 4 * 1024 ** 3 },
          ],
        };
        extra.processes.top_cpu[1].memory = Math.round(extra.processes.top_cpu[1].memory);
      }
    }
    this.send({
      type: 'report',
      ts: new Date(tsMs).toISOString(),
      metrics,
      boot_id: this.bootId,
      report_seq: this.reportSeq,
      ...extra,
    });
  }

  inventory() {
    this.send({
      type: 'inventory',
      ts: new Date().toISOString(),
      inventory: {
        os: { family: 'linux', name: 'Ubuntu', version: '24.04', long_version: 'Linux (Ubuntu 24.04)', kernel: '6.8.0-45-generic', arch: 'x86_64' },
        boot_time: new Date(this.bootMs).toISOString().replace(/\.\d+Z$/, 'Z'),
        cpu: { model: 'AMD EPYC 7763 64-Core Processor', vendor: 'AuthenticAMD', physical_cores: 4, logical_cores: 8, frequency_mhz: 2445 },
        memory: { total: 8 * 1024 ** 3, swap_total: 2 * 1024 ** 3 },
        system: { vendor: 'QEMU', product: 'Standard PC (Q35 + ICH9, 2009)', serial: 'E2E-SERIAL-01', bios_vendor: 'SeaBIOS', bios_version: '1.16.3' },
        disks: [
          { mountpoint: '/', device: 'vda1', fstype: 'ext4', total: 100e9, kind: 'hdd', removable: false, read_only: false },
          { mountpoint: '/data', device: 'vdb1', fstype: 'xfs', total: 2000e9, kind: 'hdd', removable: false, read_only: false },
        ],
        network_interfaces: [{ name: 'eth0', mac: '52:54:00:12:34:56', addresses: ['127.0.0.1/8', 'fe80::5054:ff:fe12:3456/64'], mtu: 1500 }],
      },
    });
  }

  /** 連線並送出 hello；收到 hello_ack 後補送歷史資料 */
  connect() {
    return new Promise((resolve, reject) => {
      const sock = tls.connect({ host: '127.0.0.1', port: this.port, servername: 'localhost', rejectUnauthorized: false }, () => {
        this.sock = sock;
        this.send({
          type: 'hello',
          proto: 1,
          proto_min: 1,
          proto_max: 2,
          features: this.f4 ? ['metrics', 'durable', 'inventory', 'processes'] : ['metrics', 'durable'],
          agent_id: this.agentId,
          hostname: this.hostname,
          agent_version: this.agentVersion || (this.f4 ? '0.8.0' : '0.6.0'),
          install: this.install || undefined,
          heartbeat_interval_ms: 5000,
          ts: new Date().toISOString(),
          tags: this.tags.length ? this.tags : undefined,
          report_interval_ms: 5000,
          os: this.os,
        });
      });
      let buf = '';
      let ready = false;
      sock.setEncoding('utf8');
      sock.on('data', (chunk) => {
        buf += chunk;
        let i;
        while ((i = buf.indexOf('\n')) >= 0) {
          const line = buf.slice(0, i);
          buf = buf.slice(i + 1);
          let msg;
          try {
            msg = JSON.parse(line);
          } catch {
            continue;
          }
          if (msg.type === 'hello_ack' && !ready) {
            ready = true;
            if (this.f4 && (msg.features || []).includes('inventory')) this.inventory();
            const now = Date.now();
            for (let m = this.backfillMinutes; m > 0; m -= 4) this.report(now - m * 60000);
            this.timer = setInterval(() => {
              this.report(Date.now());
              this.send({ type: 'heartbeat', ts: new Date().toISOString() });
            }, 5000);
            this.report(Date.now());
            resolve(this);
          } else if (msg.type === 'ack') {
            this.acked += 1;
          } else if (msg.type === 'error' && !ready) {
            reject(new Error(`${this.hostname}: ${msg.code} ${msg.message}`));
          }
        }
      });
      sock.on('error', (e) => {
        if (!ready) reject(e);
      });
    });
  }

  /** 中斷連線（Kairos 記為 connection_closed） */
  close() {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
    if (this.sock) this.sock.destroy();
    this.sock = null;
  }
}
