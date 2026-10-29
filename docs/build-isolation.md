# Build isolation — design

Status: **implemented** — Phases 1–7 are in: env, sandbox, closure v1
(extract-per-build) **and v2 (overlayfs layer cache)**, lint, and rootless
builds as a first-class mode. `scripts/spike-sandbox.sh` remains the
Phase 0 host check. The layer cache needs overlayfs inside a user
namespace (kernel ≥ 5.11) and silently falls back to extract-per-build
when the probe says no (`nawa sandbox-probe overlay`).

Implementation notes that refine the text below:

* The sandbox spec is **TOML**, not JSON: `toml` is already a dependency and
  `serde_json` is not, and nawa's own offline vendored bootstrap must not
  gain crates.
* The `live`-mode sandbox keeps the current root and overlays it (a
  recursive self-bind that nawa then marks read-only) instead of pivoting;
  `closure` mode binds the assembled root onto itself, `pivot_root`s into it
  with a `chroot` fallback, masks `/old`, and only then marks `/` read-only
  (pivot into a read-only tree is refused).
* **Layer cache (Phase 5, implemented):** `closure::assemble_cached` keeps
  one extracted directory per archive under
  `<cache>/layers/<archive-sha256>/` (`NAWA_LAYER_CACHE` overrides the
  location, default `<root>/var/cache/nawa`, so the layers land at
  `<root>/var/cache/nawa/layers/<sha>`; `NAWA_NO_LAYER_CACHE=1` disables
  the feature). The build root is mounted as an **overlayfs**
  (lowerdirs = layers, throw-away upper/work under the sandbox scratch)
  instead of being extracted per build. Because the overlay mount happens
  *inside* the sandbox, its lowerdirs must be traversable by the §4.3
  mapped identity, not by whoever ran nawa: `nawa build` checks
  `sandbox::first_untraversable_ancestor` before choosing the layered
  strategy and falls back to extract-per-build with a note when the cache
  sits under a tree the map cannot reach (e.g. a mode-700 `$HOME` in a
  `sudo` run — keep the rootfs under the build dir, or point
  `NAWA_LAYER_CACHE` at a reachable path). The archive is signature-verified
  on *every* assembly — cache hits included — so the trust guarantee is
  identical to `nawa install`. The extract-per-build fallback never touches
  the cache. Paths containing `,` or
  `:` cannot be expressed in an overlayfs data string; the builder falls
  back to v1 for them (with a note).
* **build_id cache hits (implemented, opt-in):** `nawa build --use-cache`
  compares the `build_id` recorded next to the archive (`<out>/<archive>.buildid`)
  with the id the current inputs produce (recipe bytes, source sha256,
  closure archive set, profile flags). On a match the build is skipped
  entirely — before even downloading the source. `--rebuild-check` always
  ignores the cache (it exists to prove builds reproduce).
* **Rootless (Phase 7, implemented):** `nawa build` works unchanged as a
  normal user at every level including `closure`: the sandbox self-writes
  its `0 → <caller-uid>` map, `/build` falls back to a base on the
  sandbox's private `/tmp`, and packages land as the caller (the archive
  itself still records uid/gid 0). tests/rootless.rs runs the full
  closure dance as an unprivileged user (as `nobody` when the runner is
  root).
* The build's `/build` is `/build` where that exists or is creatable (stage
  chroots, root nawa); an unprivileged host stages under the sandbox's fresh
  `/tmp` instead, with `%{srcdir}`/`%{pkgdir}` still expanding to whatever
  the build actually sees. Recipes must never hardcode `/build`.
* Integration tests (tests/sandbox.rs, tests/isolation.rs, tests/rootless.rs)
  probe the kernel first (`nawa sandbox-probe`) and skip with a message on
  restricted hosts.

Scope: `nawa build` and the bootstrap stages. Install-time behaviour is unchanged.

## 1. The problem today

