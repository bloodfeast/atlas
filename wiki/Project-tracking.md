Project tracking: atlas can read Jira and Confluence directly over their REST APIs, with an API token you provide in
an environment variable. atlas never holds the token: the configuration names the variable, atlas reads it when a
request is made, keeps it in memory for that one command, and never writes it anywhere (store, configuration, logs,
handles, errors, hook output or tool answers). The reads are digested at the source, the same way large tool output
is (see [[Handles and digests]]): you get a short digest, and the full response is kept as a handle for 7 days.

The read client is read-only by construction: it sends `GET` and nothing else, to a fixed list of read endpoints (the
JQL and CQL searches are `GET`s too). Four writes live in a separate module with their own endpoint list: a Jira
comment, a transition, a field edit (assignee, labels, sprint) and a Confluence page update (see [Writes](#writes)).
They are off until you give them a write token of their own and a per-project allow-list, and every one is a dry-run
until `--apply`. Creating and deleting issues or pages stay with the Atlassian MCP connector.

## Setup

### Atlassian Cloud (an API token)

1. Create an API token at `https://id.atlassian.com/manage-profile/security/api-tokens`. A token with read scopes only
   is the safest choice (see [Token scopes](#token-scopes) for what to tick); `atlas track check` shows what the
   account itself could do.
2. Put the account email and the token in two environment variables, for example in your shell profile or your
   harness's secret store:

   ```sh
   export ATLASSIAN_EMAIL="you@example.com"
   export ATLASSIAN_API_TOKEN="..."      # never in atlas.json
   ```

3. Name the site and the variables in `atlas.json`:

   ```json
   {"project_tracking": {"atlassian": {
     "site": "https://example.atlassian.net",
     "email_env": "ATLASSIAN_EMAIL",
     "token_env": "ATLASSIAN_API_TOKEN"
   }}}
   ```

Confluence is read at `<site>/wiki` with the same credential, unless you give it its own (below).

### A scoped token: the gateway, and one token per product

A scoped token (Atlassian's "Create API token with scopes") holds one product's scopes, Jira or Confluence, and
Atlassian accepts it only through its API gateway, never on the `*.atlassian.net` site URL. So with scoped tokens:

- set `site` to the Jira gateway, `https://api.atlassian.com/ex/jira/<cloud id>`, and `confluence_site` to the
  Confluence one, `https://api.atlassian.com/ex/confluence/<cloud id>/wiki`. The cloud id is the `cloudId` at
  `https://<your-site>.atlassian.net/_edge/tenant_info`;
- create a second token for Confluence and name its variable in `confluence_token_env`. Confluence requests then
  carry that token, and Jira requests keep `token_env`'s. The email is `email_env`'s unless `confluence_email_env`
  names another variable.

```json
{"project_tracking": {"atlassian": {
  "site": "https://api.atlassian.com/ex/jira/11111111-2222-3333-4444-555555555555",
  "confluence_site": "https://api.atlassian.com/ex/confluence/11111111-2222-3333-4444-555555555555/wiki",
  "email_env": "ATLASSIAN_EMAIL",
  "token_env": "ATLASSIAN_JIRA_TOKEN",
  "confluence_token_env": "ATLASSIAN_CONFLUENCE_TOKEN"
}}}
```

`check` helps when this goes wrong: a 401 from a scoped token on a `*.atlassian.net` site prints the gateway URLs to
use (for Jira, both `site` and `confluence_site`). A Jira gateway `site` with no `confluence_site` leaves Confluence
unconfigured: `check` says "confluence: not tried" and a Confluence read is refused with the same message, since
`<site>/wiki` would be Jira's gateway; Jira reads work as before. A site that answers `200` with `x-seraph-loginreason: AUTHENTICATED_FAILED` (it refused the login and
answered as an anonymous user, so a search comes back empty; Atlassian's JRACLOUD-96181) is reported as an auth
failure, not as an empty result.

### Data Center or Server (a personal access token)

Create a personal access token in your profile (Jira and Confluence each have their own), put it in a variable, and
name it with `pat_env`; it is sent as a bearer token. When Confluence runs on its own host, give it as
`confluence_site`:

```json
{"project_tracking": {"atlassian": {
  "site": "https://jira.example.com",
  "confluence_site": "https://wiki.example.com",
  "pat_env": "ATLASSIAN_PAT"
}}}
```

### Secret managers and ARNs

Resolving a secret manager reference is the harness's job, not atlas's: whatever starts Claude Code (or atlas) reads
the secret and sets the variable. For example, with an AWS Secrets Manager ARN:

```sh
export ATLASSIAN_API_TOKEN="$(aws secretsmanager get-secret-value \
  --secret-id arn:aws:secretsmanager:us-east-1:123456789012:secret:atlassian-token-AbCdEf \
  --query SecretString --output text)"
```

`op read`, `vault kv get` and the like work the same way. atlas only ever sees the variable.

### What the configuration accepts

`email_env`, `token_env`, `confluence_email_env`, `confluence_token_env` and `pat_env` must be variable names: upper
case letters, digits and `_`, at most 64 characters. A token pasted there is refused at load, and the error does not
repeat the value. Give `pat_env`, or `email_env` and `token_env`, not both; `confluence_token_env` is for Cloud only.
`site` must be `https` with no user, query or fragment. Every key is on [[Configuration reference]].

## Token scopes

Pick the scopes when you create a scoped token; one token holds one product's scopes, so Jira and Confluence each get
their own. Only the gateway accepts a scoped token. Data Center and Server personal access tokens have no scopes: they
act as the user, so use a low-permission account to limit them.

Jira (classic scopes recommended: 2 against 14 granular):

| Endpoint atlas calls | Classic | Granular |
|---|---|---|
| `GET /rest/api/3/myself` | `read:jira-user` | `read:application-role:jira`, `read:group:jira`, `read:user:jira`, `read:avatar:jira` |
| `GET /rest/api/3/mypermissions` | `read:jira-work` | `read:permission:jira` |
| `GET /rest/api/3/issue/{key}` | `read:jira-work` | `read:issue-meta:jira`, `read:issue-security-level:jira`, `read:issue.vote:jira`, `read:issue.changelog:jira`, `read:avatar:jira`, `read:issue:jira`, `read:status:jira`, `read:user:jira`, `read:field-configuration:jira` |
| `GET /rest/api/3/search/jql` | `read:jira-work` | `read:issue-details:jira`, `read:audit-log:jira`, `read:avatar:jira`, `read:field-configuration:jira`, `read:issue-meta:jira` |

Tick: `read:jira-work` and `read:jira-user`.

Confluence (granular scopes recommended: 2; the v2 pages endpoint has no classic scope):

| Endpoint atlas calls | Classic | Granular |
|---|---|---|
| `GET /wiki/api/v2/pages/{id}` | none | `read:page:confluence` |
| `GET /wiki/rest/api/search` (CQL) | `search:confluence` | `read:content-details:confluence` |
| `GET /wiki/rest/api/user/current` | `read:confluence-user` | `read:content-details:confluence` |

Tick: `read:page:confluence` and `read:content-details:confluence`. One token can hold classic and granular scopes
together, so if a CQL search with the granular scope alone comes back thin, add `search:confluence` and
`read:confluence-user`.

Reads need nothing more. For writes, create a second token per product with the write scopes below, and name its
variable in `jira_write_token_env` / `confluence_write_token_env`; fine-grained tokens are recommended at setup, so a
leaked write token reaches only what the scopes list. The write token is used for the write request alone; the pre-reads
(current status, transitions, the page's version) use the read token.

| Write | Jira classic | Confluence granular |
|---|---|---|
| comment, transition, edit | `write:jira-work` and `read:jira-work` (to read the status, transitions and field list), `read:jira-user` (assignee `me` or a name) | |
| page update | | `read:page:confluence`, `write:page:confluence`, and `read:space:confluence` when the allow-list names a space by key rather than by id |

Tick for Jira writes: `write:jira-work`, `read:jira-work`, `read:jira-user`. Tick for Confluence writes:
`read:page:confluence`, `write:page:confluence` (plus `read:space:confluence` for a keyed allow-list). On Data Center
and Server a personal access token has no scopes: make it a low-permission user's, with only the projects and spaces
you list in `writes`.

### Not yet verified on a real site

- `user/current?expand=operations` may need more than the scope listed.
- A CQL search with the granular `read:content-details:confluence` alone may return hits without some fields.
- The v2 page body (`body-format=storage`) is assumed covered by `read:page:confluence`.
- The granular names are marked beta by Atlassian and have been renamed before.
- The write calls (the ADF comment body, the transition and edit payloads, the v2 page PUT with `version.number`, a
  sprint set through the field list) are built from the documented shapes and tested against fixtures and a local
  socket server only; none has been run against a real site. Try the dry-run, then one `--apply` on a test issue and
  page, and report what the site says.
- Whether the write scopes above are sufficient, and the exact granular names, are unconfirmed in the same way.

## Commands

```text
atlas track check
atlas track smoke [--issue KEY] [--page ID] [--jql QUERY] [--cql QUERY]
atlas track issue KEY
atlas track jql QUERY [--limit N] [--fields LIST|*all]
atlas track page ID
atlas track cql QUERY [--limit N]
atlas track snapshot ITEM [--status S --assignee A --summary T [--category C]]
atlas track snapshot --all [--older-than DUR]
atlas track comment KEY (--body TEXT | --body-file F) [--again] [--apply] [--lane L]
atlas track transition KEY --to NAME|ID [--apply] [--lane L]
atlas track edit KEY [--assignee me|none|ACCOUNT_ID|QUERY] [--labels-add a,b] [--labels-remove c]
                 [--sprint ID|none] [--apply] [--lane L]
atlas track page-update ID (--section HEADING [--append] --body-file F | --body-file F | --append --body-file F)
                 [--title T] [--expect-version N] [--message M] [--apply] [--lane L]
atlas queue plan --from-jql QUERY [--kind K] [--priority P] [--uses LIST] [--after LIST] [--scope TEXT]
                 [--order-from N] [--lane-from key|fixed:LANE] [--limit N]
```

- `check` verifies the configuration, says whether each variable is present (present or absent, never its value),
  makes one whoami call per product, and reports the write permissions the account holds (Jira's `mypermissions` for
  creating, editing, transitioning, commenting and deleting; Confluence's operations), so you can confirm a
  read-only setup. After a failed login the write line says "unknown (auth failed)": an anonymous answer says
  nothing about the account. Any Confluence operation verb atlas does not know as a read counts as a possible write,
  so an unknown verb never makes a token look read-only. A scoped token's scopes are not visible to the API: a
  read-only scoped token still shows the account's permissions, and its writes would fail with 401 or 403. atlas
  sends `GET` only either way.
- `smoke` runs the reads you name and prints only shapes and sizes: field names, counts, pages, bytes raw against
  digest, approximate tokens and timings. It prints no issue key, summary, name, title or body, so its output can be
  pasted into an issue or a chat without work data.
- `issue`, `jql`, `page` and `cql` print the digest and keep the response as a handle (`recall h:ID lines=A-B`,
  `grep=RE`; `atlas slice h:ID ...` from a shell, which takes either form; in a terminal the hint is printed as
  that command). A search's `digest-grep=KEY` finds an issue's or hit's digest row past the first 30. A Jira issue's digest is its key, type, status, priority,
  assignee, reporter, dates, labels, components, fix versions, parent and sprint, the summary, the description's
  opening and heading outline, the comment count with the last two, links, attachments and the names of the custom
  fields that are set. A JQL search is one row per issue; a Confluence page is its metadata and the body as markdown
  with a heading outline (line ranges point into the handle); a CQL search is one row per hit. Anything else gets a
  JSON outline.
- `snapshot` writes a queue item's ticket snapshot (below).

Searches follow the API's cursors (`nextPageToken` for JQL on Cloud, `startAt` on Data Center, `_links.next` for CQL)
up to `--limit` items (default 50, at most 1000). A next link is followed only when it is a search path on the
configured site.

### What a handle holds

- **An issue handle is the full record**: the response to `GET /rest/api/3/issue/KEY` (every field, every comment,
  the description as Atlassian Document Format, links and attachments), pretty-printed. Only the auth material is
  redacted (the `Authorization` header value, the token and its base64 forms, should a proxy echo them); the
  account's own email passes through like any other user's.
- **A JQL handle is the full response, not the full issues**: each issue holds only the fields the search asked for,
  by default the six its rows show (`summary, status, issuetype, priority, assignee, updated`). Ask for more with
  `--fields description,labels,parent` or every field with `--fields '*all'`.
- A page handle is the v2 page with its storage body; a CQL handle is the search response.
- An issue or page handle also holds the description or body rendered as markdown, after the JSON. Each part is an
  item: `item=json` is the response alone (it parses as JSON), `item=description-md` or `item=body-md` the markdown.
- A slice is capped at 8 KB. To take a whole handle, or one item, into a file uncapped and without line numbers
  (an existing file is replaced, and the confirmation line says "replaced"; a FILE starting with `-` is refused):

  ```sh
  atlas slice h:ID --item json --out issue.json
  atlas slice h:ID --item description-md --out description.md
  atlas slice h:ID --out whole.txt
  ```

### Not yet verified on a real site

These pieces were built from Atlassian's public docs and tested against recorded fixtures, never against a live site.
If one misbehaves it is more likely atlas than your token: run the command again with `ATLAS_TRACK_DEBUG=1` and paste
back what is listed (the debug lines already show the header as `[redacted]`; remove anything else you would rather
not share).

- **Confluence write probe** (`GET /wiki/rest/api/user/current?expand=operations`, Cloud and Data Center). `check`
  names create, update, delete, administer, archive, restore, move, copy and export as writes, counts anything but
  read and `use application` as a possible write, and prints `use application` as "other". If `check` says "reads only" for an account that
  can edit, or "WOULD be allowed" for one that cannot, paste back the `confluence writes:` and `confluence other
  operations:` lines and the `operation`/`targetType` pairs from the response (`curl` it yourself; leave out the
  account fields).
- **Confluence through the gateway** (`https://api.atlassian.com/ex/confluence/<cloud id>/wiki`; Jira through its
  gateway is verified). If `check` answers 404 or 401 for Confluence there, paste back the `check` output and the
  debug request lines, and say whether `confluence_site` ended in `/wiki`.
- **The `x-seraph-loginreason` check** (JRACLOUD-96181). If a read through the site URL comes back empty with no
  error, paste back the debug request lines and the response's status. Only `AUTHENTICATED_FAILED` on a `200` is
  handled; `AUTHENTICATION_DENIED` (a CAPTCHA after repeated failed logins, usually a `403` on Data Center) is not
  yet, so `check` may still read permissions after it; a browser login clears the CAPTCHA.
- **Jira Data Center `mypermissions?permissions=`.** If `check` prints `jira writes: not known` or an HTTP error on
  Data Center, paste back that line and the Jira version (Administration, System info).
- **Cloud CQL `_links.next`.** A next link is followed only when it starts with the search path
  (`/wiki/rest/api/search?`), relative to the configured site; a gateway base or a site that answers with another
  form stops after one page. If `atlas track cql QUERY --limit 200` stops at the first page when the site has more
  hits, paste back the `smoke --cql QUERY` line (pages and counts) and the debug request lines.

## Ticket snapshots on queue items

A queue item with a `ticket: KEY` meta line (`atlas queue plan ... --ticket PROJ-123`, or a `ticket:` line in the
`journal` tool's meta) carries a snapshot of its ticket: `ticket_status`, `ticket_assignee`, `ticket_summary`,
`ticket_category` (the status category: To Do, In Progress or Done), `ticket_fetched` and `ticket_via`. With a token
configured and present, `atlas track snapshot ITEM` reads the issue directly (`ticket_via: rest`). Without one it
prints the fallback, and the agent-finished notice says the same for a waiting item with no snapshot: fetch the ticket
with the Atlassian MCP connector (`getJiraIssue`) and record it with
`atlas track snapshot ITEM --status S --assignee A --summary T [--category C]` (`ticket_via: mcp`). Hooks never make a
network call.

### Refreshing snapshots

A snapshot is as old as its `ticket_fetched` line; nothing refreshes it on its own. `atlas track snapshot --all`
fetches every open item's ticket again (an open item waits, or started and is not yet marked done), one `GET` per
ticket, written to every open item that tracks it. It also fetches each ticket that a waiting item's unmet
`ticket KEY STATUS` condition names, even when only a done or dropped item tracks it, and writes it to the newest such
item, so the condition can still move. `--older-than DUR` (`30m`, `6h`, `2d`, `1w`, unit in any case) fetches only
those whose snapshot is older than that, and an item with no snapshot yet. A fetch or a store write that fails keeps
the item's old snapshot, prints the item and the error, and makes the exit code 1; the last line counts refreshed,
failed and fresh-enough items. `--all` takes no ITEM, and `--older-than` goes only with `--all`.

### Waiting on a ticket

An `--after` condition of the form `ticket KEY STATUS` waits on a ticket's status: `ticket ES1-1557 Done`,
`ticket ES1-1557 In Progress` or `ticket ES1-1557 Ready For Dev`. STATUS is a status name or a status category, case
ignored. The condition is met when the latest snapshot of KEY (the newest `ticket_fetched` across the queue items
that track it) matches, with no `atlas queue met`; refresh the snapshots and the waiting items move on their own. A
KEY that no queue item tracks shows as "no queue item tracks this ticket", since no snapshot of it can be taken: plan
an item with `--ticket KEY` first. Free-text conditions are unchanged.

## Queue from JQL

`atlas queue plan --from-jql QUERY` turns a search into queue items in one command, one per issue:

```sh
atlas queue plan --from-jql 'sprint in openSprints() AND assignee = currentUser() AND statusCategory = "To Do"' --order-from 10
```

- Each item's lane and `ticket:` line are the issue key, and its title is the summary. `--lane-from fixed:LANE` puts
  every item on one lane instead.
- The snapshot comes from the same search response (status, its category, assignee, summary), so no second request
  is made per issue.
- An issue that already has an open item (waiting, or started and not yet marked done) is skipped, so the same query
  can be run again each morning to pick up only the new tickets.
- `--kind` (default builder), `--priority`, `--uses`, `--after` and `--scope` apply to every item; `--order-from N`
  numbers the new items N, N+1, and so on in the search's order. `--lane`, `--title`, `--ticket` and `--order` are set
  per issue and refused here.
- `--limit N` caps the issues read (default 50, at most 1000, at least 1), following the search's pages. Skipped
  issues count toward it: once the first 50 matches all have open items, a re-run plans nothing and says the search
  may hold more. For the morning re-run, write a JQL that leaves out planned work (a status or label the planned
  tickets no longer match), or raise `--limit`.
- One line per issue, then a summary: `from-jql: 2 planned, 1 skipped (an open item tracks them), 0 failed`. The exit
  code is 1 when an item could not be filed. It needs the direct read; without a token it prints the per-issue
  fallback and exits 2.

## Writes

Four writes, each the same through the command line and the MCP tool `track_write` (`verb` is `comment`,
`transition`, `edit` or `page_update`; the other flags are fields of the same names; `apply` is false unless set):
a Jira comment, a transition, an edit of assignee, labels and sprint, and a Confluence page update. Nothing creates or
deletes.

`track_write` is a tool of its own, marked as a write (no `readOnlyHint`), so a client asks before it runs unless its
permissions allow `track_write` by name; `note` stays read only. A client that allows `track_write` without asking lets
an agent send with `apply: true`, and then the `writes` allow-list below is the only gate: allow it per project only
where that is what you want.

Turn them on with a write token of its own and an allow-list (nothing is allowed by default):

```json
{"project_tracking": {"atlassian": {
  "jira_write_token_env": "ATLASSIAN_WRITE_TOKEN",
  "confluence_write_token_env": "CONFLUENCE_WRITE_TOKEN",
  "writes": {
    "jira": {"PROJ": ["comment", "transition", "assignee", "labels", "sprint"]},
    "confluence": {"4456": ["page_update"]},
    "journal_lane": "PROJECT-TRACKING"
  }
}}}
```

Data Center and Server use `pat_write_env` instead. The write variable may name the same variable as the read one, but
a separate, narrowly scoped token is the recommendation: atlas reads the write variable only when `--apply` sends, so
a read-only setup never touches it. A set write variable that is empty or unset is named in the refusal, never its value.

- **Allow-list.** A verb on a project (Jira) or space (Confluence) outside `writes` is refused with nothing sent, and an
  `edit` naming one disallowed field (say `labels` without `labels` allowed) sends nothing at all. A Jira key's prefix
  is not trusted: a moved issue keeps answering to its old key, so before every Jira write atlas reads the issue once
  (`GET issue/{key}?fields=project,status`, read token) and checks the list against `fields.project.key`; a key whose
  typed prefix is not listed is refused before that read. The dry-run shows the project it read. A Confluence
  allow-list entry is a space id; a space key also works and costs one read (`GET /api/v2/spaces/{id}`, scope
  `read:space:confluence`).
- **Dry-run first.** Without `--apply` atlas reads what it needs, prints the exact request (method, URL, body, header
  redacted) and sends nothing. The output of `--apply` is one line, the outcome and the journal key.
- **Journaled.** Every applied write is journaled once, to `writes.journal_lane` (default `PROJECT-TRACKING`, or
  `--lane`), with the target and what changed, never the body.
- **Idempotent.** A comment identical to one sent in the last ten minutes is refused unless `--again`. A transition to the
  status the issue is already in sends nothing. A 429 is retried for every write. After a 5xx or a dropped connection
  a comment is never sent again (it says to check), a transition re-reads the status before a retry, and a page update
  re-reads the version. An `edit` retries a 5xx without a re-read: its body sets fields and adds or removes labels, so
  sending it twice leaves the issue as sending it once does.
- **A page is never overwritten blind.** `page-update` reads the page's version, sends version + 1 with the body, and
  refuses on a 409 or when `--expect-version` is stale. `--section HEADING` replaces what follows that heading up to the
  next heading of the same or a higher level in the same element (the heading is kept), `--append` adds to the end of
  the section or page, and a missing or repeated heading is refused with the count. A heading inside a layout column,
  a table cell or a macro body ends its section at that element's close, so neither a replace nor an append crosses into
  the next column or cell; a heading in a code macro's CDATA or a comment is not a heading. A page body or a new section
  whose tags do not nest cannot be scoped and is refused with the reason. The body is Confluence storage format. The
  model sends only the section, which is most of the saving against the Atlassian MCP; the request to Confluence still
  carries the whole page body (atlas reads the page and splices the section in).
- **Errors do not assume a 403.** The refusal names the HTTP status and what it usually means (token scopes, the
  project's permission, the allow-list) without claiming one cause. Refusals and failures print to stderr; a success to
  stdout.

## Cost against the Atlassian MCP

The Atlassian MCP tool replies are the whole resource (the comment with its author and avatar URLs, the issue, the full
page body) and an update takes the whole page in. atlas replies with one line, and for a section update the model
sends only the section (atlas still PUTs the whole page to Confluence). Measured per write, with the model's tokens estimated by `cargo xtask write-cost` (xtask/src/bpe.rs):

| write | atlas sends | MCP sends | atlas dry-run reply | atlas --apply reply | MCP reply |
|---|---|---|---|---|---|
| comment | 157 B / 62 tok | 204 B / 101 tok | 450 B / 194 tok | 57 B / 32 tok | 2534 B / 1369 tok |
| transition | 48 B / 19 tok | 175 B / 111 tok | 305 B / 132 tok | 57 B / 31 tok | 5709 B / 2943 tok (assumed) |
| edit | 101 B / 44 tok | 192 B / 111 tok | 422 B / 199 tok | 84 B / 44 tok | 4094 B / 2111 tok (assumed) |
| page_update | 663 B / 265 tok | 6085 B / 2182 tok | 553 B / 226 tok | 77 B / 39 tok | 6091 B / 2177 tok |

Basis: documented response shapes as synthetic fixtures; swap in recorded replies from a live run when Brandon has them.
The MCP reply for a transition and an edit is assumed (marked so in the table): the REST calls behind both answer
`204 No Content`, and whether the Atlassian MCP returns the whole issue there is unverified until a live run. A
dry-run and then `--apply` costs both: the model sends the write twice and reads both replies (the comment row:
62 + 194 + 62 + 32 = 350 tokens, against 101 + 1369 = 1470 for the MCP).
The atlas replies are the real output of the write code against fixtures (the golden is checked by a test), the MCP
side is `fixtures/atlassian/mcp-replies/`. A transition through the MCP is two calls (list transitions, then
transition), both counted. The page row uses `--section` on a 6 KB page with a 600-byte section.

## Being polite to the site

Requests to one site are spaced by `min_interval_ms` (default 200). A 429 waits for its `Retry-After`, capped at
`max_wait_s` (default 60); a 429 without one, or a 5xx, backs off from `backoff_ms` (default 500), doubling, for up
to `max_retries` tries (default 3). `Retry-After` is read as seconds (what Atlassian sends); an HTTP-date value is
ignored and the doubling backoff applies. Each request may take `timeout_s` (default 30) and a response at most 64 MB.
Errors name the HTTP status and the endpoint (`GET /rest/api/3/issue/{key}: HTTP 404 (...)`), never the request:
no key, query or header.

## How the token is kept

- The token goes to the system curl (the one atlas already uses for its model files and updates) on curl's stdin as
  a config, never on its command line, so a process listing does not show it. `-q` comes first, so a `.curlrc`
  cannot turn on tracing. curl's own error text is never read: a failure is its exit code (DNS, connect, TLS,
  timeout, size).
- atlas follows redirects itself. A redirect that leaves the site's origin (scheme, host and port) is followed
  without the `Authorization` header, for that hop and every later one; a redirect to plain `http` is refused.
- Every message a failure can produce is scrubbed of the auth material: the header value, the token and its base64
  forms. So are a read's handle and printed digest, in case a proxy or error page echoes the request headers in its
  body. The account email by itself is not a secret and is left as it is.
- `ATLAS_TRACK_DEBUG=1` prints each request line to stderr with the header as `[redacted]`.
- The test suite runs every failure path (DNS failure, TLS error, 401, 403, 404, 429 with `Retry-After`, 5xx,
  malformed JSON, a timeout, a redirect to another host, a panic in the digest) with a sentinel token against a
  local server, then scans stdout, stderr and every file atlas wrote for the sentinel and its base64 forms.

## Work-laptop smoke

The commands to try a real site and paste the result back without work data:

```sh
atlas track check
atlas track smoke --issue PROJ-123 --page 123456 --jql "project = PROJ ORDER BY updated DESC" --cql "type = page AND space = ENG"
```

Both outputs are shapes and sizes only. `ATLAS_TRACK_DEBUG=1` adds the request lines (they hold the site and the
queries, so leave it off for output you paste elsewhere).
