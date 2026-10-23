// DOM-walk accessibility snapshot for backends without a CDP AX tree.
// Function body. Expects in scope:
//   __abOpts = {key, selector, urls, resolve?: {id, role, name}}
//   __abCursorScan (the shared cursor-interactive scan)
// Snapshot mode returns {document, nodes, cursor, hrefs}, with `nodes` shaped
// like CDP Accessibility.getFullAXTree output so the Rust side reuses the CDP
// tree builder and renderer. Resolve mode returns the element for a ref, null
// when it is gone, or a string describing why it no longer matches.
var opts = __abOpts;
var W = window;
if (!opts || typeof opts.key !== 'string' || !opts.key) throw new Error('agent-browser DOM snapshot: missing registry key');

// Per-document element registry. It lives under a key the daemon picks (the
// page cannot know it in advance) as a non-enumerable, read-only,
// non-configurable property with frozen accessors, and the page DOM is never
// mutated. Resolution still re-checks role and name, so a page that patches
// built-ins cannot silently redirect a ref to another element.
var ID_SPAN = 4194304; // 2^22 ids per document salt; salt < 2^30 keeps ids below 2^53.
var registryDescriptor = Object.getOwnPropertyDescriptor(W, opts.key);
var registry = registryDescriptor && registryDescriptor.value;
if (!registry) {
  registry = (function () {
    var byElement = new WeakMap();
    var byId = new Map();
    var salt = 1 + Math.floor(Math.random() * 1073741823);
    var next = 1;
    var weak = typeof WeakRef === 'function';
    return Object.freeze({
      document: String(performance.timeOrigin) + ':' + salt,
      idOf: function (el) {
        var id = byElement.get(el);
        if (id === undefined) {
          id = salt * ID_SPAN + next++;
          byElement.set(el, id);
          byId.set(id, weak ? new WeakRef(el) : { deref: function () { return el; } });
        }
        return id;
      },
      get: function (id) {
        var ref = byId.get(id);
        return ref ? ref.deref() : undefined;
      }
    });
  })();
  Object.defineProperty(W, opts.key, { value: registry, enumerable: false, writable: false, configurable: false });
}
var idOf = registry.idOf;

// Per-call caches: every element's style and text alternative is computed
// once, so name-from-content over nested tables stays linear in page size.
var styleCache = new WeakMap();
var pseudoCache = new WeakMap();
var textCache = [new WeakMap(), new WeakMap()];

