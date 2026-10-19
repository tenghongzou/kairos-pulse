// S07 告警事件詳情（設計包 kairos/screens.md S07；API GET /api/v1/alerts/{event_id}、POST …/ack|assign|snooze|resolve|comments）。
//
// - 動作依 API 的 `actions`（目前可做的動作）與 alerts:write 決定；規劃提醒與紀錄只能新增註解。
// - 判讀卡片以事件的 details（中斷原因、網段統計、數值）組出，沒有另外的後端（S07「0.6 後端現況」）。
// - 0.7.0 隱藏（decisions/INTEGRATION.md §28.6）：「我也來處理（加入）」、匯出處理紀錄 PDF、通知失敗的「重送」
//   （只保留「查看錯誤」）、指派給他人時的個人通知。

import { h, icon, mount } from '../core/dom.js';
import { t, has } from '../core/strings.js';
import { api, qs } from '../core/api.js';
import * as session from '../core/session.js';
import * as sse from '../core/sse.js';
import * as fmt from '../core/format.js';
import { pageHead, card, emptyState, errorState, inlineAlert, field, button, setBusy, dl, codeTag, skeletonBlock, errorAlert } from '../ui/components.js';
import { toast } from '../ui/toast.js';
import { reasonText, reasonDetail } from './host-common.js';
import {
  sevBadge, stateText, attentionTag, flags, durationSecs, kindLabel, rootCauseLabel, timelineText,
  ackDialog, assignDialog, resolveDialog, snooze, unavailableNotice, who,
} from './alerts-common.js';

const DELIVERY_STATES = ['pending', 'held', 'sent', 'failed', 'dropped', 'merged'];

function hostLink(agentId, name) {
  if (!agentId) return name || '—';
  return h('a', { href: `/hosts/${encodeURIComponent(agentId)}`, class: 'k-host-name' }, name || agentId);
}

/** 判讀卡片（可能原因、依據、建議）：由 details 組出 */
function interpretation(e) {
  const d = e.details || {};
  const rows = [];
  let tone = e.severity === 'emergency' ? 'emergency' : e.severity === 'critical' ? 'critical' : e.severity === 'warning' ? 'warning' : null;
  if (e.category === 'aggregate' && d.subnet) {
    rows.push([t('alerts.ev.cause'), t('alerts.ev.subnetCause')]);
    rows.push([t('alerts.ev.basis'), t('alerts.ev.subnetBasis', { n: fmt.num(d.window_offline || 0), subnet: d.subnet, online: fmt.num(d.online || 0), total: fmt.num(d.total || 0) })]);
    rows.push([t('alerts.ev.advice'), h('ol', { class: 'k-fix-steps' }, h('li', null, t('alerts.ev.subnetFix1', { subnet: d.subnet })), h('li', null, t('alerts.ev.subnetFix2')), h('li', null, t('alerts.ev.ackFirst')))]);
  } else if (e.category === 'aggregate') {
    rows.push([t('alerts.ev.cause'), t('alerts.ev.massCause')]);
    if (d.window_offline != null) rows.push([t('alerts.ev.basis'), t('alerts.ev.massBasis', { n: fmt.num(d.window_offline), online: fmt.num(d.online || 0) })]);
    if (d.count != null) rows.push([t('alerts.ev.basis'), t('alerts.ev.restartBasis', { n: fmt.num(d.count), expected: fmt.num(d.expected || 0) })]);
    rows.push([t('alerts.ev.advice'), t('alerts.ev.massFix')]);
  } else if (e.kind === 'metric_threshold' && d.metric) {
    const metric = t(`alerts.metric.${d.metric}`);
    rows.push([t('alerts.ev.host'), hostLink(e.agent_id, d.hostname)]);
    rows.push([t('alerts.ev.value'), t('alerts.ev.metricValue', { metric, value: fmt.pct(d.value), op: t(`alerts.op.${d.op}`), threshold: fmt.pct(d.threshold) })]);
    if (d.ip) rows.push([t('col.ip'), h('span', { class: 'mono' }, d.ip)]);
  } else if (e.agent_id) {
    rows.push([t('alerts.ev.host'), hostLink(e.agent_id, d.hostname)]);
    if (d.ip) rows.push([t('alerts.ev.ipSubnet'), h('span', { class: 'mono' }, [d.ip, d.subnet].filter(Boolean).join(' · '))]);
    if (d.offline_reason) {
      rows.push([t('host.lastReason'), reasonDetail(d.offline_reason)]);
      if (has(`reason.${d.offline_reason}.next`)) rows.push([t('alerts.ev.advice'), t(`reason.${d.offline_reason}.next`)]);
    }
    if (d.offline_after_ms) rows.push([t('host.threshold'), t('alerts.ev.threshold', { s: Math.round(d.offline_after_ms / 1000) })]);
    if (Array.isArray(d.tags) && d.tags.length) rows.push([t('alerts.ev.tags'), d.tags.join('、')]);
    if (d.superseded != null) rows.push([t('alerts.ev.basis'), t('alerts.ev.identityBasis', { n: fmt.num(d.superseded), sources: (d.sources || []).join('、') })]);
    if (d.last_report_at) rows.push([t('host.reportAt'), fmt.timeEl(d.last_report_at)]);
  } else {
    rows.push([t('alerts.ev.what'), e.title]);
    if (d.message) rows.push([t('alerts.ev.basis'), d.message]);
    if (d.channel_name) rows.push([t('alerts.ev.channel'), h('a', { href: `/alerts/channels/${encodeURIComponent(d.channel_id)}` }, d.channel_name)]);
    tone = tone === 'warning' ? 'warning' : tone;
  }
  if (e.code) rows.push([t('err.label.code'), codeTag(e.code)]);
  return card({
    title: t('alerts.ev.interpretation'),
    status: tone === 'emergency' ? null : tone,
    cls: tone === 'emergency' ? 'ds-card--status-emergency' : null,
    body: [dl(rows), h('p', { class: 'caption' }, t('alerts.ev.interpretationNote'))],
  });
}

