---
name: stathost
description: Publish a self-contained HTML page to the user's StatHost static host and hand back a clickable link, for results that outgrow chat Markdown — wide or dense tables, side-by-side comparisons, color-coded matrices, reports with real layout, charts, diagrams, sortable or filterable data. Load whenever you are about to squeeze such a result into a Markdown table or a long wall of text, and whenever the user asks for a page, a link, or a nicer view of something. Only applies when `/workspace/.claude-sandbox/stathost/secrets` exists. Published pages are public with no authentication, so load it before publishing anything to learn what must stay out.
---

# Rich HTML pages on StatHost

Chat Markdown is fine for a short answer and poor at everything else: tables
cannot merge cells, color a row, freeze a header, or hold more than a few
columns before they wrap into noise. When a result deserves real layout, write
it as an HTML page, publish it to StatHost, and give the user the link. They
open it in a browser and get the full width of their screen.

Reach for a page when the layout carries meaning: a comparison across many
attributes, a matrix where color shows status, a report with sections and
figures, a chart, a timeline, a table the reader will want to sort or filter.
Stay in chat for anything a few lines or a small table already says well — a
link is an extra click, and it has to be worth it.

## The page is public

StatHost serves every published file to anyone who has the URL. There is no
login, no expiry, and no access log you can consult afterwards. The only thing
between a page and the whole internet is that its URL is hard to guess — and a
URL is not a secret that stays put: it lands in browser history, chat
transcripts, link previews, and whatever the user forwards it to.

So decide before publishing, not after: **would it be fine if this page turned
up in a search result?** If not, or if you cannot tell, do not publish it.

Never publish:

- Credentials of any kind — tokens, passwords, API keys, private keys, cookies,
  `.env` contents, connection strings, and the StatHost secrets themselves.
- Personal data — names tied to contact details, addresses, account or
  customer records, anything from a production database.
- Private source code, unreleased work, internal documents, or business
  figures the user has not already made public.
- Internal hostnames, addresses, network layout, or unfixed security findings.

Check what you are about to render, not just what was asked for: a harmless
"show me this table" can carry a token column, and a screenshot can show more
than its subject. When part of a result is sensitive, drop or redact those
fields and say that you did. When the result is sensitive as a whole, keep it
in chat or in a local file, and tell the user why there is no link.

Publishing material from the list above is the user's call to make explicitly.
A request for a page, a link, or a browser view is not that decision: the user
is thinking about layout, not about where the file ends up. Ask one short
question that names what would become public, and publish once they say yes.
Live credentials are the exception — they do not go on a public host under
any instruction.

If something sensitive does get published, delete it at once (see below), tell
the user plainly, and treat any exposed credential as compromised: deleting
the page does not recall copies already fetched.

## Configuration

The endpoint lives in `/workspace/.claude-sandbox/stathost/secrets`, a shell
file with three variables:

```text
STATHOST_HOST=https://static.example.com/
STATHOST_BUCKET=my-bucket
STATHOST_TOKEN=...
```

If the file is missing, StatHost is not set up for this workspace: say so and
give the result another way. Do not look for a host elsewhere or guess one.

Source the file inside the command that needs it — shell state does not carry
over between commands. Never print the file, echo the token, or put the token
on a command line where it shows up in the transcript and process list. The
commands below feed it to `curl` on stdin for that reason.

## Write the page

Keep the page, and any script or data used to build it, under
`/workspace/.claude-sandbox/stathost/pages/<id>/`, where `<id>` is the random
directory name from the next section. The local path then mirrors the remote
one, and the id is still there when the page needs revising. Upload only the
files the page needs, never the directory wholesale.

Make the page one self-contained file:

```html
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="referrer" content="no-referrer">
  <meta name="robots" content="noindex, nofollow">
  <title>Short descriptive title</title>
  <style>
    :root { color-scheme: light dark; }
    /* all CSS inline */
  </style>
</head>
```

- `charset` is required: StatHost sends `text/html` without one, so a page
  that omits it shows non-ASCII text as mojibake.
- `referrer` and `robots` protect the URL. Without `no-referrer`, every
  outbound link and third-party request hands the unguessable URL to another
  site in the `Referer` header.
- Inline all CSS, JavaScript, and SVG. Load nothing from a CDN, font service,
  or analytics host: each one is a third party learning the URL, and a page
  that breaks when that host is unreachable. Embed small images as `data:`
  URIs, or upload them next to the page and link them relatively.
- Support light and dark with `color-scheme` and `prefers-color-scheme`, and
  let wide tables scroll horizontally inside their own container instead of
  stretching the page.
- Give the page a title and a one-line statement of what it shows and when it
  was generated. The user will find this tab again later with no chat context
  around it.

## Publish

Every page goes in its own directory named with 128 bits of randomness. That
name is the page's only protection, so never shorten it, derive it from the
content, or replace it with something readable. The MIME type comes from the
file extension, so keep `.html`.

```bash
. /workspace/.claude-sandbox/stathost/secrets
id=$(openssl rand -hex 16)
dir=/workspace/.claude-sandbox/stathost/pages/$id
mkdir -p "$dir"
# ... write "$dir/report.html" ...
url="${STATHOST_HOST%/}/$STATHOST_BUCKET/$id/report.html"
printf 'header = "Authorization: Bearer %s"\n' "$STATHOST_TOKEN" |
  curl -sS --fail -K - -T "$dir/report.html" -o /dev/null -w '%{http_code}\n' "$url"
curl -sS -o /dev/null -w '%{http_code} %{content_type}\n' "$url"
echo "$url"
```

The upload prints `201`, and the fetch that follows prints `200 text/html` —
proof that the page is really served before the link goes to the user. A `401`
or `403` means the token or bucket in the secrets file is wrong; report it
instead of retrying.

Link the file itself. A directory URL serves `index.html` only with its
trailing slash, and that slash is easy to lose when a link is copied.

Look at the page before you send it when the layout is the point — open the
URL in whatever browser tool the session has and check that it renders the
way you meant.

## Hand over the link

Put the URL in the reply as a Markdown link or a bare URL on its own line, so
the user can click it:

```markdown
[Dependency upgrade matrix](https://static.example.com/my-bucket/3f9c…/report.html)
```

Do not wrap the real link in a code block or backticks — that makes it text to
copy rather than a link to click. Add a sentence or two in chat with the
conclusion, so the answer does not live only behind the link, and mention that
the page is public to anyone holding the URL.

## Revise and remove

To change a page, edit the local file and upload it to the **same** URL. The
link the user already has keeps working; tell them to reload. Uploads are
atomic, so a reader never sees a half-written page.

Delete a page when the user asks, or when it should not have been published:

```bash
. /workspace/.claude-sandbox/stathost/secrets
printf 'header = "Authorization: Bearer %s"\n' "$STATHOST_TOKEN" |
  curl -sS --fail -K - -X DELETE "${STATHOST_HOST%/}/$STATHOST_BUCKET/$id/report.html"
```

It returns `204`, and the URL answers `404` from then on. Delete each file in
the directory separately; there is no recursive delete.

The bucket is shared with other sessions and whatever else the user keeps
there. Write only inside directories you created, never at the bucket root,
and never delete or overwrite a path you did not publish. `GET
…/<bucket>/_meta/list` with the same header lists every file in the bucket —
use it to find your own pages again, not to tidy up someone else's.
