// 測試用的假 agent：以 NDJSON 協定（crates/kairos-proto/PROTOCOL.md）連上 Kairos 的 agent 連接埠，
// 送出 v2 hello（metrics、durable）、補送過去 2 小時的回報（每 4 分鐘一筆），之後每 5 秒一筆。
// （Kairos 依序確認每一筆 durable 回報；補送量太大時即時的回報會排在後面，測試期間看不到最近的資料）
// 只用 Node.js 內建模組；Kairos 以 loopback、未啟用 mTLS 的設定執行（tests/run.sh）。

import tls from 'node:tls';
import { randomUUID } from 'node:crypto';

export class FakeAgent {
  constructor({ port, agentId, hostname, tags = [], os = { name: 'Ubuntu', version: '24.04', arch: 'x86_64' }, backfillMinutes = 120 }) {
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
    this.send({
      type: 'report',
      ts: new Date(tsMs).toISOString(),
      metrics: [
        { name: 'system.cpu.usage', value: Math.round(cpu * 10) / 10, unit: '%' },
        { name: 'system.memory.total', value: total, unit: 'By' },
        { name: 'system.memory.used', value: used, unit: 'By' },
      ],
      boot_id: this.bootId,
      report_seq: this.reportSeq,
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
          features: ['metrics', 'durable'],
          agent_id: this.agentId,
          hostname: this.hostname,
          agent_version: '0.6.0',
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