function childrenCard(d) {
  const ch = d.children || { total: 0, items: [] };
  if (!ch.total) return null;
  const items = ch.items || [];
  const still = items.filter((c) => c.state === 'firing' || c.state === 'acknowledged').length;
  return card({
    title: t('alerts.ev.children', { n: fmt.num(ch.total) }),
    actions: h('span', { class: 'caption' }, t('alerts.ev.childrenStill', { n: fmt.num(still) })),
    cls: 'ds-card--flush',
    body: h(
      'div',
      { class: 'ds-table-wrap k-pad' },
      h(
        'table',
        { class: 'ds-table' },
        h('caption', { class: 'ds-visually-hidden' }, t('alerts.ev.childrenCaption')),
        h(
          'thead',
          null,
          h('tr', null, h('th', { scope: 'col' }, t('alerts.col.state')), h('th', { scope: 'col' }, t('col.host')), h('th', { scope: 'col' }, t('col.ip')), h('th', { scope: 'col' }, t('alerts.col.started', { tz: fmt.tzLabel() })), h('th', { scope: 'col' }, t('host.lastReason'))),
        ),
        h(
          'tbody',
          null,
          items.map((c) => {
            const cd = c.details || {};
            return h(
              'tr',
              null,
              h('td', null, stateText(c)),
              h('td', null, c.agent_id ? hostLink(c.agent_id, cd.hostname) : h('a', { href: `/alerts/events/${encodeURIComponent(c.event_id)}` }, c.title)),
              h('td', { class: 'mono' }, cd.ip || '—'),
              h('td', null, fmt.timeEl(c.started_at, { mode: 'absolute' })),
              h('td', null, reasonText(cd.offline_reason) || '—'),
            );
          }),
        ),
      ),
      ch.total > items.length ? h('p', { class: 'caption' }, t('alerts.ev.childrenMore', { n: fmt.num(items.length), total: fmt.num(ch.total) })) : null,
    ),
  });
}

