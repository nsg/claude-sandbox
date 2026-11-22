#!/usr/bin/env node
"use strict";

// T3 Code labels file links with the basename ("data · srv") and has no setting
// for it. Edit the built client to show paths instead: workspace-relative links
// as written, others in full (also links relative to a previewed file's
// directory). The release ships the client as plain files, so this needs no
// build. An unknown client is left untouched.

const fs = require("fs");
const path = require("path");

// In ChatMarkdown's fileLinkChip, minified:
//   label:fn({path:meta.filePath,...line, column and endLine},suffixes)
const LABEL = /label:[\w$]+\(\{path:([\w$]+)\.filePath,[^()]{0,300}\.endLine[^()]{0,100}\},[\w$]+\)/g;
const LABEL_MARKER = "/*full-paths*/";

// Upstream cuts chips at the end, which would hide the filename. Only the
// label wraps, so the icon stays with it.
const STYLE = `<style id="full-paths">
.chat-markdown .chat-markdown-file-link { display: inline; overflow-wrap: anywhere; }
.chat-markdown .chat-markdown-file-link > [data-slot="context-chip-label"] { white-space: normal; }
.chat-markdown :is(td, th) .chat-markdown-file-link { overflow-wrap: break-word; }
</style>
`;

function label(_call, meta) {
  return (
    `label:${LABEL_MARKER}((${meta}.workspaceLookupPath??${meta}.filePath)` +
    `+(${meta}.line?":"+${meta}.line` +
    `+(${meta}.endLine?"-"+${meta}.endLine:${meta}.column?":"+${meta}.column:""):""))`
  );
}

// The server may be serving these files, so never leave one half written.
function replace(file, content) {
  const temporary = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(temporary, content);
  fs.renameSync(temporary, file);
}

function patch(client) {
  const assets = path.join(client, "assets");
  const found = [];
  let patched = false;
  for (const name of fs.readdirSync(assets)) {
    if (!/^ChatMarkdown-.*\.js$/.test(name)) continue;
    const file = path.join(assets, name);
    const source = fs.readFileSync(file, "utf8");
    if (source.includes(LABEL_MARKER)) patched = true;
    for (const match of source.matchAll(LABEL)) found.push({ file, source, match });
  }
  if (patched) return;
  if (found.length !== 1) {
    throw new Error(`expected one file link label in the client, found ${found.length}`);
  }

  const index = path.join(client, "index.html");
  const html = fs.readFileSync(index, "utf8");
  if (!html.includes('id="full-paths"')) {
    if (!html.includes("</head>")) throw new Error("no </head> in index.html");
    replace(index, html.replace("</head>", () => `${STYLE}</head>`));
  }

  // Last, as its marker is what says the patch is complete.
  const { file, source, match } = found[0];
  const end = match.index + match[0].length;
  replace(file, source.slice(0, match.index) + label(match[0], match[1]) + source.slice(end));
}

try {
  if (!process.argv[2]) throw new Error("usage: t3code-full-paths <client directory>");
  patch(process.argv[2]);
} catch (error) {
  process.stderr.write(`t3code-full-paths: not patched, file links keep short labels: ${error.message}\n`);
  process.exit(1);
}