`build.rs` runs each recipe section as `bash -e -c <script>` in the source
directory, with nawa's own environment, as whatever user runs nawa (root in the
stage chroots), against the live system. Consequences:

| # | What happens today | Why it hurts |
|---|--------------------|--------------|
| P1 | Build sees **everything installed in `/`** plus the seed in `/opt/seed` | `./configure` silently links optional libraries nobody declared; `depends.run` can be wrong without anyone noticing; two machines build different packages from the same recipe |
| P2 | Recipes list only *libraries*; seed tools (bash, coreutils, python3, rustc…) are "implicit" | there is no definition of "what a build may use"; the stage2 hack that deletes Debian's glibc headers is a symptom |
| P3 | Network is open during `configure/compile/install` | sources are sha256-pinned at fetch time, but a build script can still download anything |
| P4 | Build runs as root on the real filesystem | an `install` step that forgets `DESTDIR` writes straight into the running system and nobody notices |
| P5 | Leftover processes, `/tmp`, `$HOME`, locale, timezone, umask leak in | flaky and non-reproducible builds |
| P6 | Archives carry build-host ownership/mtimes and a wall-clock `build_date` | the same recipe never yields the same archive hash |
| P7 | Stage2 *installs each package into `/` before building the next* | building is coupled to mutating the system; the DB lock must be held while compiling |

Facts about the current tree that shape the design (measured on this repo):

* 147 recipes; **53 declare no build dependencies at all**, so they rely entirely on the implicit seed.
* Only one recipe (`nawa`) needs a build tool that fetches (cargo) and it already builds `--offline` from a vendored config — **no recipe needs network access to build**.
* `libc 0.2.191` is already in `Cargo.lock` (via `tar`), so adding it as a direct dependency of nawa costs nothing in the offline/vendored bootstrap.
* The `linux` recipe enables `OVERLAY_FS` but does not set `USER_NS` (it defaults to *off*) — must be added for the distro's own kernel (see §9).

## 2. Goals and non-goals

Goals

1. **Hermetic inputs**: a build can read only (a) its sources, (b) the declared build-dependency closure, (c) a fixed base toolchain set.
2. **No network** during build sections; everything fetched beforehand and hash-pinned.
3. **No side effects**: the only writable places are the source dir, `DESTDIR`, and a private `/tmp`. Everything else read-only.
4. **Clean, deterministic environment** and **byte-reproducible archives** (same recipe + same inputs ⇒ same sha256).
5. **Decouple building from installing** (fixes P7).
6. **Incremental**: each phase is useful alone, ships behind a switch, and keeps the current behaviour as `isolation = "none"` until proven.

Non-goals

* Not a security boundary against a hostile kernel exploit. Sources are sha256-pinned and recipes are trusted code; this stops *accidents and impurity*, like Gentoo's sandbox / Arch's clean chroot, not a determined attacker. Anything stronger = build inside a VM.
* No cross-compilation in this design. The closure model leaves room for it (§8) but does not implement it.

## 3. Isolation levels

One setting, ordered from cheap to strict. Each level includes the previous ones.

| Level | Adds | Fixes |
|-------|------|-------|
| `none` | today's behaviour | — |
| `env` | cleared environment, fixed locale/TZ/umask, `SOURCE_DATE_EPOCH`, normalised archives | P5, P6 |
| `sandbox` | user+mount+pid+net+ipc+uts namespaces over the **existing** root: no network, read-only root, private `/tmp`, `/dev`, `/proc`, PID-namespace cleanup | P3, P4, P5 |
| `closure` | root = **assembled build root** (base set + declared deps only), not the live system | P1, P2, P7 |

Selected by `[build] isolation` in the profile, overridable with `nawa build --isolation <level>`.
Default stays `none` until Phase 4 has rebuilt the whole tree cleanly, then flips to `closure`.

## 4. The sandbox

### 4.1 Mechanism

nawa implements it itself (no `bubblewrap` dependency — bwrap would itself have to be bootstrapped before the build that needs it).

