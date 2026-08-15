#!/usr/bin/env bash
#
# Run the dataset benchmarks on a dedicated-CPU Fly.io machine.
#
# scripts/bench_datasets.sh requires an otherwise idle host, and a shared
# workstation rarely is one: two full runs on 2026-08-15 disagreed with the
# published table on arms the code had not touched, under load averages of 5
# to 10. A Fly machine with a `performance` CPU class has dedicated vCPUs and
# no other tenants, so its figures are reproducible by anyone who rents the
# same machine type. Sprites and `shared` CPU classes are not suitable: their
# timing noise is the problem this script exists to remove.
#
# Everything durable lives on one volume mounted at /data: the datasets, the
# Rust toolchain, the source checkouts and their build artifacts, and the run
# logs. The machine itself is disposable and is destroyed between sessions;
# the volume persists and is the only ongoing cost besides machine seconds.
#
# Subcommands:
#   setup                    create the app and the data volume (one-time)
#   start                    create and boot the benchmark machine
#   exec <cmd>               run one shell command on the machine
#   upload [file...]         copy local benchdata .gpkg files onto the volume
#   fetch                    download and convert the datasets onto the volume
#   bench [ref] [reps]       build `ref` and run bench_datasets.sh over it
#   ab <refA> <refB> [iters] interleaved A/B of the admin write arm
#   stop                     destroy the machine; the volume persists
#   destroy                  destroy the machine, the volume and the app
#
# Configuration, all overridable from the environment:
#   FLY_BENCH_APP     app name            (default geopackage-bench)
#   FLY_BENCH_ORG     organisation slug   (default personal)
#   FLY_BENCH_REGION  region              (default ams)
#   FLY_BENCH_VM      machine size        (default performance-8x)
#
# The machine size and memory are part of the protocol: published figures
# must name them, and comparisons are only valid within one machine type.

set -euo pipefail

APP="${FLY_BENCH_APP:-geopackage-bench}"
ORG="${FLY_BENCH_ORG:-personal}"
REGION="${FLY_BENCH_REGION:-ams}"
VM="${FLY_BENCH_VM:-performance-8x}"
MEMORY_MB=16384
VOLUME=bench_data
VOLUME_GB=25
IMAGE=ubuntu:24.04
MACHINE_NAME=bench
REPO_URL=https://github.com/urschrei/geopackage

# Every remote job starts from this preamble: strict mode, the persistent
# toolchain on the volume, and a one-time dependency install guarded by a
# marker file so repeat runs skip it.
preamble() {
    cat <<'EOF'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
export RUSTUP_HOME=/data/rust/rustup
export CARGO_HOME=/data/rust/cargo
export PATH="/data/rust/cargo/bin:$PATH"
export TMPDIR=/data/tmp
mkdir -p /data/tmp /data/logs
# Two state layers with different lifetimes: apt packages live on the
# machine's throwaway rootfs, the toolchain on the persistent volume. Each
# is tested directly, since a marker file cannot span both.
if ! command -v git >/dev/null 2>&1; then
    echo "installing system packages onto this machine's rootfs"
    apt-get update -q
    apt-get install -q -y build-essential curl git unzip ca-certificates \
        gdal-bin python3
fi
if [[ ! -x /data/rust/cargo/bin/cargo ]]; then
    echo "installing the Rust toolchain onto the volume"
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal \
        --default-toolchain stable
fi
EOF
}

# Run a generated script on the machine. The script travels as base64 so no
# quoting layer between here and the remote shell can rewrite it.
remote_run() {
    local script="$1"
    local b64
    b64="$( { preamble; cat "$script"; } | base64 | tr -d '\n')"
    fly ssh console -a "$APP" \
        -C "bash -c 'echo $b64 | base64 -d > /tmp/job.sh && bash /tmp/job.sh'"
}

# Clone or update one checkout on the volume and build the measurement tool
# at `ref`. Bundled SQLite is explicit rather than left to workspace feature
# unification, so the build is the same whatever else the workspace grows.
emit_build() {
    local dir="$1" ref="$2"
    cat <<EOF
if [[ ! -d $dir/.git ]]; then
    git clone --quiet $REPO_URL $dir
fi
git -C $dir fetch --quiet --tags origin
git -C $dir checkout --quiet --detach $ref
echo "building dataset_bench at \$(git -C $dir rev-parse --short HEAD)"
(cd $dir && cargo build --release --quiet -p geopackage \
    --features arrow,bundled --example dataset_bench)
EOF
}

cmd_setup() {
    fly apps create "$APP" --org "$ORG"
    fly volumes create "$VOLUME" --app "$APP" --region "$REGION" \
        --size "$VOLUME_GB" --yes
}

cmd_start() {
    fly machine run "$IMAGE" sleep inf \
        --app "$APP" --region "$REGION" --name "$MACHINE_NAME" \
        --vm-size "$VM" --vm-memory "$MEMORY_MB" \
        --volume "$VOLUME:/data"
}

cmd_exec() {
    local job
    job="$(mktemp)"
    printf '%s\n' "$*" > "$job"
    remote_run "$job"
    rm -f "$job"
}

