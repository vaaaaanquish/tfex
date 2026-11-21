# tfex design

## Goal

Give coding agents lightweight, trustworthy context about infrastructure code
that is spread over many directories and Git repositories: what is declared
where, which directories are roots, what is known about their state, and what
changed recently.

tfex is not an orchestrator, a deployment system, a replacement for Terraform
or Terragrunt, a live cloud inventory, or a source of truth for deployments.

## Principles

- **Read-only.** tfex reads files and runs read-only Git commands. It never
  runs `terraform init`, `plan`, `apply` or `refresh`, and writes only to its
  own index.
- **Offline unless asked.** `scan` and every query work from the local disk.
  The network is used by one explicitly named command, `fetch`, and only to
  download state objects.
- **No credentials of its own.** Remote stores are read through the vendors'
  command-line tools and whatever the user has signed them in with.
- **Never trust a state file with secrets.** A state can hold credentials in
  plain text whether or not Terraform marks them sensitive, so only metadata
  is ever read out of one. This is enforced where the file is parsed, not
  where it is printed.
- **Say what is not known.** A remote state that was not fetched is
  `unresolved`, not absent and not available. A directory that owns a state is
  `confirmed`, which is not the same as deployed. A file that could not be
  parsed is reported.
- **Deterministic and bounded output.** Stable JSON keys, total ordering in
  every query, `--limit` where a result can be large, and the file and line
  of every declaration.
- **One bad input never hides the rest.** An unreadable file, a malformed
  state, a syntax error, a missing `git`: each degrades one entry, and the
  scan goes on.

## Architecture

A single binary crate. Each module knows about one thing:

| Module | Responsibility |
|---|---|
| `cli` | Arguments, dispatch, JSON on stdout, failures on stderr. Nothing else knows the command line exists. |
| `scan` | The `scan` command: the pipeline below. |
| `fetch` | The `fetch` command: read every located state, in parallel. |
| `commands` | `inspect`, `log`, `status`, `map-state`. |
| `discovery` | Walk directory trees; classify a directory from the evidence. |
| `config` | What the configuration files of one directory declare. `native` (HCL parser), `json` (`.tf.json`), `heuristic` (fallback). |
| `state` | Where the state of each workspace is, and the metadata kept about a state. |
| `remote` | Reading a state object from `gcs` or `s3`. The only network access. |
| `git` | Repository identity and history through the `git` command. |
| `process` | Running `git`, `gcloud` and `aws` with a time limit. |
| `index` | The SQLite index: schema, `write` (one transaction per command), `read` (the row types the commands print). |
| `identity`, `paths`, `time`, `error` | Shared vocabulary. |

### Scan pipeline

1. **Walk** the given trees in parallel and collect every directory that
   directly contains a `.tf` or `.tf.json` file.
2. **Attribute** each directory to a repository. Git is asked once per
   checkout, in parallel; a directory outside Git belongs to the innermost
   scanned path.
