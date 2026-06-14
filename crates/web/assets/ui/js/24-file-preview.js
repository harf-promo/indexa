/* ── File preview pane (Context tab) ─────────────────────────────────────────
   When a FILE is selected, fetch its raw text from /api/file and show it beside the summary,
   with lightweight syntax highlighting. Highlighting is a small self-written tokenizer (no third-
   party library, to keep the frontend dependency-free) — good for a preview, not a full grammar.
   `previewOpen` (persisted) toggles the pane. */

var previewOpen = (function () {
  try { return localStorage.getItem('indexa.previewOpen') !== 'false'; } catch (_) { return true; }
}());

// Per-language keyword sets for the highlighter. Comment style is chosen separately below.
var PREVIEW_KEYWORDS = {
  rust: ['as','async','await','break','const','continue','crate','dyn','else','enum','extern','false','fn','for','if','impl','in','let','loop','match','mod','move','mut','pub','ref','return','self','Self','static','struct','super','trait','true','type','unsafe','use','where','while'],
  python: ['and','as','assert','async','await','break','class','continue','def','del','elif','else','except','finally','for','from','global','if','import','in','is','lambda','nonlocal','not','or','pass','raise','return','try','while','with','yield','True','False','None','self'],
  javascript: ['async','await','break','case','catch','class','const','continue','debugger','default','delete','do','else','export','extends','false','finally','for','function','if','import','in','instanceof','let','new','null','of','return','super','switch','this','throw','true','try','typeof','undefined','var','void','while','yield'],
  go: ['break','case','chan','const','continue','default','defer','else','fallthrough','for','func','go','goto','if','import','interface','map','package','range','return','select','struct','switch','type','var','nil','true','false'],
  java: ['abstract','boolean','break','byte','case','catch','char','class','const','continue','default','do','double','else','enum','extends','final','finally','float','for','if','implements','import','instanceof','int','interface','long','native','new','package','private','protected','public','return','short','static','super','switch','synchronized','this','throw','throws','try','void','volatile','while','true','false','null'],
  c: ['auto','break','case','char','const','continue','default','do','double','else','enum','extern','float','for','goto','if','int','long','register','return','short','signed','sizeof','static','struct','switch','typedef','union','unsigned','void','volatile','while'],
  sql: ['SELECT','FROM','WHERE','INSERT','UPDATE','DELETE','CREATE','TABLE','INDEX','JOIN','LEFT','RIGHT','INNER','OUTER','ON','GROUP','BY','ORDER','HAVING','LIMIT','VALUES','SET','INTO','AND','OR','NOT','NULL','PRIMARY','KEY','FOREIGN','REFERENCES','DISTINCT','AS'],
};
PREVIEW_KEYWORDS.typescript = PREVIEW_KEYWORDS.javascript.concat(['interface','type','enum','namespace','public','private','protected','readonly','implements','declare','abstract','as','keyof','infer']);
PREVIEW_KEYWORDS.tsx = PREVIEW_KEYWORDS.typescript;
PREVIEW_KEYWORDS.cpp = PREVIEW_KEYWORDS.c.concat(['class','namespace','template','typename','public','private','protected','virtual','override','new','delete','this','true','false','nullptr','using','constexpr']);

// Languages whose line comment is `#` rather than `//`.
var HASH_COMMENT = { python: 1, shell: 1, yaml: 1, toml: 1 };
// Languages with C-style `/* */` block comments.
var BLOCK_COMMENT = { rust: 1, javascript: 1, typescript: 1, tsx: 1, go: 1, java: 1, c: 1, cpp: 1, css: 1, sql: 1 };

function previewSpan(cls, text) {
  return '<span class="hl-' + cls + '">' + escapeHtml(text) + '</span>';
}