* A hidden subcommand `nawa sandbox-init` is the sandbox's PID 1. `build.rs` spawns `nawa sandbox-init <spec.json>` instead of `bash` directly.
* `sandbox-init` does `unshare(CLONE_NEWUSER|NEWNS|NEWPID|NEWNET|NEWIPC|NEWUTS)`, the parent writes the uid/gid maps, then it forks the real build process as PID 1 of the new PID namespace and `wait`s. When PID 1 exits the kernel kills every straggler (background daemons started by test suites).
* Mount setup (all inside the new mount namespace): make `/` private → bind the build root onto itself → add the mounts of §4.2 → `pivot_root` into it (fall back to `chroot`; in a stage chroot `pivot_root` is not always possible, and with all capabilities dropped `chroot` is adequate for this threat model) → drop capabilities → `exec bash -e -c …`.
* Implemented with the `libc` crate (`unshare`, `mount`, `pivot_root`, `prctl`, `ioctl`). The one `unsafe` surface is confined to `sandbox.rs`.
* The spec (paths, env, limits) is passed as a JSON file, so the code that *decides* the sandbox is ordinary, unit-testable Rust and only `sandbox-init` touches the kernel.

### 4.2 Filesystem layout inside

```
/            build root, mounted read-only (the live system in `sandbox`, the assembled closure in `closure`)
/build/src   rw  bind of <builddir>/<id>/src          (%{srcdir})
/build/pkg   rw  bind of <builddir>/<id>/pkg          (%{pkgdir} / DESTDIR)
/build/home  rw  empty dir                            ($HOME)
/tmp         rw  fresh tmpfs (size-limited)
/dev         minimal: null zero full random urandom tty, devpts (newinstance) + ptmx, shm tmpfs
/proc        fresh procfs for the new PID namespace
/sys         not mounted (opt-in read-only for the few packages that probe it)
/etc/passwd, /etc/group   synthesised: root + nobody (+ "builder")
masked       /mydistro /build* /opt/seed /root /home /mnt /var/lib/nawa /var/cache/nawa  (empty tmpfs)
```

`%{srcdir}` and `%{pkgdir}` expand to `/build/src` and `/build/pkg` inside the sandbox
(recipes already use the placeholders, never host paths). Build logs and the
archive stay on the host side.

### 4.3 Identity ("fake root")

Builds run as uid 0 **inside** a user namespace that maps to an unprivileged host id, so
`install -o root`, `chown root`, setuid bits and `make install` all work, while on the host the
files belong to an unprivileged id and the process has no power over the real system.

* Run by real root (stage chroots): map `0 → caller's own id` (1 id) plus `1..65535 → 100000..`. The build process itself keeps the caller's kernel ids, so those must be mapped or it would read back as 65534 instead of root.
* Run by a normal user (later, §10 Phase 7): map `0 → <caller uid>` with `setgroups deny`.
* `archive::create` normalises every entry to `uid=0 gid=0` and clamps mtimes (§6), so the host-side ids never leak into archives. (Packages that need a non-root owner use a `post_install` hook, as on any distro.)

### 4.4 Network

New net namespace, only `lo`, brought up with `ioctl(SIOCSIFFLAGS)` (many test suites need loopback). No resolv.conf.
Escape hatch for a recipe that truly needs network: `[sandbox] network = true` **and** the user passes `--allow-network`; the archive's `.PKGINFO` records `impure = network` so it is visible. Today no recipe needs it. The intended answer for language ecosystems (cargo/go/pip) is a second pinned source (`[[source.extra]]`, vendored tarball + sha256) fetched *before* the sandbox, which is what the `nawa` recipe already does with its vendored cargo config.

### 4.5 Limits

Per-section wall-clock timeout (`[build] timeout`, default 4 h, kills the PID namespace), `RLIMIT_CORE=0`, `RLIMIT_NPROC` generous, optional cgroup memory cap later. `PR_SET_NO_NEW_PRIVS` set.

