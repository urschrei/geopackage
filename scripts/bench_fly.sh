#!/usr/bin/env bash
#
# Run the benchmarks on a dedicated-CPU Fly.io machine. A `performance` CPU
# class has dedicated vCPUs, so anyone who rents the same machine type can
# reproduce the figures. Sprites and `shared` CPU classes are too noisy.
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
#   bench [rev] [reps]       build the local jj revision `rev` (default @)
#                            and run bench_datasets.sh over the dataset
#                            subsets, making them if needed
#   ab <refA> <refB> [iters] interleaved A/B of the admin write arm
#   quick <revA> <revB> [reps] [filter]
#                            compare two local jj revisions with
#                            quick_ab.sh on the dataset subsets
#   stop                     destroy the machine; the volume persists
#   destroy                  destroy the machine, the volume and the app
#
# Configuration, all overridable from the environment:
#   FLY_BENCH_APP     app name            (default geopackage-bench)
#   FLY_BENCH_ORG     organisation slug   (default personal)
#   FLY_BENCH_REGION  region              (default ams)
#   FLY_BENCH_VM      machine size        (default performance-8x)
#   FLY_BENCH_GDAL    conda-forge GDAL    (default 3.13.3)
#
# The machine size and memory are part of the protocol: published figures
# must name them, and comparisons are only valid within one machine type.

set -euo pipefail

APP="${FLY_BENCH_APP:-geopackage-bench}"
ORG="${FLY_BENCH_ORG:-personal}"
REGION="${FLY_BENCH_REGION:-ams}"
VM="${FLY_BENCH_VM:-performance-8x}"
GDAL="${FLY_BENCH_GDAL:-3.13.3}"
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

# Install the conda-forge GDAL named by FLY_BENCH_GDAL onto the volume, once,
# and the tools that time the runs onto this machine's rootfs.
emit_gdal() {
    cat <<EOF
if [[ ! -x /usr/bin/time ]] || ! command -v bzip2 >/dev/null || ! command -v rsync >/dev/null \
    || ! ls /usr/lib/linux-tools/*/perf >/dev/null 2>&1; then
    apt-get update -q
    apt-get install -q -y time bzip2 rsync linux-tools-common linux-tools-generic >/dev/null
fi
gdal_env=/data/mamba/envs/gdal-$GDAL
if [[ ! -x \$gdal_env/bin/ogr2ogr ]]; then
    echo "installing GDAL $GDAL from conda-forge onto the volume"
    mkdir -p /data/mamba
    [[ -x /data/mamba/bin/micromamba ]] || curl -fsSL \
        https://micro.mamba.pm/api/micromamba/linux-64/latest | tar -xj -C /data/mamba bin/micromamba
    /data/mamba/bin/micromamba create -y -q -r /data/mamba -p \$gdal_env -c conda-forge gdal=$GDAL
fi
EOF
}

# Sends local jj revisions, which need not be pushed, to /data/rev on the
# volume as `git archive`s, and prints their commit ids.
send_revs() {
    local tmp rev commit
    tmp="$(mktemp -d)"
    cmd_exec "mkdir -p /data/rev && find /data/rev -maxdepth 1 -type f -delete" >/dev/null
    for rev in "$@"; do
        commit="$(jj log -r "$rev" --no-graph -T 'commit_id')"
        git archive --format=tar.gz -o "$tmp/$commit.tar.gz" "$commit"
        printf 'put %s /data/rev/%s.tar.gz\n' "$tmp/$commit.tar.gz" "$commit" \
            | fly ssh sftp shell -a "$APP" >/dev/null
        echo "$commit"
    done
    rm -rf "$tmp"
}

# Shell lines that make the tree `$2` the sources of commit `$1`. rsync
# rewrites only the files whose contents differ, so cargo rebuilds only what
# changed; a file's archive mtime is its commit time, older than the last
# build, and cargo would otherwise reuse the old build.
emit_tree() {
    cat <<EOF
rm -rf /data/rev/src && mkdir /data/rev/src && tar -xzf /data/rev/$1.tar.gz -C /data/rev/src
mkdir -p $2 && rsync -rc --delete --exclude /target /data/rev/src/ $2/
EOF
}

# Two local revisions compared by quick_ab.sh. Builds are kept by commit, so
# the B of one comparison is the A of the next without a rebuild.
cmd_quick() {
    local rev_a="${1:?usage: bench_fly.sh quick <revA> <revB> [reps] [filter]}"
    local rev_b="${2:?usage: bench_fly.sh quick <revA> <revB> [reps] [filter]}"
    local reps="${3:-3}" filter="${4:-.}"
    local commits a b job
    commits="$(send_revs "$rev_a" "$rev_b")"
    a="$(sed -n 1p <<<"$commits")"
    b="$(sed -n 2p <<<"$commits")"
    local script
    for script in quick_ab.sh bench_datasets.sh; do
        printf 'put %s /data/rev/%s\n' "$(dirname "$0")/$script" "$script" \
            | fly ssh sftp shell -a "$APP" >/dev/null
    done
    job="$(mktemp)"
    {
        emit_gdal
        cat <<EOF
mkdir -p /data/quick-ab/bin
PATH=\$gdal_env/bin:\$PATH PROJ_DATA=\$gdal_env/share/proj GDAL_DATA=\$gdal_env/share/gdal \
    bash /data/rev/bench_datasets.sh subset /data/benchdata /data/quick
EOF
        local c
        for c in "$a" "$b"; do
            cat <<EOF
if [[ ! -x /data/quick-ab/bin/$c ]]; then
    echo "building ${c:0:12}"
$(emit_tree "$c" /data/quick-ab/tree)
    (cd /data/quick-ab/tree && CARGO_TARGET_DIR=/data/quick-ab/target cargo build --release --quiet \
        -p geopackage --features arrow,bundled --example dataset_bench)
    cp /data/quick-ab/target/release/examples/dataset_bench /data/quick-ab/bin/$c
fi
EOF
        done
        echo "bash /data/rev/quick_ab.sh /data/quick-ab/bin/$a /data/quick-ab/bin/$b /data/quick $reps '$filter'"
    } > "$job"
    remote_run "$job"
    rm -f "$job"
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
    local rev="${1:-@}" reps="${2:-3}"
    local commit job
    commit="$(send_revs "$rev")"
    job="$(mktemp)"
    cat > "$job" <<EOF
$(emit_gdal)
$(emit_tree "$commit" /data/bench-tree)
cd /data/bench-tree
PATH=\$gdal_env/bin:\$PATH PROJ_DATA=\$gdal_env/share/proj GDAL_DATA=\$gdal_env/share/gdal \
    bash scripts/bench_datasets.sh subset /data/benchdata /data/quick
GDAL_CONFIG=\$gdal_env/bin/gdal-config bash scripts/bench_datasets.sh run /data/quick $reps \
    2>&1 | tee /data/logs/bench-${commit:0:12}.log
EOF
    remote_run "$job"
    rm -f "$job"
}

# Interleaved A/B of the admin write arm. Alternating A B A B on one machine
# cancels drift that repeated runs of a single binary cannot.
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
    quick)   shift; cmd_quick "$@" ;;
    stop)    cmd_stop ;;
    destroy) cmd_destroy ;;
    *)
        sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
        exit 1
        ;;
esac
