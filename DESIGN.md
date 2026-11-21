# tfex design

## Goal

Give coding agents lightweight, trustworthy context about infrastructure code
that is spread over many directories and Git repositories: what is declared
where, which directories are roots, what is known about their state, what
depends on what, what changed recently, and (when asked) what Terraform would
change.

tfex is not an orchestrator, a deployment system, a replacement for Terraform
or Terragrunt, a live cloud inventory, or a source of truth for deployments.

## Principles

- **Read-only.** tfex reads files and runs read-only Git commands, and writes
  only to its own index. It never runs `terraform init`, `apply` or
  `refresh`. `terraform plan` is run by one explicitly named command, `plan`,
  without locking the state and without saving a plan.
- **Offline unless asked.** `scan` and every query work from the local disk.
  The network is used by two explicitly named commands: `fetch`, to download
  state objects, and `plan`, through Terraform. Nothing else starts either.
- **No credentials of its own.** Remote stores are read through the vendors'
  command-line tools and whatever the user has signed them in with.
- **Never trust a state file with secrets.** A state can hold credentials in
  plain text whether or not Terraform marks them sensitive, so only metadata
  is ever read out of one. This is enforced where the file is parsed, not
  where it is printed. The same goes for a plan.
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
| `plan` | The `plan` command: run `terraform plan` and summarize what it says. The only place Terraform is run. |
| `commands` | `inspect`, `log`, `status`, `graph`, `deps`, `map-state`. |
| `discovery` | Walk directory trees; classify a directory from the evidence. |
| `config` | What the configuration files of one directory declare, and their fingerprint. `native` (HCL parser), `json` (`.tf.json`), `heuristic` (fallback), `references` (which declarations an expression mentions). |
| `state` | Where the state of each workspace is, and the metadata kept about a state. |
| `remote` | Reading a state object from `gcs` or `s3`. The only network access tfex makes itself. |
| `git` | Repository identity and history through the `git` command. |
| `process` | Running `git`, `gcloud`, `aws` and `terraform` with a time limit. |
| `index` | The SQLite index: schema, `write` (one transaction per command), `read` (the row types the commands print). |
| `identity`, `paths`, `time`, `error` | Shared vocabulary. |

### Scan pipeline

1. **Walk** the given trees in parallel and collect every directory that
   directly contains a `.tf` or `.tf.json` file.
2. **Attribute** each directory to a repository. Git is asked once per
   checkout, in parallel; a directory outside Git belongs to the innermost
   scanned path.