## 5. The build root (`closure` level)

### 5.1 What is in it

```
buildroot(R) = layers( base-devel  ∪  closure(R.depends.build) [∪ R.depends.check if --check] )

closure(S)   = S plus, transitively, each member's depends.run
```

* `base-devel` is a new meta recipe: gcc, binutils, glibc, linux-headers, make, bash, coreutils, sed, grep, gawk, findutils, diffutils, tar, gzip, xz, patch, pkgconf, file, perl, python… i.e. exactly what is now "implicit". After this, **`depends.build` means "libraries/headers/tools beyond base-devel"** and the "seed tools are implicit" comment in `recipe.rs` goes away.
* Run-deps are pulled in because linking against a library needs its dependencies present.
* `depends.check` is new: extra tools needed only for `--check` (e.g. dejagnu, expect).
* Nothing else exists in the build root. A missing declaration therefore fails as `command not found` / `cannot find -lfoo` — **the failure is the audit**.

### 5.2 Where the dependencies come from: archives, not the live system

The closure is assembled from **built archives** (`<builddir>/out/*.nawa.tar.zst`, or a signed repo), not from what happens to be installed in `/`. Consequences:

* Stage2 no longer needs to `nawa install` each package before building the next. It builds the whole graph into the archive cache, then installs once into the target. The build no longer needs the DB lock at all (P7 solved).
* Resolution maps `name → archive` through the recipe version (`<name>-<version>-<rel>`); a missing archive means "build that dependency first", which is what `deps-all` ordering already guarantees.
* Archives are verified (`sign::verify_for_install`) before being unpacked into a layer — the same trust policy as install.

### 5.3 Assembly strategies

| | Strategy | Cost | When |
|---|----------|------|------|
| v1 | extract every archive of the closure into `<builddir>/<id>/root`, delete afterwards | seconds per build; I/O heavy for gcc-sized closures | Phase 4 — simple, no kernel dependency beyond §4 |
| v2 | **layer cache**: each archive is extracted once to `<cache>/layers/<archive-sha256>/`; a build root is an **overlayfs** with those layers as `lowerdir`s and a throw-away `upperdir` | near-zero assembly, no copying | Phase 5 — needs overlayfs in a user namespace (kernel ≥ 5.11; probed per build, falls back to v1 with a note) |

The package DB already guarantees packages never own the same file, so layers merge without conflicts; the only shared paths are directories, which overlayfs merges. The merged-`/usr` symlinks (`/bin → usr/bin` …) come from `base-files`' layer.

### 5.4 Bootstrap interaction

The first packages (toolchain) *cannot* use `closure` — the base set does not exist yet. So:

| Stage | Level | Root |
|-------|-------|------|
| stage2 (toolchain + core) | `sandbox` (or `none` while bringing it up) | the seed + what stage2 has built so far |
| `base-devel` complete | — | archive set exists |
| **stage3** (rebuild the world with own toolchain) | **`closure`** | assembled from stage2's archives; the seed is no longer visible |

Stage3 already exists to rebuild everything with the distro's own toolchain; making it the first
fully hermetic pass means **a stage3 failure pinpoints a recipe with a missing or wrong dependency**,
and a successful stage3 proves the distro builds without the seed. This replaces the Debian-headers purge
in `stage2-inner.sh` for everything after the toolchain.

## 6. Environment and reproducibility (`env` level)

Environment passed to every section — nothing else is inherited:

```
PATH=/usr/bin:/usr/sbin        HOME=/build/home     TMPDIR=/tmp
LC_ALL=C                       TZ=UTC               SHELL=/bin/bash
SOURCE_DATE_EPOCH=<see below>  CFLAGS/CXXFLAGS/LDFLAGS from the profile
NAWA_JOBS  NAWA_PKGDIR  NAWA_SRCDIR   (existing; recipes pass -j%{jobs} themselves, MAKEFLAGS is not set)
umask 022
```

