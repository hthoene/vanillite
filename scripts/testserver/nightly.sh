#!/usr/bin/env bash
# Nightly heavy tests on vanillite-test2 (#179). vanillite-nightly (timer, 00:30 Europe/Berlin)
# runs this file of the newest green dev commit in /opt/vanillite-work/src as user
# vanillite-work while it holds the server's lock: `nightly.sh <out dir> <commit>`.
# Every step gets a time limit; its output goes to <out dir>/<name>.log and one line per step
# to <out dir>/summary.md. All steps together must end before 03:00 Europe/Berlin, when the
# benchmark suite starts on vanillite-test (same Proxmox host). Add new heavy nightly tests
# (soak, session replay, ...) as steps here.
set -uo pipefail
out=${1:?usage: nightly.sh <out dir> <commit>}
commit=${2:-unknown}
failed=0

{
    echo "Nightly heavy tests of dev ${commit:0:12}, started $(date -u '+%Y-%m-%d %H:%M UTC')"
    echo
    echo "| step | result | minutes |"
    echo "|---|---|---|"
} > "$out/summary.md"

# step <name> <time limit in minutes> <command...>
step() {
    local name=$1 limit=$2 started=$SECONDS status result
    shift 2
    timeout --signal=INT --kill-after=120 "${limit}m" "$@" > "$out/$name.log" 2>&1
    status=$?
    case $status in
        0) result=ok ;;
        124) result="time limit (${limit} min)" ;;
        *) result="FAILED ($status)" ;;
    esac
    [ "$status" = 0 ] || failed=1
    echo "| $name | $result | $(((SECONDS - started) / 60)) |" >> "$out/summary.md"
}

# Minutes left until 02:50 Europe/Berlin; a run started by hand at another time gets 140.
minutes_left() {
    local now end
    now=$(date +%s)
    end=$(TZ=Europe/Berlin date -d '02:50' +%s)
    if [ "$end" -le "$now" ] || [ $((end - now)) -gt 10800 ]; then
        end=$((started_at + 140 * 60))
    fi
    echo $(((end - now) / 60))
}
started_at=$(date +%s)

rustup toolchain install --profile minimal > /dev/null 2>&1
step build 30 bash -c 'cargo build --locked -q -p xtask -p vanillite-test --bins &&
    cargo build --locked -q --profile release-fast -p vanillite-test --bin parity_soak --bin parity_fuzz &&
    cargo test --locked -q --release -p vanillite-worldgen --test vanilla_sweep --no-run &&
    cargo test --locked -q --release -p vanillite-light --test vanilla_light_sweep --no-run'

# Bulk parity scenarios: records missing or stale bulk goldens and runs the ignored tests.
step golden-bulk 35 cargo xtask golden regen --bulk

# World generation against Vanilla (#223): new world seeds every night, every dimension and
# world type (origin area and a far area each).
step worldgen-sweep 30 cargo xtask worldgen sweep --seed "$(($(date -u +%s) / 86400))" --seeds 4
[ -f target/worldgen-sweep/summary.md ] && cp target/worldgen-sweep/summary.md "$out/worldgen-sweep.md"

# Complex real-world worlds (#244): the corpus of crates/vanillite-test/parity-worlds.toml
# (downloads and upgraded copies cached in ~/.cache/vanillite/parity-worlds/, Vanilla's runs in
# the bulk store), every world tick by tick against Vanilla. Results next to the sweep's.
step parity-worlds 40 cargo xtask parity worlds --jobs 3 --minutes 25
[ -f target/parity-worlds/summary.md ] && cp target/parity-worlds/summary.md "$out/parity-worlds.md"
[ -d target/parity-worlds ] && cp -r target/parity-worlds "$out/parity-worlds"

# Light against Vanilla (#237): after generation and after scripted block changes, other new
# world seeds every night, every dimension and world type.
step light-sweep 20 cargo xtask light sweep --seed "$(($(date -u +%s) / 86400 + 500000))" --seeds 4
[ -f target/light-sweep/summary.md ] && cp target/light-sweep/summary.md "$out/light-sweep.md"

