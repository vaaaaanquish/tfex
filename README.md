# tfex — Terraform Explorer

A local, read-only index of the Terraform configurations scattered across your
directories and Git repositories, built for coding agents (and people) who
need quick answers to questions such as:

- Where is this resource declared?
- Which directories are roots, and which are reusable modules?
- What is known about each root's state?
- What changed in this root recently?
- Which roots use this module, and what refers to this resource?
- What would Terraform change here, and what has drifted?

`tfex scan` walks directory trees once and stores what it finds in a SQLite
file. Every other command answers from that file in milliseconds and prints
JSON.

tfex writes nowhere except its own index, and does nothing behind your back:
the network is only used by `tfex fetch`, which reads state metadata from
remote backends, and Terraform is only run by `tfex plan`.

## Install

Requires a Rust toolchain (see `rust-version` in [Cargo.toml](Cargo.toml)).

tfex uses these tools when they are installed, and degrades when they are not:

| Tool | Used for |
|---|---|
| `git` | repository identity and history |
| `gcloud` | `tfex fetch` of states in Google Cloud Storage |
| `aws` | `tfex fetch` of states in Amazon S3 |
| `terraform` or `tofu` | `tfex plan` |

```bash
cargo install --git https://github.com/vaaaaanquish/tfex
```

## Usage

```bash
# Index one or more directory trees: repositories, monorepos, or folders
# that are not under Git at all.
tfex scan ~/work/infra ~/work/services
tfex scan --full ~/work/infra             # read again what looks unchanged

tfex roots                                # roots only
tfex roots --all                          # ... and reusable modules
tfex search google_cloud_run_v2_service   # by resource address or type

tfex inspect infra/envs/prod              # resources, modules, states, last commit
tfex log infra/envs/prod --limit 5        # commits that touched the root
tfex status                               # state of every workspace
tfex status infra/envs/prod

tfex graph                                # every module call, and what it resolves to
tfex graph infra/modules/network          # who uses it, what it uses, references inside
tfex deps infra/envs/prod aws_vpc.main    # what it depends on, what depends on it

tfex fetch                                # read state metadata, remote backends included
tfex fetch infra/envs/prod

tfex plan infra/envs/prod                 # run terraform plan: what would change
tfex plan --refresh-only infra/envs/prod  # ... or what has drifted
```

A root can be named by its ID from `tfex roots` (or a unique prefix of it),
by `<repository>/<path>`, or by its directory, so `tfex inspect .` works from
inside a root.

The index is at `$XDG_DATA_HOME/tfex/index.sqlite3`
(`~/.local/share/tfex/index.sqlite3` by default). Use `--db <path>` or
`TFEX_DB` to keep it somewhere else. It is a cache: delete it and scan again
whenever you like.

### Output

Everything is JSON with a stable key order and deterministic ordering.
Declarations come with the directory, file and line they are on:

```console
$ tfex search aws_s3_bucket
[
  {
    "repository": "infra",
    "root_id": "3f6c0a9d2b1e",
    "relative_path": "envs/prod",
    "path": "/home/me/work/infra/envs/prod",
    "address": "aws_s3_bucket.logs",
    "kind": "resource",
    "file": "main.tf",
    "line": 7
  }
]
```

A failure prints one line of JSON to standard error and exits with status 1:

```console
$ tfex inspect nope
{"error":"Root not found: nope; run 'roots' first","kind":"root_not_found"}
```

## What tfex works out

### Roots and modules

Every directory that directly contains `.tf` or `.tf.json` files is indexed
and classified by the strongest signal found:

| Classification | Reason |
|---|---|
| `confirmed` | a backend (or `cloud` block) is declared, or a local state file exists |
| `module` | another indexed directory calls it with a local `source` |
| `probable` | a provider is configured, or resources or module calls are declared |
| `unknown` | none of the above |

`confirmed` means "this directory owns a state", not "this infrastructure was
deployed successfully".

`.git`, `.terraform`, `.terragrunt-cache`, `.venv`, `node_modules` and `vendor`
directories are skipped. Hidden and git-ignored directories are not.

### State

tfex records **metadata only** for a state: serial, lineage, Terraform version
and the number of managed resource instances. Resource attributes and outputs
are skipped while the file is parsed, and are never stored or printed.

| Status | Meaning |
|---|---|
| `available` | the state was read |
| `error` | the state is expected at a location but could not be read from it |
| `unresolved` | the state was not read: it is remote and was not fetched, or nothing says where it is |

`unresolved` says nothing about whether the state exists.

`tfex scan` works out where each state is, and reads the ones on the local
disk:

- a local backend: `terraform.tfstate` and `terraform.tfstate.d/*/` in the root;
- a `gcs` or `s3` backend whose settings are written out in the configuration:
  the object Terraform uses for the default workspace, such as
  `gs://my-bucket/envs/prod/default.tfstate`.

`tfex fetch` then reads every located state, remote ones included, and
records its metadata:

```console
$ tfex fetch infra/envs/prod
{
  "fetched": 1,
  "failed": 0,
  "unresolved": 0,
  "states": [
    {
      "repository": "infra",
      "root_id": "3f6c0a9d2b1e",
      "relative_path": "envs/prod",
      "workspace": "default",
      "backend_type": "gcs",
      "location": "gs://my-bucket/envs/prod/default.tfstate",
      "status": "available",
      "serial": 128,
      "resource_count": 42,
      "fetched_at": "2026-01-02T03:04:05Z",
      "error": null
    }
  ]
}
```

Remote states are read with `gcloud storage cat` and `aws s3 cp`, using the
credentials those tools are already set up with; tfex has none of its own.
The `region`, `profile` and `impersonate_service_account` settings of the
backend are passed on. What was fetched is kept across rescans for as long as
the state stays at the same location, with `fetched_at` telling how old it is.

When the configuration does not say where a state is — settings injected with
`terraform init -backend-config=...` or by CI, dynamic values, named
workspaces, a state file kept elsewhere — declare it:

```bash
tfex map-state infra/envs/prod /path/to/terraform.tfstate --workspace prod
tfex map-state infra/envs/prod gs://my-bucket/prod/default.tfstate
```

A mapping survives rescans and takes precedence over what the root itself
says.

### Dependencies

Two kinds of dependency are indexed, both read from the configuration
without evaluating it.

**Between directories**: a `module` block whose `source` is a local path
makes the calling directory depend on the called one. `tfex graph` lists
every module call with the directory it resolves to (`target` is `null` for
registry and Git modules, and for directories that are not indexed).
`tfex graph <root>` follows the calls both ways, through any number of
modules, which answers "which roots do I have to plan after changing this
module?":

```console
$ tfex graph infra/modules/network
{
  "id": "9b1d6f0c2a77",
  "repository": "infra",
  "relative_path": "modules/network",
  ...
  "depends_on": [],
  "dependents": [
    {
      "root_id": "5c0e8a1f3d42",
      "repository": "infra",
      "relative_path": "modules/app",
      "classification": "module",
      "depth": 1
    },
    {
      "root_id": "3f6c0a9d2b1e",
      "repository": "infra",
      "relative_path": "envs/prod",
      "classification": "confirmed",
      "depth": 2
    }
  ],
  "references": [...]
}
```

**Inside a directory**: an expression that mentions another declaration
makes its own declaration depend on it. Resources, data sources, module
calls, local values and outputs refer; all of those except outputs, and
input variables, are referred to. `tfex graph <root>` lists each reference
once, with the file and line of its first occurrence, and `tfex deps`
follows them from one address:

```console
$ tfex deps infra/envs/prod module.app
{
  "root_id": "3f6c0a9d2b1e",
  "repository": "infra",
  "relative_path": "envs/prod",
  "address": "module.app",
  "depends_on": [
    { "address": "data.aws_ami.latest", "depth": 1 },
    { "address": "local.name", "depth": 1 },
    { "address": "var.region", "depth": 2 }
  ],
  "dependents": [
    { "address": "aws_route53_record.app", "depth": 1 },
    { "address": "output.record", "depth": 2 }
  ]
}
```

`depth` is the number of calls or references in between: 1 is direct.

### Plans and drift

`tfex plan <root>...` runs `terraform plan` in each root and reports which
resource instances would be created, updated, replaced or deleted. With
`--refresh-only` it runs `terraform plan -refresh-only` instead, which
compares the state with the real infrastructure: that is drift detection.

```console
$ tfex plan --refresh-only infra/envs/prod
{
  "planned": 1,
  "failed": 0,
  "plans": [
    {
      "repository": "infra",
      "root_id": "3f6c0a9d2b1e",
      "relative_path": "envs/prod",
      "workspace": "default",
      "mode": "refresh-only",
      "status": "drift",
      "terraform_version": "1.9.0",
      "add": 0,
      "change": 0,
      "remove": 0,
      "import": 0,
      "changes": [],
      "drift": [
        { "address": "aws_s3_bucket.logs", "action": "update" }
      ],
      "planned_at": "2026-01-02T03:04:05Z",
      "error": null
    }
  ]
}
```

| Status | Meaning |
|---|---|
| `no_changes` | nothing to apply; for `--refresh-only`, nothing drifted |
| `changes` | applying the configuration would change something (listed in `changes`) |
| `drift` | `--refresh-only` found resources changed outside Terraform (listed in `drift`) |
| `error` | Terraform could not make the plan; `error` says why |