var SKIP = { SCRIPT: 1, STYLE: 1, NOSCRIPT: 1, TEMPLATE: 1, HEAD: 1, META: 1, LINK: 1, TITLE: 1, BR: 1, WBR: 1 };
var SPACE_BREAKS = { BR: 1, WBR: 1 };
var TRANSPARENT = { BODY: 1, B: 1, I: 1, U: 1, S: 1, SMALL: 1, BIG: 1, FONT: 1, CENTER: 1, TT: 1, MARK: 1, BDI: 1, BDO: 1, Q: 1, CITE: 1, DFN: 1, VAR: 1, KBD: 1, SAMP: 1, PICTURE: 1, SOURCE: 1, TBODY: 1, THEAD: 1, TFOOT: 1, COLGROUP: 1, COL: 1, SLOT: 1 };
var LANDMARK_SCOPES = { ARTICLE: 1, ASIDE: 1, MAIN: 1, NAV: 1, SECTION: 1 };
var NAME_FROM_CONTENT = { LayoutTableCell: 1, button: 1, link: 1, heading: 1, cell: 1, gridcell: 1, columnheader: 1, rowheader: 1, checkbox: 1, radio: 1, option: 1, menuitem: 1, menuitemcheckbox: 1, menuitemradio: 1, tab: 1, treeitem: 1, switch: 1, tooltip: 1, sectionheader: 1, DisclosureTriangle: 1, LabelText: 1, legend: 1, Figcaption: 1 };
var ARIA_ROLES = { alert: 1, alertdialog: 1, application: 1, article: 1, banner: 1, blockquote: 1, button: 1, caption: 1, cell: 1, checkbox: 1, code: 1, columnheader: 1, combobox: 1, complementary: 1, contentinfo: 1, definition: 1, deletion: 1, dialog: 1, document: 1, emphasis: 1, feed: 1, figure: 1, form: 1, generic: 1, grid: 1, gridcell: 1, group: 1, heading: 1, img: 1, image: 1, insertion: 1, link: 1, list: 1, listbox: 1, listitem: 1, log: 1, main: 1, marquee: 1, math: 1, menu: 1, menubar: 1, menuitem: 1, menuitemcheckbox: 1, menuitemradio: 1, meter: 1, navigation: 1, note: 1, option: 1, paragraph: 1, progressbar: 1, radio: 1, radiogroup: 1, region: 1, row: 1, rowgroup: 1, rowheader: 1, scrollbar: 1, search: 1, searchbox: 1, separator: 1, slider: 1, spinbutton: 1, status: 1, strong: 1, subscript: 1, superscript: 1, switch: 1, tab: 1, table: 1, tablist: 1, tabpanel: 1, term: 1, textbox: 1, time: 1, timer: 1, toolbar: 1, tooltip: 1, tree: 1, treegrid: 1, treeitem: 1 };
var TAG_ROLES = { BUTTON: 'button', TEXTAREA: 'textbox', OPTION: 'option', OPTGROUP: 'group', UL: 'list', OL: 'list', MENU: 'list', LI: 'listitem', NAV: 'navigation', MAIN: 'main', ASIDE: 'complementary', ARTICLE: 'article', TABLE: 'table', TR: 'row', P: 'paragraph', BLOCKQUOTE: 'blockquote', FIGURE: 'figure', FIGCAPTION: 'Figcaption', HR: 'separator', DIALOG: 'dialog', DETAILS: 'group', SUMMARY: 'DisclosureTriangle', LABEL: 'LabelText', IFRAME: 'Iframe', SUP: 'superscript', SUB: 'subscript', ABBR: 'Abbr', CODE: 'code', STRONG: 'strong', EM: 'emphasis', FIELDSET: 'group', LEGEND: 'legend', PROGRESS: 'progressbar', METER: 'meter', OUTPUT: 'status', SEARCH: 'search', FORM: 'form', CAPTION: 'caption', DL: 'list', DT: 'term', DD: 'definition', TIME: 'time', DEL: 'deletion', INS: 'insertion' };
var INPUT_ROLES = { button: 'button', submit: 'button', reset: 'button', image: 'button', checkbox: 'checkbox', radio: 'radio', range: 'slider', number: 'spinbutton', search: 'searchbox', color: 'button', file: 'button' };

function styleOf(el) {
  var style = styleCache.get(el);
  if (!style) { style = W.getComputedStyle(el); styleCache.set(el, style); }
  return style;
}
function isHidden(el) {
  if (el.hidden || el.getAttribute('aria-hidden') === 'true') return true;
  if (el.tagName === 'INPUT' && el.type === 'hidden') return true;
  return styleOf(el).display === 'none';
}
function collapse(text) { return (text || '').replace(/[\s ]+/g, ' ').trim(); }
function inLandmarkScope(el) {
  for (var p = el.parentElement; p; p = p.parentElement) if (LANDMARK_SCOPES[p.tagName]) return true;
  return false;
}
function hasExplicitName(el) { return !!(el.getAttribute('aria-label') || el.getAttribute('aria-labelledby') || el.getAttribute('title')); }

