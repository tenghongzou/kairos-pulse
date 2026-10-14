// DataTable（design-system/README.md §6.3）：伺服器端篩選、排序、游標分頁。
//
// 0.7.0 的取捨（kairos/web/README.md「表格」）：以原生 <table>＋「載入更多」取代虛擬捲動與 role=grid：
// - 設計系統允許退回分段載入（§6.3「若評估虛擬捲動成本過高…」）；API 只有游標（沒有頁碼），所以是
//   「載入更多（還有 N 台）」，每次 100 筆。
// - 原生表格的語意讓螢幕報讀器可以逐欄朗讀；鍵盤以 Tab 走過欄位標頭的排序按鈕與每列的主機名稱連結
//   （整個表格不是單一 Tab 停駐點，但不需要記 grid 的快捷鍵）。
// - 排序只依一欄（伺服器端），標頭以 aria-sort 標示。

import { h, icon, announce, mount } from '../core/dom.js';
import { t } from '../core/strings.js';
import { errorState } from './components.js';

/**
 * @param {object} o
 *   caption       表格的可存取名稱（視覺隱藏）
 *   columns       [{ key, label, sort（API 的排序鍵）, num, cls, render(row) → 節點或字串 }]
 *   rowKey(row)   列的識別
 *   rowHref(row)  整列點擊時前往的網址（列內的連結、按鈕不受影響）
 *   sort          { key, desc }
 *   onSort(key, desc)
 *   onLoadMore()
 *   unit          計數單位（「台」）
 */