A plain plan lists drift too, but only the drift Terraform considers
relevant to what it plans; use `--refresh-only` to see all of it.

This is the one command that runs Terraform, and it is as unobtrusive as a
plan can be:

- It runs in the root as it is. tfex does not run `terraform init`; a root
  that is not initialized is an `error` on that plan.
- The state is not locked (`-lock=false`), so a plan never makes somebody
  else's apply wait. No plan file is saved.
- Terraform runs with the credentials and environment of your shell, in the
  workspace selected in the root unless `--workspace` (or `TF_WORKSPACE`)
  names another.
- `--terraform tofu` (or `TFEX_TERRAFORM`) runs another program;
  `--timeout` bounds each plan (600 seconds by default).

Only addresses, actions and counts are read from Terraform's output; planned
values are never stored or printed. The summary of the last plan of each
kind is kept, and shown by `tfex inspect` under `plans` with `planned_at`
telling how old it is.

### Repositories and checkouts

A root is identified by its repository (the `origin` remote, or the location
on disk when there is none) and its path inside the repository. Its ID is
therefore the same on every machine and in every clone.

It also means that one repository is indexed from one checkout. When a scan
finds several checkouts of the same repository — clones, or `git worktree`
worktrees — it indexes the main working tree in preference to linked
worktrees, then the checkout whose path sorts first, and reports the others
as `duplicate_checkouts`. Scan a worktree on its own to index it instead.

A folder outside Git is treated as one repository named after the scanned
directory.

### Rescanning

`tfex scan` replaces what the index holds for the scanned paths: directories
that disappeared are pruned (with their state mappings and plans), and
everything outside the scanned paths is left alone.

A rescan is incremental. A directory whose `.tf` and `.tf.json` files have
the names, sizes and modification times the previous scan found is not read
again (`unchanged_directories` in the report); its classification and its
local states are still brought up to date. `tfex scan --full` reads
everything regardless. A file modified in the last two seconds is never
trusted to be unchanged, nor is a directory with a file that has a syntax
error.

## Security

- Read-only: tfex reads files and runs a few read-only Git commands
  (`rev-parse`, `remote get-url`, `log`). It runs `terraform` only for
  `tfex plan`, and then only `terraform plan`, without locking the state or
  saving a plan.
- No network access of its own, except for `tfex fetch`, which only
  downloads state objects, and only through `gcloud` and `aws`. During
  `tfex plan`, Terraform and its providers talk to whatever they talk to.
- Plan values are never kept: of Terraform's output, only resource
  addresses, actions, counts and the one-line summary of an error are read.
- State values are never kept (see [State](#state)). A fetched state is held
  in memory just long enough to count its resources; it is not written to
  disk.
- Backend settings are allowlisted: only settings that locate a state or name
  the identity to read it with are kept, so `access_key`, `secret_key`, tokens
  and the like are never indexed.
- Credentials embedded in a Git remote URL are stripped before it is stored.

Treat the output as untrusted input all the same: repository names, commit
subjects and HCL identifiers come from whatever was scanned.

## Limitations

- Live infrastructure is only looked at by `tfex plan`, one root at a time
  and only when asked. A state says what Terraform last recorded, not what
  exists, and a recorded plan says what was true at `planned_at`.
- `tfex plan` needs roots that are already initialized, and passes no
  `-var` or `-var-file`: variables without defaults must come from
  `*.auto.tfvars` files or `TF_VAR_*` environment variables.
- Only `gcs` and `s3` states can be fetched. For a remote backend only the
  default workspace is located automatically.
- Configuration is read shallowly. Expressions are not evaluated, so `count`
  and `for_each` are not expanded and a module `source` that is not a plain
  string literal is not followed. Registry and Git modules are not resolved.
- A file with a syntax error is indexed on a best-effort basis and listed in
  the scan report under `parse_fallbacks`.
- References are found by the shape of expressions and kept when they name
  something the directory declares. They stop at module boundaries:
  `module.app.address` is a reference to `module.app`, not to the output
  inside the module. A file with a syntax error contributes none.
- Declarations and references in `.tf.json` files are all reported on
  line 1.
- An incremental scan trusts sizes and modification times; a change that
  preserves both goes unnoticed until `tfex scan --full`.

## Development

The toolchain is pinned in [rust-toolchain.toml](rust-toolchain.toml).

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items
cargo deny check   # advisories and licenses; needs cargo-deny
typos              # spelling; needs typos-cli
```

See [DESIGN.md](DESIGN.md) for how the code is organized and why.

## License

[MIT](LICENSE)