function implicitRole(el) {
  var tag = el.tagName;
  if (tag === 'A' || tag === 'AREA') return el.hasAttribute('href') ? 'link' : 'generic';
  if (tag === 'INPUT') {
    var t = (el.getAttribute('type') || 'text').toLowerCase();
    if (INPUT_ROLES[t]) return t === 'search' && el.hasAttribute('list') ? 'combobox' : INPUT_ROLES[t];
    return el.hasAttribute('list') ? 'combobox' : 'textbox';
  }
  if (tag === 'SELECT') return el.multiple || el.size > 1 ? 'listbox' : 'combobox';
  if (/^H[1-6]$/.test(tag)) return 'heading';
  if (tag === 'IMG') return el.getAttribute('alt') === '' ? null : 'image';
  if (tag === 'HEADER') return inLandmarkScope(el) ? 'generic' : 'banner';
  if (tag === 'FOOTER') return inLandmarkScope(el) ? 'generic' : 'contentinfo';
  if (tag === 'SECTION') return hasExplicitName(el) ? 'region' : 'generic';
  if (tag === 'TABLE') return isDataTable(el) ? 'table' : 'LayoutTable';
  if (tag === 'TR' || tag === 'TD' || tag === 'TH') {
    var table = el.closest('table');
    if (table && !isDataTable(table)) return tag === 'TR' ? 'LayoutTableRow' : 'LayoutTableCell';
  }
  if (tag === 'TD') return 'cell';
  if (tag === 'TH') {
    var scope = (el.getAttribute('scope') || '').toLowerCase();
    if (scope === 'row' || scope === 'rowgroup') return 'rowheader';
    if (scope === 'col' || scope === 'colgroup') return 'columnheader';
    var row = el.parentElement;
    if (row && row.querySelector(':scope > td')) return 'rowheader';
    return 'columnheader';
  }
  if (TAG_ROLES[tag]) return TAG_ROLES[tag];
  if (TRANSPARENT[tag]) return null;
  return 'generic';
}
// Chrome's data-vs-layout table heuristic, simplified: layout tables expose
// LayoutTable roles whose cells take no refs.
var dataTableCache = new WeakMap();
function isDataTable(table) {
  var cached = dataTableCache.get(table);
  if (cached !== undefined) return cached;
  var result = (function () {
    var tableRole = (table.getAttribute('role') || '').trim().toLowerCase();
    if (tableRole === 'presentation' || tableRole === 'none') return false;
    if (tableRole) return true;
    if (table.caption || table.tHead || table.tFoot || table.querySelector(':scope > colgroup, :scope > col')) return true;
    if (table.getAttribute('summary')) return true;
    var headerCells = Array.prototype.some.call(table.rows, function (row) {
      return !!row.querySelector(':scope > th, :scope > td[headers], :scope > td[scope], :scope > td[abbr]');
    });
    if (headerCells) return true;
    if (table.querySelector('table')) return false;
    var border = table.getAttribute('border');
    if (border && border !== '0') return true;
    return table.rows.length >= 20;
  })();
  dataTableCache.set(table, result);
  return result;
}
function pseudoText(el, which) {
  var cached = pseudoCache.get(el);
  if (!cached) { cached = {}; pseudoCache.set(el, cached); }
  if (!(which in cached)) cached[which] = computePseudoText(el, which);
  return cached[which];
}
function computePseudoText(el, which) {
  var content = W.getComputedStyle(el, which).content;
  if (!content || content === 'none' || content === 'normal') return '';
  // `content: "x" "y" / "alt"` exposes the alt text when present.
  var tokens = content.match(/"(?:[^"\\]|\\.)*"|\//g) || [];
  var slash = tokens.indexOf('/');
  var parts = slash >= 0 ? tokens.slice(slash + 1) : tokens;
  return parts.filter(function (t) { return t !== '/'; }).map(function (t) {
    return t.slice(1, -1).replace(/\\(.)/g, '$1');
  }).join('');
}
function roleOf(el) {
  var explicit = (el.getAttribute('role') || '').trim().toLowerCase().split(/\s+/)[0];
  if (explicit === 'none' || explicit === 'presentation') return el.tabIndex >= 0 && el.hasAttribute('tabindex') ? implicitRole(el) : null;
  if (explicit === 'img') return 'image';
  if (ARIA_ROLES[explicit]) return explicit;
  return implicitRole(el);
}

