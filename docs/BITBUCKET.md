# Bitbucket

tuicr reviews Bitbucket Cloud pull requests the same way it reviews GitHub pull
requests and GitLab merge requests. Unlike the GitHub (`gh`) and GitLab (`glab`)
integrations, Bitbucket has no ubiquitous official CLI, so tuicr talks to the
[Bitbucket Cloud REST API 2.0](https://developer.atlassian.com/cloud/bitbucket/rest/)
directly and needs a token of its own.

> [!NOTE]
> Only Bitbucket **Cloud** (`bitbucket.org`) is supported. Bitbucket Server /
> Data Center uses a different API and is not yet supported.

## Setup

Provide credentials one of two ways. Environment variables take precedence over
the config file.

### Option 1 — an access / API token (Bearer)

Create an [access token](https://support.atlassian.com/bitbucket-cloud/docs/access-tokens/)
(repository, project, or workspace) or an Atlassian API token with pull request
scopes, then:

```bash
export BITBUCKET_TOKEN="your-token"
```

### Option 2 — username + app password (Basic)

Create an [app password](https://support.atlassian.com/bitbucket-cloud/docs/app-passwords/)
with **Pull requests: read and write** (and **Repositories: read**) scopes, then:

```bash
export BITBUCKET_USERNAME="your-bitbucket-username"
export BITBUCKET_APP_PASSWORD="your-app-password"
```

`BITBUCKET_API_TOKEN` is accepted as a synonym for `BITBUCKET_APP_PASSWORD`.

### Config file

Instead of environment variables, add credentials to
`~/.config/tuicr/config.toml`:

```toml
[forge.bitbucket]
username = "your-bitbucket-username"   # omit to send `token` as a Bearer token
token = "your-app-password-or-token"
```

tuicr submits as whatever account the token belongs to. It stores no
credentials of its own beyond what you put in these variables or this file.

## Open a pull request

```bash
tuicr pr 125
```

`mr` is an alias for `pr`, so `tuicr pr 125`, `tuicr mr 125`, and
`tuicr tui pr 125` all open the same review. The target accepts several forms:

| Target | Example |
|--------|---------|
| PR id | `125` |
| PR URL | `https://bitbucket.org/workspace/repo/pull-requests/125` |
| `workspace/repo#id` | `myworkspace/myrepo#125` |

tuicr detects the forge from the repository's remotes. A remote routes to
Bitbucket when its host is `bitbucket.org` (`altssh.bitbucket.org`, used for SSH
over port 443, is mapped back to `bitbucket.org`).

## Submit a review

`:submit` opens a picker. On Bitbucket it offers three events:

| Event | Result |
|-------|--------|
| Comment | Posts your inline and review-level comments without changing approval state. |
| Approve | Posts your comments and approves the pull request. |
| Request changes | Posts your comments and marks the pull request as changes requested. |

Inline comments land on their lines as inline PR comments. Review-level comments
post as a general (non-inline) PR comment. Bitbucket inline comments are
single-line; a multi-line selection anchors to its end line.

`:submit draft` remains GitHub-only. Bitbucket has no pending-review primitive,
so running it against a Bitbucket PR returns an unsupported-operation error
rather than submitting.

## Limitations and troubleshooting

Reviewing a commit range within a PR requires a local checkout of the branch. A
remote-only commit-range diff returns a "not yet supported" error, matching the
GitLab backend.

Common errors:

| Message | Fix |
|---------|-----|
| `Bitbucket integration requires credentials.` | Set `BITBUCKET_TOKEN` (or `BITBUCKET_USERNAME` + `BITBUCKET_APP_PASSWORD`), or add a `[forge.bitbucket]` section to your config. |
| `Bitbucket authentication failed (HTTP 401/403)` | The token is missing, expired, or lacks pull-request scopes. Recreate it with read/write access to pull requests. |
| `Bitbucket API error (HTTP 404)` | The workspace/repo or PR id is wrong, or the token cannot see a private repository. |
