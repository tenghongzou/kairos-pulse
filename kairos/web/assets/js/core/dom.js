// DOM 建構的唯一入口：所有文字一律以 Text 節點（textContent）插入，不使用 innerHTML。
// 主機名稱、標籤、錯誤訊息等外部字串因此不會被當成 HTML 解析（XSS；CSP 另以 Trusted Types 擋下 innerHTML）。
//
//   h('a', { href: '/hosts', class: 'ds-button', on: { click: fn } }, '主機', icon('server'))
//
// props：
//   class        字串或陣列（假值略過）
//   text         等同一個文字子節點
//   on           { 事件名稱: 函式 }
//   dataset      { key: value } → data-key
//   value／checked／disabled／hidden／indeterminate／selected  以屬性（property）設定
//   其他         setAttribute（true → 空字串；false／null／undefined 不設定）

const SVG_NS = 'http://www.w3.org/2000/svg';
const PROPS = new Set(['value', 'checked', 'disabled', 'hidden', 'indeterminate', 'selected']);

function append(el, child) {
  if (child == null || child === false || child === true) return;
  if (Array.isArray(child)) {
    for (const c of child) append(el, c);
    return;
  }
  if (child instanceof Node) {
    el.appendChild(child);
    return;
  }
  el.appendChild(document.createTextNode(String(child)));
}

function applyProps(el, props) {
  if (!props) return;
  for (const [k, v] of Object.entries(props)) {
    if (v == null || v === false) {
      if (PROPS.has(k) && k !== 'value') el[k] = false;
      continue;
    }
    if (k === 'class') {
      const cls = Array.isArray(v) ? v.filter(Boolean).join(' ') : v;
      if (cls) el.setAttribute('class', cls);
    } else if (k === 'text') {
      append(el, v);
    } else if (k === 'on') {
      for (const [ev, fn] of Object.entries(v)) if (fn) el.addEventListener(ev, fn);
    } else if (k === 'dataset') {
      for (const [dk, dv] of Object.entries(v)) if (dv != null) el.dataset[dk] = String(dv);
    } else if (PROPS.has(k)) {
      el[k] = v;
    } else if (k === 'style') {
      // style 屬性會被 CSP 擋下：只接受物件，以 CSSOM 設定
      for (const [sk, sv] of Object.entries(v)) el.style.setProperty(sk, sv);
    } else {
      el.setAttribute(k, v === true ? '' : String(v));
    }
  }
}

/** 建立 HTML 元素 */
export function h(tag, props, ...children) {
  const el = document.createElement(tag);
  if (props && (props instanceof Node || typeof props !== 'object' || Array.isArray(props))) {
    append(el, props);
  } else {
    applyProps(el, props);
  }
  append(el, children);
  return el;
}

/** 建立 SVG 元素（圖表、QR code） */
export function s(tag, props, ...children) {
  const el = document.createElementNS(SVG_NS, tag);
  if (props) {
    for (const [k, v] of Object.entries(props)) {
      if (v == null || v === false) continue;
      if (k === 'on') {
        for (const [ev, fn] of Object.entries(v)) el.addEventListener(ev, fn);
      } else if (k === 'text') {
        append(el, v);
      } else {
        el.setAttribute(k, String(v));
      }
    }
  }
  append(el, children);
  return el;
}

/** 設計系統的圖示（index.html 內的 sprite，`ds-icon-<name>`） */
export function icon(name, cls) {
  const svg = s('svg', { class: cls ? `ds-icon ${cls}` : 'ds-icon', 'aria-hidden': 'true', focusable: 'false' });
  svg.appendChild(s('use', { href: `#ds-icon-${name}` }));
  return svg;
}

/** 清空元素（不使用 innerHTML） */
export function clear(el) {
  if (el) el.replaceChildren();
  return el;
}

/** 以新的子節點取代內容 */
export function mount(el, ...children) {
  clear(el);
  append(el, children);
  return el;
}

export function $(sel, root = document) {
  return root.querySelector(sel);
}

export function $$(sel, root = document) {
  return Array.from(root.querySelectorAll(sel));
}

let uidN = 0;
/** 頁面內唯一的 id（aria-labelledby、label for） */
export function uid(prefix = 'k') {
  uidN += 1;
  return `${prefix}-${uidN}`;
}

/** 螢幕報讀宣告（polite） */
export function announce(text) {
  const region = document.getElementById('sr-polite');
  if (!region) return;
  region.textContent = '';
  // 讓同一句話重複時也會被宣告
  setTimeout(() => {
    region.textContent = text;
  }, 50);
}

/** 正在輸入（快捷鍵不應觸發） */
export function isTyping(target) {
  return (
    target &&
    (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.tagName === 'SELECT' || target.isContentEditable)
  );
}