export function createTable(o) {
  const tbody = h('tbody');
  const headRow = h('tr');
  const statusEl = h('p', { class: 'k-table-foot__status', role: 'status' });
  const moreBtn = h(
    'button',
    { type: 'button', class: 'ds-button ds-button--secondary ds-button--sm', hidden: true, on: { click: () => o.onLoadMore && o.onLoadMore() } },
    t('table.loadMore'),
  );
  const table = h(
    'table',
    { class: 'ds-table ds-table--two-line' },
    h('caption', { class: 'ds-visually-hidden' }, o.caption),
    h('thead', null, headRow),
    tbody,
  );
  const wrap = h('div', { class: 'ds-table-wrap' }, table);
  const foot = h('div', { class: 'k-table-foot' }, statusEl, moreBtn);
  const body = h('div', { class: 'k-table-region' });
  const el = h('div', { class: 'k-table-card' }, body, foot);
  let sort = o.sort || null;
  let count = 0;
  const rowsByKey = new Map();

  function renderHead() {
    mount(headRow, 
      ...o.columns.map((c) => {
        if (!c.sort) return h('th', { scope: 'col', class: c.num ? 'num' : null }, c.label);
        const active = sort && sort.key === c.sort;
        const ariaSort = active ? (sort.desc ? 'descending' : 'ascending') : 'none';
        const iconName = active ? (sort.desc ? 'sort-desc' : 'sort-asc') : 'sort-none';
        return h(
          'th',
          { scope: 'col', 'aria-sort': ariaSort, class: c.num ? 'num' : null },
          h(
            'button',
            {
              type: 'button',
              class: 'vt__sort',
              on: {
                click: () => {
                  const desc = active ? !sort.desc : Boolean(c.defaultDesc);
                  sort = { key: c.sort, desc };
                  renderHead();
                  announce(t('table.sortedBy', { col: c.label, dir: desc ? t('table.desc') : t('table.asc') }));
                  if (o.onSort) o.onSort(c.sort, desc);
                },
              },
            },
            c.label,
            icon(iconName),
          ),
        );
      }),
    );
  }

  function renderRow(row) {
    const href = o.rowHref ? o.rowHref(row) : null;
    const tr = h(
      'tr',
      { class: href ? 'is-clickable' : null, dataset: { key: o.rowKey(row) } },
      o.columns.map((c) => h('td', { class: [c.num ? 'num' : null, c.cls] }, c.render ? c.render(row) : row[c.key] ?? '—')),
    );
    if (href) {
      tr.addEventListener('click', (ev) => {
        if (ev.target.closest('a, button, input, select, label')) return;
        // 選取文字時不換頁
        if (window.getSelection && String(window.getSelection())) return;
        const a = tr.querySelector('a[href]');
        if (a) a.click();
      });
    }
    return tr;
  }

  function showTable() {
    if (body.firstChild !== wrap) mount(body, wrap);
  }

  renderHead();

  return {
    el,
    setSort(s) {
      sort = s;
      renderHead();
    },
    /** 首次載入：10 列骨架（§6.3 表格狀態） */
    setLoading() {
      showTable();
      rowsByKey.clear();
      count = 0;
      mount(tbody, 
        ...Array.from({ length: 10 }, () =>
          h(
            'tr',
            { 'aria-hidden': 'true' },
            o.columns.map(() => h('td', null, h('span', { class: 'ds-skeleton k-skel-cell' }))),
          ),
        ),
      );
      table.setAttribute('aria-busy', 'true');
      statusEl.textContent = t('common.loading');
      moreBtn.hidden = true;
    },
    /** 取代所有列 */
    setRows(rows, { total, hasMore }) {
      showTable();
      table.removeAttribute('aria-busy');
      rowsByKey.clear();
      mount(tbody, 
        ...rows.map((r) => {
          const tr = renderRow(r);
          rowsByKey.set(o.rowKey(r), tr);
          return tr;
        }),
      );
      count = rows.length;
      this.setFooter({ total, hasMore });
    },
    /** 加上下一段 */
    appendRows(rows, { total, hasMore }) {
      for (const r of rows) {
        const k = o.rowKey(r);
        if (rowsByKey.has(k)) continue;
        const tr = renderRow(r);
        rowsByKey.set(k, tr);
        tbody.appendChild(tr);
        count += 1;
      }
      this.setFooter({ total, hasMore });
    },
    setFooter({ total, hasMore }) {
      statusEl.textContent = t('table.showing', { n: count.toLocaleString('en-US'), total: (total ?? count).toLocaleString('en-US'), unit: o.unit || t('table.rows') });
      moreBtn.hidden = !hasMore;
      moreBtn.disabled = false;
      moreBtn.removeAttribute('aria-busy');
      moreBtn.textContent = hasMore ? t('table.loadMoreRest', { n: Math.max(0, (total ?? 0) - count).toLocaleString('en-US') }) : t('table.loadMore');
    },
    /** 載入下一段中 */
    setLoadingMore() {
      moreBtn.disabled = true;
      moreBtn.setAttribute('aria-busy', 'true');
      moreBtn.textContent = t('table.loadingMore');
    },
    /** 只更新一列（即時更新；不重排） */
    updateRow(row) {
      const k = o.rowKey(row);
      const old = rowsByKey.get(k);
      if (!old) return false;
      const tr = renderRow(row);
      old.replaceWith(tr);
      rowsByKey.set(k, tr);
      return true;
    },
    hasRow(key) {
      return rowsByKey.has(key);
    },
    /** 空結果或錯誤：表格區域換成 EmptyState／ErrorState */
    setEmpty(node) {
      table.removeAttribute('aria-busy');
      mount(body, node);
      statusEl.textContent = '';
      moreBtn.hidden = true;
      count = 0;
      rowsByKey.clear();
    },
    setError(err, onRetry, title) {
      if (count > 0) {
        // 已載入資料時只在底部顯示行內錯誤
        statusEl.textContent = t('table.loadMoreFailed');
        moreBtn.hidden = false;
        moreBtn.disabled = false;
        moreBtn.removeAttribute('aria-busy');
        moreBtn.textContent = t('common.retry');
        return;
      }
      this.setEmpty(errorState(err, { title, onRetry }));
    },
    get count() {
      return count;
    },
  };
}