function idrefText(el, attr) {
  var ids = (el.getAttribute(attr) || '').trim();
  if (!ids) return '';
  var root = el.getRootNode ? el.getRootNode() : document;
  return collapse(ids.split(/\s+/).map(function (id) {
    var target = (root.getElementById ? root.getElementById(id) : null) || document.getElementById(id);
    return target ? textAlternative(target, true) : '';
  }).join(' '));
}
function controlValue(el) {
  if (el.tagName === 'SELECT') {
    var opt = el.selectedOptions && el.selectedOptions[0];
    return opt ? collapse(opt.textContent) : '';
  }
  return el.value || '';
}
// Text of `node`'s subtree for name-from-content, approximating accname step 2F.
function contentText(node, inLabel) {
  var out = '';
  for (var c = node.firstChild; c; c = c.nextSibling) {
    if (c.nodeType === 3) { out += c.data; continue; }
    if (c.nodeType !== 1 || SKIP[c.tagName.toUpperCase()]) { if (c.nodeType === 1 && SPACE_BREAKS[c.tagName]) out += ' '; continue; }
    if (!inLabel && isHidden(c)) continue;
    var block = styleOf(c).display.indexOf('inline') !== 0;
    var piece = pseudoText(c, '::before') + textAlternative(c, inLabel) + pseudoText(c, '::after');
    out += block ? ' ' + piece + ' ' : piece;
  }
  if (node.shadowRoot) out += ' ' + contentText(node.shadowRoot, inLabel);
  return out;
}
function textAlternative(el, inLabel) {
  var cache = textCache[inLabel ? 1 : 0];
  var text = cache.get(el);
  if (text === undefined) { text = computeTextAlternative(el, inLabel); cache.set(el, text); }
  return text;
}
function computeTextAlternative(el, inLabel) {
  var aria = collapse(el.getAttribute('aria-label'));
  if (aria) return aria;
  var tag = el.tagName;
  if (tag === 'IMG' || tag === 'AREA') return collapse(el.getAttribute('alt'));
  if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return inLabel ? collapse(controlValue(el)) : '';
  return contentText(el, inLabel);
}
function labelText(el) {
  var labels = el.labels;
  if (!labels || !labels.length) return '';
  return collapse(Array.prototype.map.call(labels, function (label) {
    var text = '';
    for (var c = label.firstChild; c; c = c.nextSibling) {
      if (c === el) continue;
      if (c.nodeType === 3) text += c.data;
      else if (c.nodeType === 1 && !SKIP[c.tagName.toUpperCase()] && !isHidden(c)) text += ' ' + textAlternative(c, true) + ' ';
    }
    return text;
  }).join(' '));
}
function nameOf(el, role) {
  var name = idrefText(el, 'aria-labelledby');
  if (name) return name;
  name = collapse(el.getAttribute('aria-label'));
  if (name) return name;
  var tag = el.tagName;
  if (tag === 'INPUT') {
    var t = (el.type || '').toLowerCase();
    if (t === 'submit' || t === 'reset' || t === 'button') return collapse(el.value) || (t === 'submit' ? 'Submit' : t === 'reset' ? 'Reset' : '');
    if (t === 'image') return collapse(el.alt) || collapse(el.value) || 'Submit';
  }
  if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') {
    return labelText(el) || collapse(el.getAttribute('title')) || collapse(el.getAttribute('placeholder'));
  }
  if (tag === 'IMG' || tag === 'AREA') return collapse(el.getAttribute('alt')) || collapse(el.getAttribute('title'));
  if (tag === 'FIELDSET') { var lg = el.querySelector(':scope > legend'); if (lg) return collapse(contentText(lg)); }
  if (tag === 'FIGURE') { var fc = el.querySelector(':scope > figcaption'); if (fc) return collapse(contentText(fc)); }
  if (tag === 'TABLE') { var cap = el.querySelector(':scope > caption'); if (cap) return collapse(contentText(cap)); }
  if (el.namespaceURI === 'http://www.w3.org/2000/svg') { var st = el.querySelector(':scope > title'); return st ? collapse(st.textContent) : ''; }
  if (NAME_FROM_CONTENT[role]) { name = collapse(pseudoText(el, '::before') + contentText(el) + pseudoText(el, '::after')); if (name) return name; }
  return collapse(el.getAttribute('title'));
}