3. **Select** one checkout per repository (see [Identity](#identity)).
4. **Read** the configuration of each selected directory, in parallel,
   unless its fingerprint says it is as the index has it (see
   [Incremental scans](#incremental-scans)).
5. **Classify** each directory, now that it is known which directories are
   called as local modules.
6. **Write** everything in one transaction, then prune the directories that
   were indexed under the scanned paths and are gone.

Steps 1, 2 and 4 are where the time goes, and are the parallel ones. The
rest is sequential and ordered by path, which keeps a scan deterministic.

### Incremental scans

Parsing and rewriting the rows of a directory is most of the work of a scan,
and almost always produces what is already in the index. So each directory
is recorded with a fingerprint: a digest of the name, size and modification
time of every configuration file directly inside it, and of the tfex version
(what is extracted from a file depends on the code extracting it). A scan
that finds the same fingerprint, at the same path, leaves the directory
unread and its resources, module calls and references untouched.

The walk itself is not skipped: it is how new and deleted directories are
found, and the fingerprint needs a `stat` of each file anyway. Git tree
hashes would avoid even that, but only for clean checkouts of committed
files, and tfex also indexes folders outside Git and uncommitted work.

Three things keep this from making the index wrong:

- **What depends on other directories or on other files is recomputed.**
  The classification of a directory depends on who calls it, and its states
  on files that are not configuration. Both are worked out by every scan
  from an *outline* of the configuration (backend, whether a provider is
  configured, whether anything is declared, which local modules are
  called), which is exactly what the index can give back for an unchanged
  directory.
- **Modification times are not trusted when they are fresh.** A file written
  twice within the resolution of the file system's timestamps keeps its
  time. A directory with a file modified in the last two seconds gets no
  fingerprint, and is read again by the next scan. The fingerprint is taken
  before the files are read, so a write racing with the scan is caught by
  the next one too.
- **Whatever must be reported again is read again.** A directory with a
  file the parser rejected gets no fingerprint, so `parse_fallbacks` keeps
  listing it until it is fixed.

What is left is a change that preserves both size and modification time,
which `scan --full` is for.

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
- `variable` blocks and the attributes of `locals` blocks are noted as
  things that can be referred to, and `output` blocks as things that refer;
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

## Dependency graph

There are two graphs, and neither needs Terraform to build.

**Module calls** connect directories. A call with a local `source` records
the canonical path it points at; queries join that path with the paths of
the indexed directories. Storing the path rather than the ID of the target
means the edge appears as soon as the target is indexed, by whichever scan,
and disappears with it. Reachability in either direction ("what does this
root pull in", "which roots use this module") is a recursive query.

**References** connect the declarations of one directory. Expressions are
walked with the parser's visitor, and every traversal shaped like a
reference (`<type>.<name>`, `data.<type>.<name>`, `module.<name>`,
`var.<name>`, `local.<name>`) is a candidate, wherever it is: arguments,
nested and `dynamic` blocks, templates, `depends_on`. A candidate is kept
only if it names something the directory declares. That one rule is what
discards `each.value`, `count.index`, `self.id`, `path.module`, iterator
variables of `for` expressions and `dynamic` blocks, and references to
things that do not exist, without tfex having to model Terraform's scopes.
The price is a false edge when a `for` variable is named like a resource
type and reads an attribute named like a resource, which nobody writes.

Each pair is stored once, at its first occurrence. In `.tf.json` files every
string is parsed as a template, and the items of `depends_on` as bare
references.

References stop at module boundaries: `module.app.address` depends on
`module.app`, and what `address` is made of is in another directory, under
other addresses. Joining the two is left for later.

Both recursive queries stop after 64 steps. Terraform rejects cycles, but
tfex reads configurations Terraform never validated.

## Plans

`tfex plan` runs `terraform plan -json -input=false -lock=false` (plus
`-refresh-only` for a drift check) in the directory of each selected root,
side by side, each with a time limit.

- **Opt-in, per root.** There is no "plan everything": roots must be named.
  No other command starts Terraform.
- **No init.** `terraform init` downloads providers and modules and writes
  `.terraform`; that is a decision for the user. An uninitialized root
  fails with Terraform's own message.
- **No lock, no plan file.** Since Terraform refreshes in memory while
  planning, a plan without `-out` writes neither state nor plan, so there
  is nothing a lock would protect, and holding one would make a concurrent
  apply fail. Not saving the plan also keeps its values off the disk.
- **The streaming JSON output, not the plan.** `terraform show -json` on a
  saved plan carries every value, before and after. The machine-readable
  UI output names each change (`planned_change`, `resource_drift`) by
  address and action and summarizes the counts (`change_summary`), which
  is all tfex wants. It is deserialized into types that have only those
  fields. Of a diagnostic, only the severity and the one-line summary are
  read: the detail and the source snippet can quote values.
- **A failure is an outcome.** No `terraform`, no credentials, a timeout, an
  error diagnostic: each is `status: error` on that plan, recorded like any
  other outcome, and the command succeeds.

The workspace a plan is about is the one given with `--workspace` (passed
on as `TF_WORKSPACE`), or the one Terraform has selected in the directory
(`.terraform/environment`), or `default`.

The index keeps a summary of the last plan of each kind per workspace:
status, counts, when. It is a record of something that happened, not a
cache, so rescans keep it, and `planned_at` is there to judge it by. The
list of changed addresses is printed but not stored.

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
than migrated. The index is a cache of what a scan can rebuild, with two
exceptions: `state_mappings` holds what the user declared, and `plans` what
Terraform said the last time it was asked.

## Roads not taken (yet)

- **More of remote state.** Listing the workspaces a backend holds instead
  of locating only the default one; `azurerm` and HCP Terraform; reading the
  resolved backend settings that `terraform init` leaves in `.terraform` when
  the configuration relies on `-backend-config`.
- **Joining configuration and state.** Expanded instance addresses
  (`count`, `for_each`), `moved` blocks, resources in state but not in code
  and the reverse.
- **References across module boundaries.** Connecting `module.x.<output>`
  in a caller to `output.<output>` in the module, and the arguments of a
  call to the module's variables.
- **Skipping the walk.** Using Git tree hashes to leave whole unchanged
  checkouts unvisited.
- **Keeping the changes of a plan**, to answer "what drifted last night"
  offline, and passing `-var-file` or `-target` through.
- **Full-text search** with SQLite FTS5, beyond substring search over
  addresses and types.
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
- A rescan leaves unchanged directories unread, and still notices a changed
  file, a new caller, a new state, a moved checkout and a syntax error.
- Module calls resolve to indexed directories, whichever scan indexed them,
  and are followed in both directions.
- Only references to what a directory declares are kept; cycles end.
- Terraform is run by `plan` and by nothing else, with arguments that
  neither lock nor write.
- Plan values and diagnostic details never reach the index or the output.
- A plan Terraform cannot make (not initialized, no program, timeout) is an
  error on that plan, not a failed command.
