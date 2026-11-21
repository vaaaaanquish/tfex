# tfex — Terraform Explorer

A local, read-only index of the Terraform configurations scattered across your
directories and Git repositories, built for coding agents (and people) who
need quick answers to questions such as:

- Where is this resource declared?
- Which directories are roots, and which are reusable modules?
- What is known about each root's state?
- What changed in this root recently?

`tfex scan` walks directory trees once and stores what it finds in a SQLite
file. Every other command answers from that file in milliseconds and prints
JSON.

tfex never runs Terraform and never writes anywhere except its own index.
Nothing touches the network unless you run `tfex fetch`, which reads state
metadata from remote backends.

## Install

Requires a Rust toolchain (see `rust-version` in [Cargo.toml](Cargo.toml)).

tfex uses these tools when they are installed, and degrades when they are not:

| Tool | Used for |
|---|---|
| `git` | repository identity and history |
| `gcloud` | `tfex fetch` of states in Google Cloud Storage |
| `aws` | `tfex fetch` of states in Amazon S3 |

```bash
cargo install --git https://github.com/vaaaaanquish/tfex
```

## Usage

```bash
# Index one or more directory trees: repositories, monorepos, or folders
# that are not under Git at all.
tfex scan ~/work/infra ~/work/services

tfex roots                                # roots only
tfex roots --all                          # ... and reusable modules
tfex search google_cloud_run_v2_service   # by resource address or type

tfex inspect infra/envs/prod              # resources, modules, states, last commit
tfex log infra/envs/prod --limit 5        # commits that touched the root
tfex status                               # state of every workspace
tfex status infra/envs/prod

tfex fetch                                # read state metadata, remote backends included
tfex fetch infra/envs/prod
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
that disappeared are pruned (with their state mappings), and everything
outside the scanned paths is left alone.

## Security

- Read-only: tfex reads files and runs a few read-only Git commands
  (`rev-parse`, `remote get-url`, `log`). It does not run `terraform`.
- No network access, except for `tfex fetch`, which only downloads state
  objects, and only through `gcloud` and `aws`.
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

- Nothing is known about live infrastructure: no plan, no drift detection. A
  state says what Terraform last recorded, not what exists.
- Only `gcs` and `s3` states can be fetched. For a remote backend only the
  default workspace is located automatically.
- Configuration is read shallowly. Expressions are not evaluated, so `count`
  and `for_each` are not expanded and a module `source` that is not a plain
  string literal is not followed. Registry and Git modules are not resolved.
- A file with a syntax error is indexed on a best-effort basis and listed in
  the scan report under `parse_fallbacks`.
- Declarations in `.tf.json` files are all reported on line 1.
- Every scan re-reads everything; there is no incremental indexing.

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