function prop(list, name, type, value) { list.push({ name: name, value: { type: type, value: value } }); }
function propertiesOf(el, role) {
  var props = [];
  var level = el.getAttribute('aria-level');
  if (role === 'heading') prop(props, 'level', 'integer', level ? parseInt(level, 10) : (/^H[1-6]$/.test(el.tagName) ? +el.tagName[1] : 2));
  if (role === 'checkbox' || role === 'radio' || role === 'switch' || role === 'menuitemcheckbox' || role === 'menuitemradio') {
    var ac = el.getAttribute('aria-checked');
    var checked = el.tagName === 'INPUT' ? (el.indeterminate ? 'mixed' : String(el.checked)) : (ac === 'mixed' ? 'mixed' : String(ac === 'true'));
    prop(props, 'checked', 'tristate', checked);
  }
  var expanded = el.getAttribute('aria-expanded');
  if (expanded === 'true' || expanded === 'false') prop(props, 'expanded', 'booleanOrUndefined', expanded === 'true');
  else if (el.tagName === 'SUMMARY' && el.parentElement && el.parentElement.tagName === 'DETAILS') prop(props, 'expanded', 'booleanOrUndefined', el.parentElement.open);
  else if (role === 'combobox' && el.tagName === 'SELECT') prop(props, 'expanded', 'booleanOrUndefined', false);
  var selected = el.getAttribute('aria-selected');
  if (selected === 'true' || (el.tagName === 'OPTION' && el.selected)) prop(props, 'selected', 'booleanOrUndefined', true);
  if (el.disabled === true || el.getAttribute('aria-disabled') === 'true') prop(props, 'disabled', 'boolean', true);
  if (el.required === true || el.getAttribute('aria-required') === 'true') prop(props, 'required', 'boolean', true);
  return props;
}
function valueOf(el, role) {
  if (role === 'textbox' || role === 'searchbox' || role === 'combobox' || role === 'slider' || role === 'spinbutton') {
    if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT') {
      if (el.type === 'password') return '';
      return controlValue(el);
    }
  }
  return '';
}

// Resolve mode: the element behind a ref, verified against its snapshot role
// and name. Hidden radio/checkbox promotion (LabelText/generic wrapping a
// hidden input) and cursor-interactive text names are accepted as matches.
function cursorText(el) { return (el.textContent || '').trim().slice(0, 100); }
function resolveRef(want) {
  var el = registry.get(want.id);
  if (!el || !el.isConnected) return null;
  var role = roleOf(el);
  var promoted = (role === 'LabelText' || role === 'generic') && (want.role === 'radio' || want.role === 'checkbox');
  if (role !== want.role && !promoted) {
    return 'its element is now role=' + role + ', expected role=' + want.role;
  }
  var name = nameOf(el, role);
  if (name !== want.name && cursorText(el) !== want.name && !(want.name === '' && !name)) {
    return 'its element is now named ' + JSON.stringify(name) + ', expected ' + JSON.stringify(want.name);
  }
  return el;
}
if (opts.resolve) return resolveRef(opts.resolve);

var nodes = [];
var hrefs = {};
function push(node) { node.nodeId = String(nodes.length); nodes.push(node); return node; }
function str(v) { return { type: 'computedString', value: v }; }

