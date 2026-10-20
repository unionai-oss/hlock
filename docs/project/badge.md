# Badge

If your repository commits a `harness.lock`, you can add a badge to its README to show that
it pins its coding agent setup. The badge links to the lock, so readers can see which
harness, models, MCP servers, and skills the project uses, and pull the setup with
[`hlock pull`](../guides/pull.md).

[![hlock](https://img.shields.io/endpoint?url=https://docs.hlock.ai/badge.json)](https://docs.hlock.ai/)

The badge is a [shields.io endpoint badge](https://shields.io/badges/endpoint-badge): shields.io
reads its label, color, and logo from [`badge.json`](https://docs.hlock.ai/badge.json) on this
site, so the snippet stays short.

## Add it to your README

Replace `OWNER/REPO` with your repository, and `main` with your default branch if it's
different. For a repository with one `harness.lock` at its root, link the badge to that file:

```markdown
[![hlock](https://img.shields.io/endpoint?url=https://docs.hlock.ai/badge.json)](https://github.com/OWNER/REPO/blob/main/harness.lock)
```

If contributors keep their own locks in `.hlock/locks/` (see
[Team repositories](../guides/teams.md)), link to the directory instead, so readers see
every setup:

```markdown
[![hlock](https://img.shields.io/endpoint?url=https://docs.hlock.ai/badge.json)](https://github.com/OWNER/REPO/tree/main/.hlock/locks)
```

On GitLab, use `https://gitlab.com/OWNER/REPO/-/blob/main/harness.lock` or
`https://gitlab.com/OWNER/REPO/-/tree/main/.hlock/locks`.

In an HTML README header, such as a centered block, use the same URLs in an `<a>`:

```html
<a href="https://github.com/OWNER/REPO/blob/main/harness.lock"><img alt="hlock" src="https://img.shields.io/endpoint?url=https://docs.hlock.ai/badge.json"></a>
```

## Customize it

Shields.io's query parameters work on the endpoint badge. Add `&style=flat-square` or
`&style=for-the-badge` to match your other badges, or `&label=agent` to change the left side:

```markdown
[![hlock](https://img.shields.io/endpoint?url=https://docs.hlock.ai/badge.json&style=flat-square)](https://github.com/OWNER/REPO/blob/main/harness.lock)
```

The message comes from `badge.json`. To change it, for example to name the harness you pin,
use the static form below.

## Without the docs site

The endpoint badge depends on docs.hlock.ai being up. The static form doesn't: it embeds the
logo in the URL, so it's long, but shields.io renders it on its own. Change `locked` to set
the message, writing a literal `-` as `--` and a space as `_` (`hlock-claude--code-7C3AED`
renders "hlock | claude-code"):

```markdown
[![hlock](https://img.shields.io/badge/hlock-locked-7C3AED?logo=data:image/svg%2bxml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCA2NCA2NCI+PHBhdGggZD0iTTIxIDMwVjIwLjVhMTEgMTEgMCAwIDEgMjIgMFYzMCIgZmlsbD0ibm9uZSIgc3Ryb2tlPSIjZmZmIiBzdHJva2Utd2lkdGg9IjUiLz48cmVjdCB4PSIxMCIgeT0iMjciIHdpZHRoPSI0NCIgaGVpZ2h0PSIzMSIgcng9IjYiIGZpbGw9IiNmZmYiLz48ZyBmaWxsPSIjN0MzQUVEIj48cmVjdCB4PSIxNyIgeT0iMzQuOCIgd2lkdGg9IjMwIiBoZWlnaHQ9IjIuNCIgcng9IjEuMiIvPjxyZWN0IHg9IjE3IiB5PSI0MS44IiB3aWR0aD0iMzAiIGhlaWdodD0iMi40IiByeD0iMS4yIi8+PHJlY3QgeD0iMTciIHk9IjQ4LjgiIHdpZHRoPSIzMCIgaGVpZ2h0PSIyLjQiIHJ4PSIxLjIiLz48Y2lyY2xlIGN4PSIzOSIgY3k9IjM2IiByPSIzLjMiLz48Y2lyY2xlIGN4PSIyMyIgY3k9IjQzIiByPSIzLjMiLz48Y2lyY2xlIGN4PSIzMyIgY3k9IjUwIiByPSIzLjMiLz48L2c+PC9zdmc+Cg==)](https://github.com/OWNER/REPO/blob/main/harness.lock)
```

The `logo` parameter is [`docs/assets/badge-logo.svg`](https://github.com/unionai-oss/hlock/blob/main/docs/assets/badge-logo.svg),
base64-encoded, and `badge.json` embeds the same SVG. To rebuild the static URL after
changing the SVG:

```bash
base64 < docs/assets/badge-logo.svg | tr -d '\n'
```