# Session replays (#152, #257): the recordings of the players on vanillite-test 30001 that no
# nightly replayed yet, against Vanilla.
step session-replay 50 env VANILLITE_PARITY_PROFILE=release-fast bash scripts/testserver/replay-recordings.sh "$HOME/recordings" "$out"

# Coverage of Vanilla's code by the parity scenarios (#257, #267): every scenario on the pinned
# jar under JaCoCo, two 2 GiB recorders (the host has 6 GiB). Each night's gameplay and
# per-system figures go to coverage-history.jsonl next to the nightly runs; coverage-trend.md
# there is the readable trend (`cargo xtask parity coverage --trend <history>` prints it too).
history=$(dirname "$out")/coverage-history.jsonl
rm -f target/parity-coverage/summary.md
step parity-coverage 50 cargo xtask parity coverage --jobs 2 --heap 2g --history "$history"
[ -f target/parity-coverage/summary.md ] && cp target/parity-coverage/summary.md "$out/parity-coverage.md"
[ -f "$history" ] && cargo xtask parity coverage --trend "$history" > "$(dirname "$out")/coverage-trend.md" 2>/dev/null

# Weak hardware (#270): 20 minutes of a mixed survival load (explorers, builders, mob and
# redstone farms) on one core with 1 GiB; MSPT, TPS, RSS and chunk delivery in result.json.
step weak-hardware 35 cargo xtask loadtest weak --minutes 20
weak=$(ls -d target/loadtest-weak/2* 2> /dev/null | tail -n 1)
[ -n "$weak" ] && cp "$weak/result.json" "$out/weak-hardware.json"

# Soak parity runs (#166): game days against Vanilla, per-tick checksums; Vanilla's run is
# recorded once per golden key, so only a new jar, harness or scenario costs its recording.
soak=$(($(minutes_left) / 2))
[ "$soak" -gt 45 ] && soak=45
step parity-soak $((soak + 10)) cargo xtask parity soak --minutes "$soak"
[ -d target/parity-soak ] && cp -r target/parity-soak "$out/parity-soak"

# Random parity scenarios (#132): a new seed range every night, for the time that is left.
seed=$(($(date -u +%s) / 86400 * 1000))
fuzz=$(($(minutes_left) - 10))
if [ "$fuzz" -ge 5 ]; then
    step parity-fuzz $((fuzz + 8)) cargo xtask parity fuzz --seed "$seed" --minutes "$fuzz"
    [ -d target/parity-fuzz ] && cp -r target/parity-fuzz "$out/parity-fuzz"
else
    echo "| parity-fuzz | skipped (no time left) | 0 |" >> "$out/summary.md"
fi

if [ -f "$out/worldgen-sweep.md" ]; then
    { echo; echo "## worldgen sweep"; sed '1d' "$out/worldgen-sweep.md"; } >> "$out/summary.md"
fi
if [ -f "$out/light-sweep.md" ]; then
    { echo; echo "## light sweep"; sed '1d' "$out/light-sweep.md"; } >> "$out/summary.md"
fi
if [ -f "$out/session-replay/summary.md" ]; then
    { echo; echo "## session replay"; sed '1d' "$out/session-replay/summary.md"; } >> "$out/summary.md"
fi
if [ -f "$out/parity-worlds.md" ]; then
    { echo; echo "## parity worlds"; sed '1d' "$out/parity-worlds.md"; } >> "$out/summary.md"
fi
if [ -f "$out/parity-coverage.md" ]; then
    { echo; echo "## parity coverage"; sed -n '/^## Per system/,/^## The/p' "$out/parity-coverage.md" | sed '1d;$d'; } >> "$out/summary.md"
fi
if [ -f "$out/parity-soak/summary.md" ]; then
    { echo; echo "## parity soak"; cat "$out/parity-soak/summary.md"; } >> "$out/summary.md"
fi
if [ -f "$out/parity-fuzz/summary.md" ]; then
    { echo; echo "## parity fuzz (seeds from $seed)"; cat "$out/parity-fuzz/summary.md"; } >> "$out/summary.md"
fi
echo >> "$out/summary.md"
echo "Finished $(date -u '+%Y-%m-%d %H:%M UTC')" >> "$out/summary.md"
exit "$failed"