function deliveriesCard(d) {
  const list = d.deliveries || [];
  const body = list.length
    ? h(
        'ul',
        { class: 'k-check-list' },
        list.map((x) => {
          const ok = x.state === 'sent' || x.state === 'merged';
          const bad = x.state === 'failed' || x.state === 'dropped';
          const stateLabel = DELIVERY_STATES.includes(x.state) ? t(`alerts.delivery.${x.state}`) : x.state;
          return h(
            'li',
            null,
            h('span', { class: ok ? 'k-fg--online' : bad ? 'k-fg--critical' : 'k-fg--unknown' }, icon(ok ? 'status-online' : bad ? 'status-critical' : 'clock')),
            h(
              'div',
              { class: 'k-grow' },
              h('strong', null, x.channel_name || x.channel_id),
              h(
                'div',
                { class: 'caption' },
                [
                  has(`alerts.deliveryKind.${x.kind}`) ? t(`alerts.deliveryKind.${x.kind}`) : x.kind,
                  stateLabel,
                  x.response_code ? t('alerts.delivery.http', { n: x.response_code }) : null,
                  x.attempts > 1 ? t('alerts.delivery.attempts', { n: x.attempts }) : null,
                  fmt.dateTime(x.sent_at || x.created_at),
                ].filter(Boolean).join(' · '),
                x.error_code ? [' ', codeTag(x.error_code)] : null,
              ),
              x.error ? h('details', { class: 'k-details' }, h('summary', null, t('alerts.delivery.viewError')), h('p', { class: 'k-code-line' }, x.error)) : null,
            ),
          );
        }),
      )
    : h('p', { class: 'secondary' }, d.severity === 'info' || d.attention !== 'action' ? t('alerts.ev.noPush') : t('alerts.ev.noDeliveries'));
  return card({ title: t('alerts.ev.deliveries'), body });
}