* `SOURCE_DATE_EPOCH` = newest mtime found in the extracted source tree (recorded in `.PKGINFO` as `source_date_epoch`); a recipe may pin it with `[package] source_date_epoch`.
* `.PKGINFO` loses the wall-clock `build_date`; install time is recorded in the DB at install (it already has `install_date`).
* `archive::create`: `uid=gid=0`, empty uname/gname, mtime = `min(mtime, SOURCE_DATE_EPOCH)`, entries sorted (already), fixed zstd level/params. The build-id (`sha256` of inputs, §7) goes in `.PKGINFO`.
* `nawa build --rebuild-check` builds twice in different directories at different times and compares archive hashes; on mismatch it lists the differing members. This is the regression test for everything above.

## 7. Post-build checks ("lint")

Run inside the sandbox on `/build/pkg` before the archive is created (needs only `readelf` from binutils, which is in `base-devel`; no new Rust dependency):

1. **Run-dependency check**: collect `DT_NEEDED` of every ELF in the package, map sonames → owning package through the closure's file index, compare with `depends.run`. Phase 6a: warn; 6b: error. This is what makes `depends.run` trustworthy.
2. **Leaked build paths**: any file containing `/build/src` or `/build/pkg` (rpath/runpath, `.la`, pkg-config files, scripts) ⇒ error with the file list.
3. **Layout**: nothing outside the allowed trees, no world-writable files, no `/usr/local`, no stray `/tmp`.
4. **Empty/doc-only package** warnings as today.

`.PKGINFO` also records `build_id = sha256(recipe.toml bytes ‖ source sha256 ‖ sorted closure archive sha256s ‖ profile flags)`, so "is this archive up to date?" becomes a hash comparison and a cache hit can skip the build.

## 8. Recipe / profile surface

```toml
# profiles/x86_64.toml
[build]
isolation = "closure"        # none | env | sandbox | closure
timeout   = 14400
# base set is the meta recipe `base-devel`; override only for experiments
# base = ["base-devel"]

# recipe.toml
[depends]
build = ["zlib", "openssl"]   # beyond base-devel (libraries, headers, extra tools)
run   = ["zlib"]
check = ["dejagnu"]           # only with --check

[sandbox]                     # rare overrides
network = false               # true requires --allow-network, recorded as impure
sysfs   = false               # mount /sys read-only
```

Cross-compilation later: split `build` into `tools` (run on the build machine, in `/usr`) and `host` (headers/libs for the target, in a sysroot). The closure code is written against "a list of (package, role)" now so that is an extension, not a rewrite.

## 9. Kernel and host requirements

| Need | Used for | Notes |
|------|----------|-------|
| `CONFIG_USER_NS`, `PID_NS`, `NET_NS`, `UTS_NS`, `IPC_NS` | sandbox | `USER_NS` defaults to **n** — add `-e USER_NS` (and verify the others) to `recipes/boot/linux`; the seed host's kernel must have them too |
| `CONFIG_UNIX98_PTYS`, `TMPFS` | `/dev/pts`, `/tmp` | check test suites that need ptys |
| `CONFIG_OVERLAY_FS` | layer cache (Phase 5) | already enabled; **user-namespace overlay needs kernel ≥ 5.11** |
| util-linux `unshare` | only `scripts/spike-sandbox.sh` | nawa itself does not use it |

If a requirement is missing, nawa reports it and **falls back one level** (`closure → sandbox → env`) only with `--allow-degraded`; by default it refuses, because silently weaker isolation defeats the purpose.

## 10. Phases (one commit/PR per phase, tests with each)