3. **Select** one checkout per repository (see [Identity](#identity)).
4. **Read** the configuration of each selected directory, in parallel.
5. **Classify** each directory, now that it is known which directories are
   called as local modules.
6. **Write** everything in one transaction, then prune the directories that
   were indexed under the scanned paths and are gone.

Steps 1, 2 and 4 are where the time goes, and are the parallel ones. The
rest is sequential and ordered by path, which keeps a scan deterministic.

## Identity

| Entity | Identity |
|---|---|
| Repository | hash of the `origin` remote without credentials; without a remote, of the location of its main working tree; outside Git, of the scanned folder |
| Root (Terraform directory) | hash of the repository identity and the path inside the repository |
| Workspace | root and workspace name |
| Resource | root, Terraform address, file and line |

Identity deliberately ignores where a repository is checked out, so that IDs
are stable across machines and clones. The consequence is that the same
directory in two checkouts of one repository is a single root. Indexing both
would leave whichever was written last, so a scan indexes one checkout per
repository: the main working tree in preference to linked worktrees (which
are transient by nature), then the path that sorts first.

A state can be shared or remapped; tfex does not assume that one root has
exactly one state.

## Reading configuration

Native-syntax files are parsed with `hcl-edit`, a complete HCL parser that
keeps source positions. Only top-level blocks are looked at:

- `resource` and `data` blocks become resources;
- `module` blocks become module calls, with `source` when it is a literal;
- a `provider` block is a sign of a root;
- `backend` and `cloud` blocks inside `terraform` give the backend.

Because the file is parsed, text that merely looks like a block (in a
comment, a heredoc, a string, a nested block) cannot be indexed by mistake.

A file the parser rejects is scanned with regular expressions instead, after
its comments are masked. That fallback is knowingly weaker (it does not
understand heredocs or nesting), which is why it is only a fallback and why
the scan report lists the files it was used for.

`.tf.json` files are read as JSON. Terraform accepts an object or a list of
objects wherever a block may appear, and so does tfex.

Backend settings are filtered through an allowlist of the settings that
locate a state (`bucket`, `key`, `prefix`, `path`, ...). Anything else is
dropped while reading.

## State

Where the state of a workspace is, in decreasing order of authority:

1. an explicit mapping given with `map-state`;
2. for a `gcs` or `s3` backend, the object its settings name for the default
   workspace, when those settings are complete literals;
3. for the local backend, the state files in the root (`terraform.tfstate`,
   or `terraform.tfstate.d/<workspace>/terraform.tfstate`);
4. otherwise unknown: the backend type and its allowlisted settings are
   recorded, and the state is `unresolved` with no location.

`scan` reads local files and only *locates* remote states. `fetch` reads
every located state. A fetched summary belongs to a location: a rescan keeps
it while the state is still expected at the same place and resets it to
`unresolved` when the location changes, so the index never shows the
metadata of one object under the name of another.

### Reading remote state

`gcloud storage cat` and `aws s3 cp <uri> -` write the object to a pipe. Using
the tools instead of the cloud SDKs keeps tfex small and synchronous, and,
more importantly, makes authentication somebody else's well-tested problem:
SSO, profiles, impersonation and workload identity all behave as they do in
the user's shell. The cost is a process per state, so states are fetched side
by side, each with a time limit.

A failure to read is recorded per state (`error`, with the last line of the
tool's error output) rather than failing the command: one expired login
should not hide the states that could be read.

`.terraform/terraform.tfstate` is backend metadata written by
`terraform init`, not infrastructure state. `.terraform` is never descended
into.

Settings injected by `-backend-config`, CI, or workspace prefixes cannot be
seen in the source, which is what explicit mappings are for.

A state file is deserialized into a type that only has the metadata fields;
`serde` skips the rest without building values for it. Error messages are
built from the position and category of a parse error, never from its text,
because that text can quote the offending value.

## The index

One SQLite file, bundled SQLite, `STRICT` tables, foreign keys with
`ON DELETE CASCADE`. The schema version is stored in `user_version`; a file
with any other version, or one that tfex did not create, is refused rather
than migrated. The index is a cache of what a scan can rebuild, with one
exception: `state_mappings` holds what the user declared.

## Roads not taken (yet)

- **More of remote state.** Listing the workspaces a backend holds instead
  of locating only the default one; `azurerm` and HCP Terraform; reading the
  resolved backend settings that `terraform init` leaves in `.terraform` when
  the configuration relies on `-backend-config`.
- **Joining configuration and state.** Expanded instance addresses
  (`count`, `for_each`), `moved` blocks, resources in state but not in code
  and the reverse.
- **Dependency graph.** Module ownership and references between resources.
- **Incremental indexing.** Rescan only what changed, keyed by file
  modification time or Git tree hashes.
- **Full-text search** with SQLite FTS5, beyond substring search over
  addresses and types.
- **Drift.** `terraform plan -refresh-only` as an opt-in, never implicit.
- **An MCP server** exposing the same read-only queries.

## Verification criteria

These are covered by the test suite:

- Scanning a repository does not modify it.
- A root is distinguished from the local modules it calls.
- Comments, heredocs, strings and nested blocks do not produce resources.
- Backend metadata under `.terraform` is not treated as state.
- State values, backend credentials and remote URL credentials never reach
  the index or the output.
- An explicit mapping overrides a discovered local state and survives a
  rescan.
- History is scoped to a root, and a directory outside Git has none.
- A remote state is located by a scan but read only by `fetch`.
- Fetched metadata survives a rescan, and is dropped when the state moves.
- A state that cannot be fetched (no access, no tool, timeout) is an error
  on that state, not a failed command.
- Several checkouts of one repository yield one set of roots.
- Rescanning prunes what is gone and keeps what was not rescanned.
