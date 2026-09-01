// RustClaw UI — Preact with htm, no build step.
//
// htm gives JSX-like syntax through tagged template literals, so the source in
// the repo is the source the browser runs. Everything is vendored under
// /assets/vendor, which keeps the page working on a machine with no network.

import { h, render } from "preact";
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import htmModule from "htm";
import { marked } from "marked";
import hljs from "hljs";

const html = htmModule.bind(h);

// --- markdown ---------------------------------------------------------------

const LANGUAGES = ["rust", "javascript", "typescript", "python", "bash", "json",
                   "xml", "css", "sql", "yaml", "markdown", "ini", "diff"];
const ALIASES = { js: "javascript", ts: "typescript", sh: "bash", shell: "bash",
                  html: "xml", yml: "yaml", toml: "ini", rs: "rust", py: "python" };

await Promise.all(LANGUAGES.map(async (name) => {
  const mod = await import(`/assets/vendor/hl/${name}.mjs`);
  hljs.registerLanguage(name, mod.default);
}));

// Raw HTML stays off. Tool output carries file contents the model never wrote,
// so a file containing <script> would otherwise run in this page.
function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

marked.setOptions({ gfm: true, breaks: true });

// Escaping the source is not enough on its own: marked builds anchors itself,
// so `[x](javascript:alert(1))` in a file the agent read would still produce a
// live javascript: link. Only http(s) and mailto survive.
const SAFE_SCHEME = /^(https?:|mailto:|#|\/|\.)/i;
marked.use({
  renderer: {
    link({ href, title, tokens }) {
      const text = this.parser.parseInline(tokens);
      if (!href || !SAFE_SCHEME.test(href.trim())) return text;
      const t = title ? ` title="${escapeHtml(title)}"` : "";
      return `<a href="${escapeHtml(href)}"${t} target="_blank" rel="noopener noreferrer">${text}</a>`;
    },
    image({ href, title, text }) {
      // Images would fetch from wherever the markdown says; keep the alt text.
      if (!href || !/^https?:/i.test(href.trim())) return escapeHtml(text || "");
      return `<img src="${escapeHtml(href)}" alt="${escapeHtml(text || "")}"${
        title ? ` title="${escapeHtml(title)}"` : ""} />`;
    },
  },
});


/// Split markdown into prose and fenced code, so code blocks become real
/// components with a copy button instead of innerHTML.
function splitBlocks(text) {
  const parts = [];
  const fence = /^```([\w+-]*)\n([\s\S]*?)```$/gm;
  let last = 0, m;
  while ((m = fence.exec(text)) !== null) {
    if (m.index > last) parts.push({ type: "md", text: text.slice(last, m.index) });
    parts.push({ type: "code", lang: m[1] || "", code: m[2] });
    last = fence.lastIndex;
  }
  const tail = text.slice(last);
  // An unclosed fence means the model is still streaming the block.
  const open = tail.match(/^```([\w+-]*)\n([\s\S]*)$/m);
  if (open) {
    const before = tail.slice(0, open.index);
    if (before.trim()) parts.push({ type: "md", text: before });
    parts.push({ type: "code", lang: open[1] || "", code: open[2], streaming: true });
  } else if (tail) {
    parts.push({ type: "md", text: tail });
  }
  return parts;
}

function Markdown({ text }) {
  // Escaping before parsing is what keeps raw HTML inert; marked then only
  // produces the tags it generates itself.
  const dirty = marked.parse(escapeHtml(text));
  return html`<div dangerouslySetInnerHTML=${{ __html: dirty }} />`;
}

function CodeBlock({ lang, code, streaming }) {
  const [copied, setCopied] = useState(false);
  const resolved = ALIASES[lang] || lang;
  let body;
  try {
    body = resolved && hljs.getLanguage(resolved)
      ? hljs.highlight(code, { language: resolved }).value
      : hljs.highlightAuto(code).value;
  } catch {
    body = escapeHtml(code);
  }
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(code);
      setCopied(true);
      setTimeout(() => setCopied(false), 1400);
    } catch { /* clipboard blocked; the selection still works */ }
  };
  return html`
    <div class="code">
      <div class="code-head">
        <span>${resolved || "text"}${streaming ? " …" : ""}</span>
        <button onClick=${copy} title="Copy code">${copied ? "copied" : "copy"}</button>
      </div>
      <pre><code dangerouslySetInnerHTML=${{ __html: body }} /></pre>
    </div>`;
}

function Rich({ text }) {
  return splitBlocks(text).map((p, i) =>
    p.type === "code"
      ? html`<${CodeBlock} key=${i} ...${p} />`
      : html`<${Markdown} key=${i} text=${p.text} />`);
}

// --- icons ------------------------------------------------------------------

const Icon = ({ d, size = 18 }) => html`
  <svg width=${size} height=${size} viewBox="0 0 24 24" fill="none"
       stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
    <path d=${d} />
  </svg>`;
const I = {
  plus: "M12 5v14M5 12h14",
  menu: "M3 12h18M3 6h18M3 18h18",
  trash: "M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6",
  pencil: "M12 20h9M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z",
  send: "M12 19V5M5 12l7-7 7 7",
  stop: "M6 6h12v12H6z",
  redo: "M21 2v6h-6M21 8a9 9 0 1 0-2 5",
  copy: "M9 9h10v10H9zM5 15V5h10",
  down: "M12 5v14M5 12l7 7 7-7",
  sun: "M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8M12 2v2M12 20v2M5 5l1.5 1.5M17.5 17.5 19 19M2 12h2M20 12h2M5 19l1.5-1.5M17.5 6.5 19 5",
  moon: "M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8",
  download: "M12 3v12M7 11l5 5 5-5M5 21h14",
};

// --- api --------------------------------------------------------------------

async function api(path, opts = {}) {
  const r = await fetch(`/api${path}`, {
    headers: { "content-type": "application/json" }, ...opts,
  });
  if (!r.ok) throw new Error(`${r.status} ${await r.text()}`);
  return r.status === 204 ? null : r.json();
}

// --- app --------------------------------------------------------------------

function App() {
  const [convos, setConvos] = useState([]);
  const [active, setActive] = useState(null);
  const [items, setItems] = useState([]);      // rendered transcript
  const [busy, setBusy] = useState(false);
  const [usage, setUsage] = useState(null);
  const [meta, setMeta] = useState({ model: "", version: "" });
  // On a phone the sidebar is an overlay, so starting it open would cover the
  // conversation the link was opened to read.
  const narrow = () => typeof window !== "undefined" && window.innerWidth <= 720;
  const [sidebar, setSidebar] = useState(() => !narrow());
  const [theme, setTheme] = useState(() => localStorage.getItem("rustclaw-theme") || "system");
  const [atBottom, setAtBottom] = useState(true);
  const logRef = useRef(null);

  useEffect(() => {
    const root = document.documentElement;
    if (theme === "system") root.removeAttribute("data-theme");
    else root.setAttribute("data-theme", theme);
    // The highlight themes are media-gated for "system"; force one otherwise.
    const dark = theme === "dark" ||
      (theme === "system" && matchMedia("(prefers-color-scheme: dark)").matches);
    document.getElementById("hl-dark").media = dark ? "all" : "not all";
    document.getElementById("hl-light").media = dark ? "not all" : "all";
    try { localStorage.setItem("rustclaw-theme", theme); } catch { /* private mode */ }
  }, [theme]);

  const refreshConvos = useCallback(async () => {
    try {
      const d = await api("/sessions");
      setConvos(d.sessions);
      setActive((cur) => cur ?? d.active);
    } catch { /* the server will be polled again on the next turn */ }
  }, []);

  const loadConvo = useCallback(async (id) => {
    try {
      const d = await api(`/sessions/${id}`);
      setItems(d.messages.map(fromDto));
      setActive(id);
    } catch (e) { pushNotice(setItems, String(e)); }
  }, []);

  useEffect(() => {
    (async () => {
      try {
        const s = await api("/state");
        setMeta({ model: s.model, version: s.version });
        setActive(s.sessionId);
        await loadConvo(s.sessionId);
      } catch { /* server still starting */ }
      refreshConvos();
    })();
  }, [loadConvo, refreshConvos]);

  // --- live events
  useEffect(() => {
    const es = new EventSource("/api/events");
    const on = (name, fn) => es.addEventListener(name, (e) => fn(JSON.parse(e.data)));

    on("turn_start", () => {
      setBusy(true);
      setItems(closeOpen);
    });
    // The bubble being streamed into is the last item when it is an open
    // assistant turn. Deriving that from state rather than from a ref is what
    // keeps it correct: an index held in a ref went stale the moment a tool
    // call pushed an item, and every later delta then started its own bubble —
    // which split code spans across bubbles and rendered their backticks raw.
    on("delta", (t) => setItems((prev) => {
      const last = prev[prev.length - 1];
      if (last && last.kind === "assistant" && last.open) {
        return [...prev.slice(0, -1), { ...last, text: last.text + t }];
      }
      return [...prev, { kind: "assistant", text: t, open: true }];
    }));
    on("tool_start", ({ name, input }) =>
      setItems((prev) => [...closeOpen(prev), { kind: "tool", name, input, running: true }]));
    on("tool_end", ({ name, isError, preview }) => setItems((prev) => {
      const next = [...prev];
      for (let i = next.length - 1; i >= 0; i--) {
        if (next[i].kind === "tool" && next[i].running) {
          next[i] = { ...next[i], running: false, isError, preview };
          return next;
        }
      }
      return [...next, { kind: "tool", name, isError, preview }];
    }));
    on("compacted", ({ dropped }) => setItems((prev) =>
      [...prev, { kind: "notice", text: `Context compacted — ${dropped} messages summarized` }]));
    on("error", (msg) => { setItems(closeOpen); pushNotice(setItems, msg, true); });
    on("turn_end", (u) => {
      setBusy(false);
      setUsage(u.input || u.output ? u : null);
      setItems((prev) =>
        closeOpen(prev).filter((it) => !(it.kind === "assistant" && !it.text.trim())));
      refreshConvos();
    });
    return () => es.close();
  }, [refreshConvos]);

  // --- scrolling
  useLayoutEffect(() => {
    const el = logRef.current;
    if (el && atBottom) el.scrollTop = el.scrollHeight;
  }, [items, atBottom]);

  const onScroll = () => {
    const el = logRef.current;
    if (el) setAtBottom(el.scrollHeight - el.scrollTop - el.clientHeight < 80);
  };

  // --- actions
  const send = async (text) => {
    setItems((prev) => [...prev, { kind: "user", text }]);
    setAtBottom(true);
    try {
      await api("/chat", { method: "POST", body: JSON.stringify({ message: text }) });
    } catch (e) {
      // turn_end still fires and clears `busy`; this only reports the refusal.
      pushNotice(setItems, String(e), true);
      setBusy(false);
    }
  };

  const stop = () => api("/abort", { method: "POST" }).catch(() => {});

  const regenerate = async () => {
    if (busy) return;
    // The server rewinds the transcript, so mirror that locally first.
    setItems((prev) => {
      const at = prev.map((i) => i.kind).lastIndexOf("user");
      return at < 0 ? prev : prev.slice(0, at + 1);
    });
    try {
      await api("/regenerate", { method: "POST" });
    } catch (e) { pushNotice(setItems, String(e), true); setBusy(false); }
  };

  const newChat = async () => {
    try {
      const d = await api("/sessions", { method: "POST" });
      setItems([]); setUsage(null); setActive(d.id);
      if (narrow()) setSidebar(false);
      refreshConvos();
    } catch (e) { pushNotice(setItems, String(e), true); }
  };

  const selectConvo = async (id) => {
    if (id === active || busy) return;
    try {
      await api(`/sessions/${id}/select`, { method: "POST" });
      await loadConvo(id);
      setUsage(null);
      if (narrow()) setSidebar(false);
    } catch (e) { pushNotice(setItems, String(e), true); }
  };

  const rename = async (id, title) => {
    try {
      await api(`/sessions/${id}`, { method: "PATCH", body: JSON.stringify({ title }) });
      refreshConvos();
    } catch (e) { pushNotice(setItems, String(e), true); }
  };

  const remove = async (id) => {
    try {
      const d = await api(`/sessions/${id}`, { method: "DELETE" });
      if (id === active) { setActive(d.active); setItems([]); setUsage(null); }
      refreshConvos();
    } catch (e) { pushNotice(setItems, String(e), true); }
  };

  const title = convos.find((c) => c.id === active)?.title || "New chat";

  return html`
    <div class="app">
      <${Sidebar} collapsed=${!sidebar} convos=${convos} active=${active}
        onNew=${newChat} onSelect=${selectConvo} onRename=${rename} onDelete=${remove}
        theme=${theme} setTheme=${setTheme} meta=${meta} />
      ${sidebar && html`<div class="scrim" onClick=${() => setSidebar(false)} />`}
      <div class="main">
        <div class="topbar">
          <button class="icon-btn" onClick=${() => setSidebar((s) => !s)} title="Toggle sidebar">
            <${Icon} d=${I.menu} />
          </button>
          <div class="topbar-title">${title}</div>
          ${usage && html`<span class="notice">${usage.input} in / ${usage.output} out</span>`}
          ${active && html`
            <a class="icon-btn" href=${`/api/sessions/${active}/export`} download
               title="Export this conversation as Markdown">
              <${Icon} d=${I.download} size=${16} />
            </a>`}
        </div>

        <div class="log" ref=${logRef} onScroll=${onScroll}>
          <div class="log-inner">
            ${items.length === 0
              ? html`<div class="empty">
                       <h2>RustClaw</h2>
                       <p>${meta.model || "…"}</p>
                     </div>`
              : items.map((it, i) => html`<${Item} key=${i} item=${it}
                    last=${i === items.length - 1} busy=${busy} onRegenerate=${regenerate} />`)}
          </div>
        </div>

        ${!atBottom && html`
          <button class="scroll-down" title="Scroll to bottom"
                  onClick=${() => { setAtBottom(true); }}>
            <${Icon} d=${I.down} size=${16} />
          </button>`}

        <${Composer} busy=${busy} onSend=${send} onStop=${stop} />
      </div>
    </div>`;
}

// --- pieces -----------------------------------------------------------------

function Sidebar({ collapsed, convos, active, onNew, onSelect, onRename, onDelete,
                   theme, setTheme, meta }) {
  const [editing, setEditing] = useState(null);
  const cycle = () => setTheme(theme === "system" ? "light" : theme === "light" ? "dark" : "system");
  return html`
    <aside class=${"sidebar" + (collapsed ? " collapsed" : "")}>
      <div class="sidebar-head">
        <span class="brand">RustClaw<small>v${meta.version}</small></span>
      </div>
      <button class="new-chat" onClick=${onNew}>
        <${Icon} d=${I.plus} size=${16} /> New chat
      </button>
      <div class="convos">
        ${convos.map((c) => html`
          <div key=${c.id} class=${"convo" + (c.id === active ? " active" : "")}
               onClick=${() => editing !== c.id && onSelect(c.id)}>
            ${editing === c.id
              ? html`<input value=${c.title} autofocus
                       onClick=${(e) => e.stopPropagation()}
                       onBlur=${(e) => { onRename(c.id, e.target.value); setEditing(null); }}
                       onKeyDown=${(e) => {
                         if (e.key === "Enter") { onRename(c.id, e.target.value); setEditing(null); }
                         if (e.key === "Escape") setEditing(null);
                       }} />`
              : html`
                <span class="convo-title" title=${c.title}>${c.title}</span>
                <span class="convo-actions">
                  <button title="Rename"
                          onClick=${(e) => { e.stopPropagation(); setEditing(c.id); }}>
                    <${Icon} d=${I.pencil} size=${13} />
                  </button>
                  <a title="Export as Markdown" href=${`/api/sessions/${c.id}/export`} download
                     onClick=${(e) => e.stopPropagation()}>
                    <${Icon} d=${I.download} size=${13} />
                  </a>
                  <button title="Delete"
                          onClick=${(e) => { e.stopPropagation();
                            if (confirm(`Delete "${c.title}"?`)) onDelete(c.id); }}>
                    <${Icon} d=${I.trash} size=${13} />
                  </button>
                </span>`}
          </div>`)}
      </div>
      <div class="sidebar-foot">
        <span title=${meta.model}>${meta.model}</span>
        <button class="icon-btn" onClick=${cycle} title=${`Theme: ${theme}`}>
          <${Icon} d=${theme === "dark" ? I.moon : I.sun} size=${15} />
        </button>
      </div>
    </aside>`;
}

function Item({ item, last, busy, onRegenerate }) {
  const [copied, setCopied] = useState(false);

  if (item.kind === "user") {
    return html`<div class="turn user"><div class="bubble">${item.text}</div></div>`;
  }
  if (item.kind === "tool") {
    const mark = item.running ? "⋯" : item.isError ? "✗" : "✓";
    return html`
      <div class=${"tool" + (item.isError ? " err" : "")}>
        ${mark} <span class="tool-name">${item.name}</span>${
          item.running ? ` ${item.input || ""}` : ` ${oneLine(item.preview || "")}`}
      </div>`;
  }
  if (item.kind === "notice") {
    return html`<div class=${"notice" + (item.isError ? " err" : "")}>${item.text}</div>`;
  }

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(item.text);
      setCopied(true); setTimeout(() => setCopied(false), 1400);
    } catch { /* ignore */ }
  };
  const streaming = busy && item.open;
  return html`
    <div class="turn assistant">
      <div class=${"bubble" + (streaming ? " cursor" : "")}><${Rich} text=${item.text} /></div>
      ${!streaming && item.text && html`
        <div class="turn-actions">
          <button onClick=${copy}><${Icon} d=${I.copy} size=${13} />${copied ? "copied" : "copy"}</button>
          ${last && html`<button onClick=${onRegenerate}>
            <${Icon} d=${I.redo} size=${13} />regenerate</button>`}
        </div>`}
    </div>`;
}

function Composer({ busy, onSend, onStop }) {
  const [text, setText] = useState("");
  const ref = useRef(null);

  const grow = () => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = Math.min(el.scrollHeight, 192) + "px";
  };
  useEffect(grow, [text]);

  const submit = (e) => {
    e?.preventDefault();
    const t = text.trim();
    if (!t || busy) return;
    setText("");
    onSend(t);
  };

  return html`
    <div class="composer-wrap">
      <form class="composer" onSubmit=${submit}>
        <textarea ref=${ref} rows="1" value=${text} autofocus
          placeholder=${busy ? "Running…" : "Message RustClaw"}
          onInput=${(e) => setText(e.target.value)}
          onKeyDown=${(e) => {
            if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); submit(); }
          }} />
        ${busy
          ? html`<button type="button" class="send stop" onClick=${onStop} title="Stop">
                   <${Icon} d=${I.stop} size=${13} /></button>`
          : html`<button type="submit" class="send" disabled=${!text.trim()} title="Send">
                   <${Icon} d=${I.send} size=${16} /></button>`}
      </form>
      <div class="hint">Every tool call runs without approval. Enter to send, Shift+Enter for a newline.</div>
    </div>`;
}

// --- helpers ----------------------------------------------------------------

/// Mark every assistant bubble finished, so the next delta opens a new one.
function closeOpen(items) {
  return items.map((it) => (it.kind === "assistant" && it.open ? { ...it, open: false } : it));
}

function pushNotice(setItems, text, isError = false) {
  setItems((prev) => [...prev, { kind: "notice", text, isError }]);
}

const oneLine = (s) => s.replace(/\s+/g, " ").slice(0, 160);

/// Map the server's transcript DTO onto the same item shape the live stream
/// produces, so history and streaming render through one code path.
function fromDto(m) {
  if (m.role === "user") return { kind: "user", text: m.text };
  if (m.role === "tool") {
    return { kind: "tool", name: m.tool.name, isError: m.tool.isError, preview: m.tool.preview };
  }
  return { kind: "assistant", text: m.text || "", calls: m.toolCalls || [] };
}

render(html`<${App} />`, document.getElementById("app"));
