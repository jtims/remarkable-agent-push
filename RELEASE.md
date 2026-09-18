# Release checklist

This repository releases through **GitHub Actions**. The other two paths
are upstream's, kept for reference, and are not used for the releases
published here:

- **GitHub Actions** (active: `.github/workflows/release.yml` runs when a
  tag matching `v*` is pushed, or by manual dispatch with a tag input)
- **CircleCI** (upstream's config at `.circleci/config.yml`; not connected
  to this repository)
- **Local build + manual upload** (upstream's script, no CI required)

---

## Path A: GitHub Actions (active)

Two workflows live under `.github/workflows/`:

- `ci.yml` runs on a push to `main`, on a pull request into `main` and by
  manual dispatch: `rustfmt`, `clippy`, and the test suite on
  `ubuntu-latest` and `macos-15-intel`. A push to any other branch does
  not start it; compile a branch with
  `gh workflow run ci.yml --ref <branch>`.
- `release.yml` builds one target, `x86_64-apple-darwin`, on
  `macos-15-intel` with `MACOSX_DEPLOYMENT_TARGET=13.0`, runs the tests on
  that runner first, packages `rr`, `skills/`, `README.md` and `LICENSE`
  into `rr-x86_64-apple-darwin.tar.gz`, and publishes it with its
  `.sha256` file and a combined `SHA256SUMS`.

Third-party actions are pinned to full commit SHAs, the default token is
read-only, and only the publish job is granted `contents: write`.

### Triggering

Tag only a commit whose CI run on `main` is green:

```bash
git tag -a v0.3.6-jt.6 -m "rr 0.3.6-jt.6: <summary>"
git push origin v0.3.6-jt.6
```

---

## Path B: CircleCI (upstream's, not connected here)

Upstream's notes, kept for reference. No CircleCI project is connected to
this repository, so nothing below runs here.

A working CircleCI config lives at `.circleci/config.yml`. It builds the
same four targets via two jobs (macOS M1 covers both Apple targets
natively, Linux x86_64 covers both Linux targets via `cargo-zigbuild`),
then publishes the GitHub release with the `gh` CLI.

### One-time setup

1. Connect the repo on https://app.circleci.com (free plan).
2. Create a fine-grained GitHub PAT with `contents:write` on the repo.
3. In CircleCI, create a **context** named `rr-release` and add the PAT
   as `GITHUB_TOKEN`. The publish job references this context.

### Triggering

```bash
git tag v0.2.0
git push origin v0.2.0
```

CircleCI's filter is `tags: only: /^v.+/`, so only tag pushes run the
workflow. ~6–8 min end-to-end.

---

## Path C: Local build + manual upload (upstream's)

Upstream's notes, kept for reference. The releases published in this
repository are not built this way: every asset here is built and uploaded
by the workflow in Path A.

Useful for ad-hoc releases, or when you want the whole pipeline on your
laptop.

### Prerequisites

- `rustup`, `cargo`
- For Linux cross-builds from macOS, install **one** of:
  - `brew install zig && cargo install --locked cargo-zigbuild` (lighter, no Docker)
  - `cargo install --locked cross` (uses Docker)
- For upload: `gh` (`brew install gh && gh auth login`)

### Build only

```bash
./scripts/build-release.sh
```

Drops tarballs + `.sha256` files + a combined `SHA256SUMS` into `./dist/`.

### Build and upload to a GitHub release

```bash
git tag v0.2.0
git push origin v0.2.0          # tag must exist on the remote first
./scripts/build-release.sh --tag v0.2.0 --upload
```

If the release already exists for that tag, the script uploads with
`--clobber` (re-uploads, overwrites). Otherwise it creates the release
with auto-generated notes.

### Verifying locally

```bash
ls -lh dist/

INSTALL_DIR=/tmp/rr-test ./install.sh
/tmp/rr-test/rr --help
```

---

## Other free CI alternatives (not configured here)

- **Cirrus CI** — free for public repos, native macOS + Linux runners.
- **GitLab CI mirror** — push the repo as a mirror, run CI there,
  pull artifacts back.

---

## Post-release sanity

- [ ] `curl | bash` install works on a clean machine
- [ ] `rr auth` flow works
- [ ] `rr push <file.md> --dry-run` reports `lines removed: 0`, then a
      real `rr push` produces a native notebook on the tablet
- [ ] Test on macOS Intel, the one target this repository builds