// Single-pass tokenizer: each character is classified once (so keywords inside strings/comments
// are never re-highlighted). Falls back to escaped plain text for unknown languages.
function highlightCode(code, lang) {
  var kw = PREVIEW_KEYWORDS[lang];
  if (!kw) return escapeHtml(code); // unknown language → plain (still safe)
  var kwSet = {};
  kw.forEach(function (k) { kwSet[k] = 1; });

  var parts = [];
  if (BLOCK_COMMENT[lang]) parts.push('\\/\\*[\\s\\S]*?\\*\\/');     // /* block */
  parts.push(HASH_COMMENT[lang] ? '#[^\\n]*' : '\\/\\/[^\\n]*');     // line comment
  parts.push('"(?:\\\\.|[^"\\\\])*"|\'(?:\\\\.|[^\'\\\\])*\'|`(?:\\\\.|[^`\\\\])*`'); // strings
  parts.push('\\b\\d[\\d_]*(?:\\.\\d+)?(?:[eE][+-]?\\d+)?\\b');      // numbers
  parts.push('[A-Za-z_$][A-Za-z0-9_$]*');                            // identifiers
  // One alternation; the first matching group wins, so order above is the precedence.
  var re = new RegExp(
    '(' + parts[0] + ')' +
    (BLOCK_COMMENT[lang] ? ('|(' + parts[1] + ')|(' + parts[2] + ')|(' + parts[3] + ')|(' + parts[4] + ')')
                         : ('|(' + parts[1] + ')|(' + parts[2] + ')|(' + parts[3] + ')')),
    'g'
  );
  // Normalize: index of (comment, string, number, identifier) groups depends on whether a block
  // group exists. Build a typed walker instead of guessing indices.
  var out = '';
  var last = 0;
  var m;
  re.lastIndex = 0;
  while ((m = re.exec(code)) !== null) {
    if (m.index === re.lastIndex) { re.lastIndex++; continue; } // guard against zero-width
    out += escapeHtml(code.slice(last, m.index));
    var tok = m[0];
    var cls;
    if (tok.indexOf('//') === 0 || (HASH_COMMENT[lang] && tok.indexOf('#') === 0) || tok.indexOf('/*') === 0) {
      cls = 'comment';
    } else if (tok[0] === '"' || tok[0] === '\'' || tok[0] === '`') {
      cls = 'string';
    } else if (tok[0] >= '0' && tok[0] <= '9') {
      cls = 'number';
    } else if (kwSet[tok]) {
      cls = 'keyword';
    } else {
      cls = null; // plain identifier
    }
    out += cls ? previewSpan(cls, tok) : escapeHtml(tok);
    last = m.index + tok.length;
  }
  out += escapeHtml(code.slice(last));
  return out;
}

function previewPaneEl() { return document.getElementById('file-preview-pane'); }

function clearPreview() {  // eslint-disable-line no-unused-vars
  var pane = previewPaneEl();
  if (!pane) return;
  var body = document.getElementById('preview-body');
  var lang = document.getElementById('preview-lang');
  var trunc = document.getElementById('preview-trunc');
  if (body) body.innerHTML = '<div class="preview-placeholder">Select a file to preview it.</div>';
  if (lang) lang.textContent = '';
  if (trunc) trunc.hidden = true;
}

function renderPreview(d) {
  var body = document.getElementById('preview-body');
  var langEl = document.getElementById('preview-lang');
  var truncEl = document.getElementById('preview-trunc');
  if (!body) return;
  if (langEl) langEl.textContent = d.language || 'text';
  if (truncEl) {
    truncEl.hidden = !d.truncated;
    if (d.truncated) truncEl.textContent = 'first 40 KB of ' + fmtUpdateBytes(d.bytes_total);
  }
  if (d.binary || d.content == null) {
    body.innerHTML = '<div class="preview-placeholder">Binary file (' + fmtUpdateBytes(d.bytes_total || 0) + ') — no preview.</div>';
    return;
  }
  var highlighted = highlightCode(d.content, d.language);
  var lineCount = d.content.split('\n').length;
  var gutter = '';
  for (var i = 1; i <= lineCount; i++) gutter += i + '\n';
  body.innerHTML =
    '<div class="code-wrap">' +
    '<pre class="code-gutter" aria-hidden="true"></pre>' +
    '<pre class="preview-code"></pre>' +
    '</div>';
  body.querySelector('.code-gutter').textContent = gutter;
  body.querySelector('.preview-code').innerHTML = highlighted;
}

async function showFilePreview(path) {  // eslint-disable-line no-unused-vars
  var pane = previewPaneEl();
  if (!pane || !previewOpen) return;
  var body = document.getElementById('preview-body');
  if (body) body.innerHTML = '<div class="preview-placeholder">Loading…</div>';
  try {
    var r = await fetch('/api/file?path=' + encodeURIComponent(path));
    if (!r.ok) {
      // 400 = a directory (no content to preview) → neutral placeholder; other codes → brief note.
      clearPreview();
      if (r.status !== 400 && body) {
        body.innerHTML = '<div class="preview-placeholder">Preview unavailable (' + r.status + ').</div>';
      }
      return;
    }
    renderPreview(await r.json());
  } catch (_) {
    if (body) body.innerHTML = '<div class="preview-placeholder">Preview failed to load.</div>';
  }
}

function applyPreviewVisibility() {
  var pane = previewPaneEl();
  var btn = document.getElementById('preview-toggle-btn');
  if (pane) pane.hidden = !previewOpen;
  if (btn) btn.setAttribute('aria-pressed', previewOpen ? 'true' : 'false');
}

function togglePreview() {  // eslint-disable-line no-unused-vars
  previewOpen = !previewOpen;
  try { localStorage.setItem('indexa.previewOpen', previewOpen ? 'true' : 'false'); } catch (_) { /* ignore */ }
  applyPreviewVisibility();
  // If turning on while a file is selected, populate it now.
  if (previewOpen && typeof selectedPath !== 'undefined' && selectedPath) showFilePreview(selectedPath);
}

document.addEventListener('DOMContentLoaded', applyPreviewVisibility);