function childNodesOf(node) {
  if (node.nodeType === 1 && node.tagName === 'SLOT') {
    var assigned = node.assignedNodes({ flatten: true });
    return assigned.length ? assigned : Array.prototype.slice.call(node.childNodes);
  }
  var kids = node.shadowRoot ? node.shadowRoot.childNodes : node.childNodes;
  return Array.prototype.slice.call(kids);
}
// Interactive snapshots render only nodes that can take a ref, so the rest
// are flattened away here instead of being serialized and sent: for a large
// page that is most of the payload. The role lists come from the Rust side.
var interactiveOnly = !!opts.interactive;
var refRoles = {};
(opts.refRoles || []).forEach(function (r) { refRoles[r] = 1; });
var namedRefRoles = {};
(opts.namedRefRoles || []).forEach(function (r) { namedRefRoles[r] = 1; });

var cursor = [];
var cursorEls = new Set();
var cursorInfo = __abCursorScan(function (el, i) { cursor[i] = idOf(el); cursorEls.add(el); });

function canTakeRef(el, role) {
  return refRoles[role] || namedRefRoles[role] || cursorEls.has(el);
}

function walkChildren(node, ids) {
  childNodesOf(node).forEach(function (c) {
    if (c.nodeType === 3) {
      if (interactiveOnly) return;
      var text = c.data.replace(/[\s ]+/g, ' ');
      if (text.trim()) ids.push(push({ role: { type: 'internalRole', value: 'StaticText' }, name: str(text), childIds: [] }).nodeId);
    } else if (c.nodeType === 1) {
      walkElement(c, ids);
    }
  });
}
function walkElement(el, ids) {
  if (SKIP[el.tagName.toUpperCase()] || isHidden(el)) return;
  var role = roleOf(el);
  if (el.namespaceURI === 'http://www.w3.org/2000/svg') {
    // Inline SVG is a leaf: an image when it carries a name, otherwise decoration.
    var svgName = el.tagName.toLowerCase() === 'svg' ? nameOf(el, 'image') : '';
    if (svgName && (!interactiveOnly || canTakeRef(el, 'image'))) ids.push(push({ role: { type: 'role', value: 'image' }, name: str(svgName), backendDOMNodeId: idOf(el), childIds: [] }).nodeId);
    return;
  }
  var invisible = styleOf(el).visibility !== 'visible';
  if (!role || invisible) {
    walkChildren(el, ids);
    return;
  }
  if (interactiveOnly && !canTakeRef(el, role)) {
    walkChildren(el, ids);
    return;
  }
  var name = nameOf(el, role);
  if (interactiveOnly && !refRoles[role] && !cursorEls.has(el) && !name) {
    walkChildren(el, ids);
    return;
  }
  var node = push({ role: { type: 'role', value: role }, backendDOMNodeId: idOf(el), childIds: [] });
  if (name) node.name = str(name);
  var value = valueOf(el, role);
  if (value) node.value = { type: 'string', value: value };
  var props = propertiesOf(el, role);
  if (props.length) node.properties = props;
  if (opts.urls && role === 'link' && el.href) hrefs[node.backendDOMNodeId] = String(el.href);
  ids.push(node.nodeId);
  if (el.tagName !== 'IFRAME' && el.tagName !== 'TEXTAREA') walkChildren(el, node.childIds);
}

var root;
if (opts.selector) {
  var target = document.querySelector(opts.selector);
  if (!target) throw new Error("Selector '" + opts.selector + "' did not match any element");
  root = push({ role: { type: 'role', value: 'RootWebArea' }, childIds: [] });
  walkElement(target, root.childIds);
} else {
  root = push({ role: { type: 'role', value: 'RootWebArea' }, name: str(document.title || ''), backendDOMNodeId: idOf(document.documentElement), childIds: [] });
  if (document.body) walkElement(document.body, root.childIds);
}


var cursorOut = [];
for (var i = 0; i < cursorInfo.length; i++) {
  if (cursor[i] !== undefined) { cursorInfo[i].backendNodeId = cursor[i]; cursorOut.push(cursorInfo[i]); }
}

return { document: registry.document, nodes: nodes, cursor: cursorOut, hrefs: hrefs };