# Prefer this over `fetch` when the datasets already exist locally: it skips
# the downloads (some dataset hosts are unreachable from Fly regions) and the
# ogr2ogr conversions. The transfer is bound by the local uplink and runs
# through flyctl's sftp, which needs nothing installed on the machine.
cmd_upload() {
    local files=("$@")
    if [[ ${#files[@]} -eq 0 ]]; then
        files=("$(dirname "$0")/../benchdata"/*.gpkg)
    fi
    cmd_exec 'mkdir -p /data/benchdata'
    local f base
    for f in "${files[@]}"; do
        # The local benchdata directory contains empty stray files; an empty
        # dataset is never right, so it is skipped rather than uploaded.
        [[ -s "$f" ]] || continue
        base="$(basename "$f")"
        echo "uploading $base"
        printf 'put %s /data/benchdata/%s\n' "$f" "$base" \
            | fly ssh sftp shell -a "$APP"
    done
    cmd_exec 'ls -la /data/benchdata/'
}

cmd_fetch() {
    local job
    job="$(mktemp)"
    cat > "$job" <<EOF
$(emit_build /data/src origin/main)
bash /data/src/scripts/bench_datasets.sh fetch /data/benchdata \
    2>&1 | tee /data/logs/fetch.log
EOF
    remote_run "$job"
    rm -f "$job"
}

cmd_bench() {
    local ref="${1:-origin/main}" reps="${2:-3}"
    local job
    job="$(mktemp)"
    cat > "$job" <<EOF
$(emit_build /data/src "$ref")
bash /data/src/scripts/bench_datasets.sh run /data/benchdata $reps \
    2>&1 | tee "/data/logs/bench-\$(git -C /data/src rev-parse --short HEAD).log"
EOF
    remote_run "$job"
    rm -f "$job"
}

# Interleaved A/B of the admin write arm, the arm whose 2026-08-15 local
# figures disagreed with the published table. Alternating A B A B on one
# machine cancels drift that repeated runs of a single binary cannot, which
# is what separates a host effect from a code change.
cmd_ab() {
    local ref_a="$1" ref_b="$2" iters="${3:-5}"
    local job
    job="$(mktemp)"
    cat > "$job" <<EOF
$(emit_build /data/ab-a "$ref_a")
$(emit_build /data/ab-b "$ref_b")
a_bin=/data/ab-a/target/release/examples/dataset_bench
b_bin=/data/ab-b/target/release/examples/dataset_bench
declare -a a_ms b_ms
for i in \$(seq 1 $iters); do
    for side in a b; do
        bin_var="\${side}_bin"
        rm -f /data/tmp/ab.gpkg
        out="\$("\${!bin_var}" write /data/benchdata/gadm_noidx.gpkg gadm \
            /data/tmp/ab.gpkg no | grep elapsed_ms)"
        ms="\${out#elapsed_ms=}"
        echo "iter \$i \$side: \$ms ms"
        if [[ \$side == a ]]; then a_ms+=("\$ms"); else b_ms+=("\$ms"); fi
    done
done
rm -f /data/tmp/ab.gpkg
python3 - "\${a_ms[@]}" -- "\${b_ms[@]}" <<'PY'
import sys
args = sys.argv[1:]
split = args.index("--")
a = sorted(float(x) for x in args[:split])
b = sorted(float(x) for x in args[split + 1:])
med = lambda xs: xs[len(xs) // 2]
print(f"A ($ref_a): median {med(a):.0f} ms, min {a[0]:.0f}, max {a[-1]:.0f}")
print(f"B ($ref_b): median {med(b):.0f} ms, min {b[0]:.0f}, max {b[-1]:.0f}")
PY
EOF
    remote_run "$job" 2>&1 | tee "/tmp/bench_fly_ab.log"
    rm -f "$job"
}

machine_id() {
    fly machine list -a "$APP" --json \
        | python3 -c 'import json,sys; ms=json.load(sys.stdin); print(ms[0]["id"] if ms else "")'
}

cmd_stop() {
    local id
    id="$(machine_id)"
    [[ -n "$id" ]] && fly machine destroy "$id" -a "$APP" --force
}

cmd_destroy() {
    cmd_stop || true
    fly volumes list -a "$APP" --json \
        | python3 -c 'import json,sys; [print(v["id"]) for v in json.load(sys.stdin)]' \
        | while read -r vol; do fly volumes destroy "$vol" -a "$APP" --yes; done
    fly apps destroy "$APP" --yes
}

case "${1:-}" in
    setup)   cmd_setup ;;
    start)   cmd_start ;;
    exec)    shift; cmd_exec "$@" ;;
    upload)  shift; cmd_upload "$@" ;;
    fetch)   cmd_fetch ;;
    bench)   shift; cmd_bench "$@" ;;
    ab)      shift; cmd_ab "$@" ;;
    stop)    cmd_stop ;;
    destroy) cmd_destroy ;;
    *)
        sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
        exit 1
        ;;
esac
