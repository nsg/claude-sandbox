---
name: readme
description: README writing and maintenance guidelines
---

# README Management

Guidelines for writing and maintaining project README files. Always read the existing README before making changes.

**Use subagents (Task tool) extensively.** Spawn parallel subagents to gather context — e.g., one to read the existing README, one to explore source code for features and CLI flags, one to check for a Zola docs site, one to inspect configuration options. Collect all findings before writing.

## Structure

Every README should follow this order. Omit sections that don't apply, but keep the order consistent.

1. **Header** — Centered logo/image, project name, one-line description
2. **About** — 1-2 paragraphs expanding on what the project does and why it exists
3. **Features** — Bullet list of key capabilities
4. **Quick Start** — Minimal steps to install and run (copy-pasteable commands)
5. **Configuration** — Options, settings, environment variables
6. **Usage / API / CLI** — Detailed reference with examples
7. **Documentation** — Link to Zola docs site if one exists
8. **License** — Short line naming the license

## Header Format

Always include a centered header block. Use a wide banner/header image when available (recommended ~1200x300px or similar wide aspect ratio):

```markdown
<div align="center">
  <img src="path/to/header.png" alt="Project Name">
  <p>Short one-line description of the project.</p>
</div>
```

For a logo instead of a banner, constrain the width:

```markdown
<div align="center">
  <img src="path/to/logo.png" alt="Project Name" width="300">
  <p>Short one-line description of the project.</p>
</div>
```

If no image exists yet, use just the project name as an `<h1>` inside the centered div.

## Writing Style

- Be direct and concise — no filler or marketing language
- Use imperative mood for instructions ("Install the package", not "You should install the package")
- Lead with practical value — what does this do, how do I use it
- Use tables for structured data (components, parameters, comparisons)
- Use code blocks with language tags for all commands and config examples
- Keep examples copy-pasteable and realistic
- Use horizontal rules (`---`) to separate major sections when it improves readability
- Use emojis sparingly — only when they add clarity (e.g., a section header), never decoratively. When in doubt, leave them out

## README vs External Docs

Keep content in the README when:
- It fits in a single scroll for a developer evaluating the project
- It covers installation, basic usage, and configuration

Move content to an external docs site when:
- API reference or CLI docs exceed ~100 lines
- Multiple tutorials or guides are needed
- The project has distinct user roles (admin, developer, end-user)

Check for a Zola project in the repo (look for `config.toml` with `[markdown]` or a `content/` directory with `.md` files). The Zola site is the external docs site. If found, read its content for context — it may contain detailed information useful for writing or updating the README.

When a Zola docs site exists in the repo:
- Read its content for context regardless of whether it's published
- If the site is published at a URL, keep the README short — header, about, quick start, link to docs. Use a clear "Documentation" section: `Full documentation at [project.example.com](https://...)`
- If the site is not published, use its content as source material but put the relevant information directly in the README
- Don't duplicate content between README and a published docs site

## Avoid

- Changelogs — belong in CHANGELOG.md or GitHub releases, not the README
- Contributing guidelines — use CONTRIBUTING.md if needed
- Build matrix tables or CI setup details
- Acknowledgments or "powered by" sections
- Verbose dependency lists
- Repetition of information available in the docs site

## Maintenance Rules

- Update the README when features, CLI flags, or configuration options change
- Keep code examples working — if an API changes, update the examples
- Don't add a section for something that doesn't exist yet
- Remove sections for features that have been dropped
