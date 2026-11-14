// A small, safe Markdown renderer for the changelog: headings, lists, paragraphs,
// **bold**, *italic*, `code` and [links](url) (shown as text). HTML is escaped first,
// so the output is safe for {@html}.

const esc = (t: string) => t.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

/** Inline formatting on one line of text. */
export function inline(text: string): string {
  const codes: string[] = [];
  let s = esc(text).replace(/`([^`]+)`/g, (_, c) => `\u0000${codes.push(`<code>${c}</code>`) - 1}\u0000`);
  s = s
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[^*\w])\*([^*\s][^*]*?)\*(?!\w)/g, "$1<em>$2</em>")
    .replace(/(^|[^\w])_([^_\s][^_]*?)_(?!\w)/g, "$1<em>$2</em>");
  return s.replace(/\u0000(\d+)\u0000/g, (_, i) => codes[+i]);
}

/** Block-level Markdown (without the entry title) to HTML. */
export function render(md: string): string {
  const out: string[] = [];
  let list = false;
  let para: string[] = [];
  const flush = () => {
    if (para.length) out.push(`<p>${inline(para.join(" "))}</p>`);
    para = [];
  };
  const closeList = () => {
    if (list) out.push("</ul>");
    list = false;
  };
  for (const raw of md.split(/\r?\n/)) {
    const line = raw.trimEnd();
    const h = line.match(/^#{3,6}\s+(.*)$/);
    const li = line.match(/^\s*[-*+]\s+(.*)$/);
    if (h) {
      flush();
      closeList();
      out.push(`<h4>${inline(h[1])}</h4>`);
    } else if (li) {
      flush();
      if (!list) out.push("<ul>");
      list = true;
      out.push(`<li>${inline(li[1])}</li>`);
    } else if (!line.trim()) {
      flush();
      closeList();
    } else if (list && /^\s+\S/.test(raw)) {
      out[out.length - 1] = out[out.length - 1].replace(/<\/li>$/, ` ${inline(line.trim())}</li>`);
    } else {
      closeList();
      para.push(line.trim());
    }
  }
  flush();
  closeList();
  return out.join("");
}

export interface Entry {
  title: string;
  html: string;
}

/** Splits a CHANGELOG.md on its `## ` headings. */
export function parseChangelog(md: string): Entry[] {
  return md
    .split(/^##\s+/m)
    .slice(1)
    .map((chunk) => {
      const nl = chunk.indexOf("\n");
      const title = (nl < 0 ? chunk : chunk.slice(0, nl)).trim();
      return { title, html: render(nl < 0 ? "" : chunk.slice(nl + 1)) };
    })
    .filter((e) => e.title);
}