| Phase | Deliverable | Needs | Status |
|-------|-------------|-------|--------|
| **0** | `scripts/spike-sandbox.sh` ✔ — run it on the bootstrap host *and* inside the stage chroot to confirm §9 before writing code | — | done |
| **1** `env` | environment whitelist, umask, `SOURCE_DATE_EPOCH`, archive normalisation, drop `build_date`, `--rebuild-check` | nothing new | **done** |
| **2** `sandbox` (net + pid) | `libc` dep, `sandbox.rs`, `nawa sandbox-init`, net/pid/ipc/uts/user namespaces over the current root, timeouts, `/build/{src,pkg}` paths | Phase 0 green | **done** |
| **3** `sandbox` (fs) | read-only root, private `/tmp`,`/dev`,`/proc`, host-path masking | Phase 2 | **done** |
| **4** `closure` v1 | `base-devel` recipe, `depends.check`, closure resolver, extract-per-build root, archive-based dependency source, stage3 switched to `closure`, recipe audit fixes | Phases 1–3 | **done** (stage3 opt-in via `STAGE3_ISOLATION=closure`; flipping the default needs the full-tree rebuild first, §3) |
| **5** layer cache | `/var/cache/nawa/layers` (`NAWA_LAYER_CACHE`), overlayfs build root, `build_id` cache hits (`--use-cache`, opt-in) | Phase 4, kernel ≥ 5.11 | **done** (runtime probe per build; graceful extract-per-build fallback on older kernels — that path is what the restricted-container CI exercises) |
| **6** lint | ELF `NEEDED` vs `depends.run`, leaked paths, layout checks (warn → error) | Phase 4 | **done** (rundep check warns — 6a; leaks/layout error) |
| **7** rootless | uid map to caller, `nawa build` as a normal user; groundwork for cross `tools`/`host` split | Phase 4 | **done** (tests/rootless.rs runs the full closure build unprivileged; `setpriv --reuid nobody` when the runner is root) |

Phases 1–3 do not change what a recipe may *use*, only what a build may *do*, so they can land and be
used in stage2 immediately. Phase 4 is the only one that forces recipe edits.

## 11. Test plan

Pure unit tests (always run): closure computation (cycles, run-deps, role handling), sandbox spec generation, env whitelist, archive normalisation (ownership/mtime), `build_id` stability, soname → package mapping.

Integration tests (skip with a message when namespaces are unavailable — probe with `unshare(CLONE_NEWUSER)`):

* network: a listener on the host's `127.0.0.1:<port>` is **unreachable** from a build section; `lo` inside works.
* filesystem: writing under `/usr`, `/etc`, `/build/..` fails; `make install` without `DESTDIR` fails; `DESTDIR` writes succeed; `/tmp` is empty at start and gone afterwards.
* process cleanup: a section that backgrounds `sleep 1000` leaves no process after the build.
* environment: `env` output inside equals the whitelist exactly; host `CFLAGS`, `HOME`, `LANG` do not leak.
* identity: `id -u` is 0 inside; the resulting archive has `uid=0,gid=0` on every entry; host-side files are not root-owned.
* closure: a recipe using a tool not in the closure fails with `command not found`; adding it to `depends.build` makes it pass; an undeclared library is not found by `pkg-config`.
* reproducibility: `--rebuild-check` over a handful of real recipes (zlib, bzip2, sed) is a CI job.
* stage3 itself is the large-scale test for Phase 4: every failure is a dependency bug.

## 12. Open questions

1. **`pivot_root` inside the stage chroot** — expected to need the `chroot` fallback; confirm with the spike run *inside* `$LFS`.
2. **Where do archives live for stage3?** `/build/out` today; a local signed repo (`repo-create`) would let `closure` use one code path for local and remote dependencies.
3. **Should `build_id` cache hits skip builds by default?** Convenient, but needs a trustworthy `build_id`; ship it after `--rebuild-check` is green on the whole tree.
4. **Per-recipe opt-out of `closure`** (for toolchain pieces during bring-up): a `[sandbox] isolation = "sandbox"` override, removed once stage3 is hermetic.
5. **Non-root ownership in packages** (rare): decide between a `[package] owners` manifest and hooks.