export function render(ctx) {
  const root = ctx.root;
  const id = ctx.params.id;
  const canWrite = session.can('alerts:write');
  let disposed = false;
  let current = null;
  let pending = null;
  ctx.setTitle(id);
  mount(root, pageHead({ title: id, crumbs: [{ text: t('alerts.events.title'), href: '/alerts/events' }, { text: id }] }), skeletonBlock(6));

  async function load({ keepFocus = null } = {}) {
    let d;
    try {
      d = await api(`/api/v1/alerts/${encodeURIComponent(id)}`);
    } catch (err) {
      if (disposed) return;
      if (err.code === 'event_not_found') {
        mount(
          root,
          pageHead({ title: t('alerts.ev.notFound'), crumbs: [{ text: t('alerts.events.title'), href: '/alerts/events' }, { text: id }] }),
          emptyState({ title: t('alerts.ev.notFound'), body: t('alerts.ev.notFoundBody', { id }), iconName: 'search', actions: [h('a', { class: 'ds-button ds-button--secondary', href: '/alerts/events' }, t('alerts.ev.back'))] }),
        );
        return;
      }
      mount(
        root,
        pageHead({ title: id, crumbs: [{ text: t('alerts.events.title'), href: '/alerts/events' }, { text: id }] }),
        err.code === 'alerts_unavailable' ? unavailableNotice() : null,
        errorState(err, { title: t('alerts.ev.failed'), onRetry: () => load() }),
      );
      return;
    }
    if (disposed) return;
    current = d;
    draw(d);
    if (keepFocus) {
      const el = root.querySelector(keepFocus);
      if (el) el.focus();
    }
  }

  function draw(e) {
    ctx.setTitle(`${e.event_id} ${e.title}`);
    const actions = new Set(canWrite ? e.actions || [] : []);
    const me = session.current() && session.current().user ? session.current().user.username : null;
    const resolved = e.state === 'resolved';
    const after = async (ok) => {
      if (ok) await load({ keepFocus: '#page-title' });
    };
    // ---- 動作 ----
    const head = [];
    if (actions.has('ack')) head.push(button({ text: t('alerts.ack.title'), variant: 'primary', attrs: { id: 'ev-ack' }, onClick: async () => after(await ackDialog(e)) }));
    if (actions.has('assign')) head.push(button({ text: t('alerts.assign.menu'), onClick: async () => after(await assignDialog([e])) }));
    if (actions.has('snooze')) head.push(button({ text: t('alerts.snooze.button'), iconName: 'pause', onClick: async () => after(await snooze(e, 1)) }));
    if (actions.has('resolve')) {
      head.push(
        button({
          text: t('alerts.resolve.menu'),
          onClick: async () => {
            const still = ((e.children && e.children.items) || []).filter((c) => c.state === 'firing' || c.state === 'acknowledged').length;
            after(await resolveDialog([e], { unrecoveredChildren: still }));
          },
        }),
      );
    }
    if (actions.has('silence') && session.can('silences:write')) {
      head.push(h('a', { class: 'ds-button ds-button--secondary', href: `/alerts/silences/new${qs({ type: 'silence', event: e.event_id })}` }, icon('bell-off'), t('alerts.silence.create')));
    }

    // ---- 狀態列 ----
    const statusBits = [
      sevBadge(e.severity),
      attentionTag(e),
      h('span', null, stateText(e)),
      h('span', { class: 'secondary' }, t(resolved || e.state === 'recovered' ? 'alerts.ev.startedTotal' : 'alerts.ev.startedFor', { time: `${fmt.dateTime(e.started_at)} ${fmt.tzLabel()}`, d: fmt.duration(durationSecs(e)) })),
      e.rule ? h('span', { class: 'secondary' }, t('alerts.ev.rule'), session.can('alert-rules:read') ? h('a', { href: `/alerts/rules/${encodeURIComponent(e.rule.rule_id)}` }, e.rule.name) : e.rule.name) : null,
      h(
        'span',
        { class: 'secondary', id: 'ev-attn' },
        e.attention_text ? e.attention_text : e.assignee ? t('alerts.ev.assignee', { who: who(e.assignee) }) : e.acked ? t('alerts.state.acknowledged') : t('alerts.ev.unacked'),
      ),
      e.code ? codeTag(e.code) : null,
      flags(e),
    ];
    if (e.severity === 'emergency' && e.state === 'firing' && !e.acked) {
      statusBits.push(h('span', { class: 'k-fg--emergency strong' }, e.notifications && e.notifications.escalated ? t('alerts.ev.escalated') : t('alerts.ev.willEscalate')));
    }
    if (e.severity === 'emergency' && e.state === 'recovered' && !e.acked) statusBits.push(h('span', { class: 'strong' }, t('alerts.ev.recoveredNeedsAck')));
    if (e.snooze_until && Date.parse(e.snooze_until) > fmt.serverNow()) statusBits.push(h('span', { class: 'secondary' }, t('alerts.ev.snoozed', { time: fmt.time(e.snooze_until, { seconds: false }) })));

    // ---- 提示 ----
    const notices = [];
    if (e.parent) {
      notices.push(inlineAlert({ kind: 'warning', title: e.parent.banner, body: h('p', null, h('a', { href: `/alerts/events/${encodeURIComponent(e.parent.event_id)}` }, t('alerts.ev.toParent', { id: e.parent.event_id }))) }));
    }
    if (!resolved && e.acked && e.acked_by && who(e.acked_by) !== me) {
      notices.push(inlineAlert({ kind: 'info', title: t('alerts.ev.ackedBy', { who: who(e.acked_by), time: fmt.time(e.acked_at, { seconds: false }) }) }));
    }
    if (resolved) {
      const mtta = e.acked_at ? (Date.parse(e.acked_at) - Date.parse(e.started_at)) / 1000 : null;
      const mttr = (Date.parse(e.resolved_at) - Date.parse(e.started_at)) / 1000;
      notices.push(
        inlineAlert({
          kind: 'success',
          title: t('alerts.ev.resolved'),
          body: h(
            'p',
            null,
            t('alerts.ev.resolvedBody', { cause: e.root_cause_label || rootCauseLabel(e.root_cause) || '—', who: who(e.resolved_by) || '—', total: fmt.duration(mttr) }),
            mtta != null ? ` ${t('alerts.ev.mtta', { d: fmt.duration(mtta) })}` : '',
            e.resolve_note ? h('span', { class: 'k-sub' }, t('alerts.ev.note', { text: e.resolve_note })) : null,
          ),
        }),
      );
    }

    // ---- 左欄 ----
    const left = [interpretation(e), childrenCard(e)];
    if (e.rule && e.rule.description) left.push(card({ title: t('alerts.ev.ruleNote'), body: h('p', { class: 'k-pre' }, e.rule.description) }));
    const props = dl([
      [t('alerts.ev.id'), h('span', { class: 'mono' }, e.event_id)],
      [t('alerts.ev.kind'), kindLabel(e.kind)],
      [t('alerts.events.category'), t(`alerts.category.${e.category}`)],
      e.subject ? [t('alerts.col.subject'), h('span', { class: 'mono' }, e.subject)] : null,
      [t('alerts.ev.occurrences'), t('alerts.ev.occurrencesValue', { n: fmt.num(e.occurrences || 1), r: fmt.num(e.reopen_count || 0) })],
      e.recovered_at ? [t('alerts.ev.recoveredAt'), fmt.timeEl(e.recovered_at, { mode: 'absolute' })] : null,
      e.eta_at ? [t('alerts.ev.eta'), fmt.timeEl(e.eta_at, { mode: 'absolute' })] : null,
      [t('alerts.ev.notified'), e.notifications ? t('alerts.ev.notifiedValue', { n: fmt.num(e.notifications.sent || 0) }) : '—'],
    ]);
    left.push(card({ title: t('alerts.ev.properties'), body: props }));

    // ---- 右欄：處理紀錄＋註解、通知發送狀態 ----
    const tl = e.timeline || [];
    const tlList = h(
      'ol',
      { class: 'k-timeline', id: 'ev-timeline' },
      tl.map((it) =>
        h(
          'li',
          null,
          h('time', { datetime: it.at, title: `${fmt.dateTime(it.at)} ${fmt.tzLabel()}` }, fmt.monthDayTime(it.at)),
          h(
            'span',
            null,
            h('strong', null, who(it.actor)),
            ' ',
            timelineText(it),
            it.action === 'commented' && it.details && it.details.text ? h('span', { class: 'k-sub k-pre' }, it.details.text) : null,
            it.action === 'resolved' && it.details && it.details.note ? h('span', { class: 'k-sub k-pre' }, it.details.note) : null,
          ),
        ),
      ),
    );
    let form = null;
    if (canWrite && (e.actions || []).includes('comment')) {
      const ta = h('textarea', { class: 'ds-textarea', name: 'comment', maxlength: 2000 });
      const f = field({ label: t('alerts.comment.label'), input: ta, hint: resolved ? t('alerts.comment.hintResolved') : null });
      const submit = button({ text: t('alerts.comment.submit'), type: 'submit' });
      const errBox = h('div');
      form = h('form', { class: 'k-comment-form', novalidate: true }, f, errBox, submit);
      form.addEventListener('submit', async (ev) => {
        ev.preventDefault();
        const text = ta.value.trim();
        if (!text) {
          f.setError(t('alerts.comment.required'));
          ta.focus();
          return;
        }
        f.setError('');
        setBusy(submit, true, t('common.working'));
        try {
          await api(`/api/v1/alerts/${encodeURIComponent(e.event_id)}/comments`, { method: 'POST', body: { text } });
          toast({ kind: 'success', title: t('alerts.comment.done') });
          await load({ keepFocus: '#ev-timeline li:last-child time' });
        } catch (err) {
          setBusy(submit, false, true);
          mount(errBox, errorAlert(err, { title: t('alerts.comment.failed') }));
        }
      });
    }
    const right = [
      card({ title: t('alerts.ev.timeline'), body: [tl.length ? tlList : h('p', { class: 'secondary' }, t('alerts.ev.noTimeline')), form] }),
      deliveriesCard(e),
    ];

    mount(
      root,
      pageHead({
        title: e.title,
        crumbs: [{ text: t('alerts.events.title'), href: '/alerts/events' }, { text: e.event_id }],
        actions: head.length ? h('div', { class: 'k-alert-actions' }, head) : null,
      }),
      h('div', { class: 'k-row k-event-status' }, statusBits),
      notices,
      h('div', { class: 'k-grid k-grid--side' }, h('div', { class: 'k-stack' }, left), h('div', { class: 'k-stack' }, right)),
    );
    // 換頁時的焦點由 main.js 移到標題；重新載入後由呼叫端決定
  }

  // 即時更新：這個事件（或它的子事件）有變化時重新抓取（最多每 2 秒一次）
  const unsub = sse.subscribe((ev) => {
    if (!ev.type.startsWith('alert.') || !ev.data) return;
    if (ev.data.event_id !== id && ev.data.parent_id !== id) return;
    if (pending) return;
    pending = setTimeout(() => {
      pending = null;
      // 使用者正在輸入註解時不重畫（避免清掉輸入）
      const ta = root.querySelector('textarea[name="comment"]');
      if (ta && ta.value.trim()) return;
      const active = document.activeElement && document.activeElement.id ? `#${document.activeElement.id}` : null;
      load({ keepFocus: active });
    }, 2000);
  });

  load();
  return {
    dispose() {
      disposed = true;
      unsub();
      if (pending) clearTimeout(pending);
    },
  };
}

