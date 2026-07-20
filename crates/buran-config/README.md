# buran-config

[![part of Buran](https://img.shields.io/badge/part%20of-Buran-c18b57)](https://github.com/buran-project/buran)
[![license](https://img.shields.io/badge/license-Apache--2.0-informational)](https://github.com/buran-project/buran/blob/main/LICENSE)

**The configuration layer of the
[Buran](https://github.com/buran-project/buran) application server** — the
typed schema behind the single `buran.yaml` that describes the whole edge.

Parses and validates the config, expands `${ENV}` tokens, and resolves the
automatic worker count — so a bad config fails loudly at load (and at
`buran --check-config`) rather than at runtime.

- `schema.rs` — the strict, typed schema (`settings`, `listeners`, `routes`,
  `applications`, `access_log`/`error_log`); unknown keys are rejected.
- `validate.rs` — cross-field validation and inline-application extraction.
- `subst.rs` — `${ENV}` substitution over string scalars (anchors/aliases
  forbidden by design).
- `cpu.rs` — effective CPU detection (cgroup-quota aware) for `processes: auto`.

Configuration reference and examples live in the
[docs](https://github.com/buran-project/buran/tree/main/docs) and
[`examples/buran.yaml`](https://github.com/buran-project/buran/blob/main/examples/buran.yaml).

> Internal workspace crate — not published to crates.io.

## License

Licensed under the [Apache License 2.0](https://github.com/buran-project/buran/blob/main/LICENSE).
Part of the [Buran project](https://github.com/buran-project/buran).
