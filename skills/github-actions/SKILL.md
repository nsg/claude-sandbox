---
name: github-actions
description: GitHub Actions workflow development with official actions preference
---

# GitHub Actions Development

## Action Selection Policy

### Official Actions Only (Default)

**Always prefer official GitHub Actions** from these trusted sources:

- `actions/*` - Official GitHub-maintained actions (checkout, setup-node, cache, upload-artifact, etc.)
- `github/*` - GitHub's organizational actions
- `docker/*` - Official Docker actions

### Third-Party Actions

If an official action is not available for the required functionality:

1. **ALWAYS ask the user for approval before using any third-party action**
2. **Provide alternatives** - Always offer a manual implementation option (shell commands, scripts) alongside the third-party suggestion
3. **Justify the choice** - Explain why this specific action is recommended:
   - Number of stars/downloads
   - Maintainer reputation
   - Last update date
   - Security considerations
4. **Pin to specific versions** - Never use `@latest` or `@main`, always use a specific commit SHA or version tag

### Presenting Third-Party Action Choices

When a third-party action is needed, present options in this format:

```
For [functionality], you have these options:

Option 1 (Manual): [shell commands/script approach]
- Pros: Full control, no external dependencies
- Cons: [any downsides]

Option 2 (Third-party): [action/name@version]
- Maintainer: [who maintains it]
- Stars: [approximate count]
- Last updated: [date]
- Why this one: [justification]
- Cons: [any concerns]

Which approach would you prefer?
```

## Security Best Practices

- **Pin action versions** - Use commit SHAs for critical workflows: `uses: actions/checkout@a5ac7e51b41094c92402da3b24376905380afc29`
- **Minimal permissions** - Always specify the minimum required permissions
- **Review action source** - For third-party actions, review the action's repository
- **Avoid secrets in logs** - Use `::add-mask::` for sensitive values

## Workflow Structure

```yaml
name: Descriptive Workflow Name

on:
  # Be specific about triggers
  push:
    branches: [main]
  pull_request:
    branches: [main]

permissions:
  # Always specify minimal permissions
  contents: read

jobs:
  job-name:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4  # Official action - OK
      # ... rest of workflow
```

## Common Official Actions Reference

| Purpose | Official Action |
|---------|-----------------|
| Checkout code | `actions/checkout@v4` |
| Setup Node.js | `actions/setup-node@v4` |
| Setup Python | `actions/setup-python@v5` |
| Setup Go | `actions/setup-go@v5` |
| Setup Java | `actions/setup-java@v4` |
| Setup Rust | `actions-rust-lang/setup-rust-toolchain@v1` |
| Caching | `actions/cache@v4` |
| Upload artifacts | `actions/upload-artifact@v4` |
| Download artifacts | `actions/download-artifact@v4` |
| Create release | `actions/create-release@v1` (deprecated) or manual `gh` CLI |
| GitHub Pages | `actions/deploy-pages@v4` |
| GitHub Script | `actions/github-script@v7` |

## Manual Alternatives

Many tasks can be done without third-party actions:

### Release creation
```yaml
- run: gh release create ${{ github.ref_name }} --generate-notes
  env:
    GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

### Docker build and push
```yaml
- run: |
    docker build -t $IMAGE_NAME .
    docker push $IMAGE_NAME
```

### Notifications
```yaml
- run: |
    curl -X POST -H 'Content-type: application/json' \
      --data '{"text":"Build completed"}' \
      ${{ secrets.SLACK_WEBHOOK }}
```

## Critical Rules

- **NEVER use third-party actions without explicit user approval**
- **ALWAYS provide a manual alternative when suggesting third-party actions**
- **ALWAYS pin versions** - No `@latest`, `@main`, or `@master`
- **ALWAYS specify permissions** - Never rely on default (overly broad) permissions
- **Review before suggesting** - Verify the action is actively maintained and reputable
