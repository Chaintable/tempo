# Contributing

Thanks for your interest in contributing.

This repository is a **fork**: upstream [tempoxyz/tempo](https://github.com/tempoxyz/tempo)
plus block-data RPC extensions used by the external
[Chaintable pipeline](https://github.com/Chaintable/pipeline) tracer. It
runs write node(s) that produce block data for the Chaintable data pipeline, for
the chain(s) listed in this repository's CI configuration and README. It is not
a general-purpose fork of Tempo.

**First, determine where your change belongs:**

- **Chain client changes** (consensus, p2p, EVM, RPC, txpool) — contribute
  **upstream**, following their contributing process. We cannot accept
  chain-core changes in this fork: they would diverge from upstream and be lost
  or cause conflicts at the next upstream merge. If an upstream fix matters to
  this fork, open an issue here linking the upstream PR/commit and we will pull
  it in with the next sync.

- **Pipeline layer changes** — the pipeline tracer and its block-data output,
  the Dockerfile, published images, CI workflows, or docs about running this
  write node — contribute **here**, following the process below.

---

## Our Process (contributions to the Chaintable pipeline layer)

### Getting Started

Requirements:

* Rust (version per `Cargo.toml`)
* Standard Tempo build prerequisites — see the
  [developer docs](https://docs.tempo.xyz/guide/node/installation#build-from-source)

### Development Workflow

1. Fork the repository
2. Create a branch from `main`
3. Make changes, focused on the pipeline layer
4. Run local checks
5. Open a PR

Keep PRs small and focused.

### Local Checks (must pass)

```bash
cargo build --locked --bin tempo
cargo test --locked -p debank-rpc
cargo +nightly fmt --check
```

### Code Guidelines

* Keep the diff minimal — prefer hooks over invasive edits to client code
* Match the existing code style and conventions (`cargo +nightly fmt`)
* Prefer simple and explicit logic
* Do not change chain-core behavior (see the top of this document)

### Testing

Changes to the pipeline layer must include tests where practical. At minimum,
describe how you verified the emitted data: chain, block range, and what you
compared it against.

### Pull Requests

Before submitting:

* Local checks pass
* Tests added or updated
* Behavior changes clearly explained

PRs should include:

* Summary
* Motivation
* Testing details
* Compatibility impact

Note on CI: same-repository pull requests build and publish test images. Image
CI does not run on fork or Dependabot pull requests because publishing requires
repository credentials. A maintainer will build and verify those changes on an
internal branch.

### Commit Guidelines

* Use clear, descriptive messages

Example:

```
tracer: fix state-diff ordering for reorged blocks
```

### Releases

* Release tags follow `v<base-version>-ct.N` (`ct` = Chaintable; e.g.
  `v1.14.0-ct.1`); a GitHub Release publishes the versioned images

### Reporting Issues

Please include:

* Image tag or commit
* Chain and block height
* Reproduction steps
* Expected vs actual behavior

### Security

Do not disclose vulnerabilities publicly.

See [SECURITY.md](./SECURITY.md) for reporting instructions.

### License

By contributing, you agree that your contributions are licensed under the same
terms as this repository — see [LICENSE-MIT](./LICENSE-MIT) (MIT) and
[LICENSE-APACHE](./LICENSE-APACHE) (Apache-2.0).
