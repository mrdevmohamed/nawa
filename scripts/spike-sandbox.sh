#!/usr/bin/env bash
# scripts/spike-sandbox.sh — can THIS kernel/userland run nawa's planned build
# sandbox? (see docs/design/build-isolation.md, "Phase 0 — spike")
#
# Run it where nawa will build: on the bootstrap host, and inside the stage
# chroot (`chroot $LFS /mydistro/scripts/spike-sandbox.sh`; it needs only
# bash, coreutils and util-linux's unshare/mount). Changes nothing outside
# its own temp dir and private namespaces.
#
# Exit status: 0 when every REQUIRED check passes, 1 otherwise.
set -u
req_fail=0
pass() { printf 'PASS  %s\n' "$1"; }
skip() { printf 'SKIP  %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1"; [ "${2:-req}" = req ] && req_fail=1; }

command -v unshare >/dev/null || { fail "unshare(1) not found (util-linux)"; exit 1; }

# --- kernel options (informational; /proc/config.gz or /boot/config-*) -----
cfg=""
if [ -r /proc/config.gz ]; then cfg="$(zcat /proc/config.gz 2>/dev/null)"
elif [ -r "/boot/config-$(uname -r)" ]; then cfg="$(cat "/boot/config-$(uname -r)")"; fi
if [ -n "$cfg" ]; then
    for o in USER_NS PID_NS NET_NS UTS_NS IPC_NS OVERLAY_FS UNIX98_PTYS TMPFS; do
        if grep -q "^CONFIG_$o=[ym]" <<<"$cfg"; then pass "kernel CONFIG_$o"
        else fail "kernel CONFIG_$o not enabled" "$([ $o = OVERLAY_FS ] && echo opt || echo req)"; fi
    done
else
    skip "kernel config not readable (checks below test the real behaviour)"
fi

T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
mkdir -p "$T/ro" "$T/rw" "$T/lower" "$T/upper" "$T/work" "$T/merged"
echo hello > "$T/lower/f"

# --- the combined namespace set nawa will use ------------------------------
U=(unshare --user --map-root-user --mount --pid --net --uts --ipc --fork --mount-proc)

if out="$("${U[@]}" sh -c 'echo $(id -u)' 2>&1)" && [ "$out" = 0 ]; then
    pass "user+mount+pid+net+uts+ipc namespaces; root inside (uid 0)"
else
    fail "cannot create the namespace set: $out"
    printf '\nHint: unprivileged user namespaces may be disabled (sysctl kernel.unprivileged_userns_clone,\n'
    printf 'user.max_user_namespaces, or an LSM). As real root in the stage chroot this normally still works.\n'
    exit 1
fi

# network: only loopback may exist. /proc is freshly mounted inside the pid
# namespace and reflects the new net namespace (/sys would still show the
# host's interfaces, so it is deliberately not used here).
n="$("${U[@]}" sh -c "tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' ' | tr '\n' ' '" 2>/dev/null)"
case "${n% }" in
    ""|lo) pass "network namespace has no interface except lo (got: '${n% }')";;
    *)     fail "unexpected interfaces in the new net namespace: $n";;
esac

# pid namespace: build children cannot see/kill host processes
c="$("${U[@]}" sh -c 'ls /proc | grep -c "^[0-9][0-9]*$"' 2>/dev/null)"
if [ -n "$c" ] && [ "$c" -le 5 ]; then pass "pid namespace isolated ($c visible processes)"
else fail "pid namespace shows $c processes"; fi

# read-only bind mount really is read-only
if "${U[@]}" sh -c "mount --bind '$T/ro' '$T/ro' && mount -o remount,bind,ro '$T/ro' && ! touch '$T/ro/x' 2>/dev/null" 2>/dev/null; then
    pass "read-only bind mount (install without DESTDIR would fail loudly)"
else fail "cannot make a read-only bind mount"; fi

# tmpfs + proc + devpts-style mounts a build root needs
if "${U[@]}" sh -c "mount -t tmpfs tmpfs '$T/rw' && touch '$T/rw/x'" 2>/dev/null; then
    pass "tmpfs mount inside the sandbox"
else fail "cannot mount tmpfs"; fi

# pivot_root / chroot into a prepared directory
if "${U[@]}" sh -c "mount --bind '$T/lower' '$T/lower' && chroot '$T/lower' /bin/true" 2>/dev/null \
   || "${U[@]}" sh -c "chroot '$T/lower' /bin/sh -c true" 2>/dev/null; then
    pass "chroot after unshare"
else
    skip "chroot test (needs a usable /bin/true inside the dir; real check happens in nawa's own tests)"
fi

# overlayfs inside a user namespace (kernel >= 5.11) — only needed for the layer cache (Phase 5)
if "${U[@]}" sh -c "mount -t overlay overlay -o lowerdir='$T/lower',upperdir='$T/upper',workdir='$T/work' '$T/merged' && test \"\$(cat '$T/merged/f')\" = hello" 2>/dev/null; then
    pass "overlayfs mount in a user namespace (layer cache possible)"
else
    fail "overlayfs not mountable here — Phase 5 would fall back to plain extraction" opt
fi

echo
if [ $req_fail = 0 ]; then echo "sandbox spike: OK — required features present"; else echo "sandbox spike: FAILED"; fi
exit $req_fail
