# CLI reference

Buran is a single binary. Its command line is intentionally tiny —
configuration lives in the YAML file, not in flags.

```
buran [--config <path>]                    run the server
buran --check-config [--config <path>]     validate config and modules, then exit
buran --modules [--config <path>]          list runtime modules found in the modules dir
buran --version
buran --help
```

## Flags

| Flag | Alias | Effect |
|------|-------|--------|
| `--config <path>` | `-c <path>` | Path to the YAML config. Default: `/etc/buran/buran.yaml`. |
| `--check-config` | | Load and validate the config, probe every referenced module for protocol compatibility, print `config ok`, and exit. Non-zero exit on any problem. |
| `--modules` | | Print the modules directory and the module names installed there, then exit. |
| `--version` | `-V` | Print the version and exit. |
| `--help` | `-h` | Print usage and exit. |

An unknown argument is an error and prints usage.

### `--check-config`

Validates structure **and** runtime wiring:

- strict schema (unknown fields rejected),
- reference integrity (routes, applications, listeners point at real objects),
- exclusivity rules (one action terminal; `route` vs `status`; etc.),
- and it actually runs each referenced module binary with `--describe` to
  confirm it exists and speaks a compatible BWP version.

Run it in CI and before every deploy:

```bash
buran --check-config --config /etc/buran/buran.yaml
```

### `--modules`

Lists what is installed in `settings.modules`, so you know which `module:`
values are valid:

```bash
$ buran --modules
modules directory: /usr/lib/buran/modules
php83, php84, php85
```

## Environment variables

| Variable | Effect |
|----------|--------|
| `RUST_LOG` | Log verbosity/filtering (`tracing` env-filter syntax, e.g. `RUST_LOG=debug` or `RUST_LOG=buran_router=trace`). Default: `info`. Logs go to **stderr**. |

Any `${NAME}` used in the config is resolved from the process environment at
load time — see [Configuration › environment substitution](configuration.md#environment-variable-substitution).

## Signals

| Signal | Effect |
|--------|--------|
| `SIGTERM` | Graceful shutdown (container stop): stop accepting, drain in-flight connections, exit. |
| `SIGINT` | Same as `SIGTERM` (Ctrl-C). |

When running as **PID 1** in a container, Buran also reaps orphaned child
processes so they do not linger as zombies.
