#!/usr/bin/env bash
# Provisions vanillite-test2, the second test server (#179): internal only (players cannot reach
# it), for heavy tests and experiments that need no player, so they keep vanillite-test's
# benchmark lock free. Idempotent: safe to run again at any time.
# Run with `cargo xtask testserver --host vanillite-test2 provision` (pipes this script to
# `ssh <host> bash -s`).
# - Work area /opt/vanillite-work (user vanillite-work, own rustup, JDK 25):
#   `cargo xtask testserver run -- <xtask args>` copies the local checkout to
#   /opt/vanillite-work/src and runs `cargo xtask <args>` there while holding the lock (parity
#   fuzz, golden regen, session replay, soak, ...). One build dir (src/target), pruned when the
#   disk runs low.
# - Lock: `vanillite-lock <owner>` (flock on /run/vanillite-lock), the same protocol as
#   `vanillite-bench lock` on vanillite-test: prints "busy <owner>" while another run holds it,
#   then "locked", and holds it until stdin closes. `vanillite-lock status` shows the holder.
# - Nightly heavy tests: vanillite-nightly.timer (00:30 Europe/Berlin, done before the 03:00
#   benchmark on vanillite-test, which shares the Proxmox host) runs scripts/testserver/nightly.sh
#   of the newest green dev commit under the lock; results in /var/lib/vanillite-nightly/<run>/
#   (`vanillite-nightly status`, `cargo xtask testserver --host vanillite-test2 nightly`).
#   Its session replay step copies the new recordings of vanillite-test (ssh host
#   vanillite-recordings of vanillite-work, read-only key; provision vanillite-test after this).
# - Vanilla 26.3 reference server /opt/vanilla (vanilla.service, port 30900, RCON 25578) and
#   Paper 26.3 /opt/paper (paper.service, port 30000, RCON 25575) for comparisons and plugin
#   spikes: offline mode, started only on demand (`systemctl start paper`, stop afterwards).
#   `paper-rcon [--port <port>] <command>` talks to both.
# - Vanillite instances for experiments on ports 30001-30999 (/opt/vanillite/<port>,
#   vanillite@<port>.service, offline mode, console `vanillite-console <port> <command>`),
#   deployed with `cargo xtask deploy --host vanillite-test2 --port <port>`.
set -euo pipefail

PAPER_VERSION=26.3
PAPER_BUILD=262
PAPER_SHA256=b2d24a0abe99f481b35504b119e35ac721013f59d9c670aa9a4d05814a81617b
PAPER_URL="https://fill-data.papermc.io/v1/objects/${PAPER_SHA256}/paper-${PAPER_VERSION}-${PAPER_BUILD}.jar"
VANILLA_VERSION=26.3
VANILLA_SHA1=33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c

# Writes stdin to $1 only when the content changed; prints the path when it did.
write_if_changed() {
    local target=$1 tmp
    tmp=$(mktemp)
    cat > "$tmp"
    if [ -f "$target" ] && cmp -s "$tmp" "$target"; then
        rm -f "$tmp"
    else
        install -m "${2:-644}" "$tmp" "$target"
        rm -f "$tmp"
        echo "updated $target"
    fi
}

# Downloads $2 to $1 unless present and checks it with `<tool>sum -c` ($3: tool, $4: hash).
fetch() {
    local target=$1 url=$2 tool=$3 hash=$4
    [ -f "$target" ] && return 0
    curl -fsSL -o "$target.part" "$url"
    echo "$hash  $target.part" | "${tool}sum" -c -
    mv "$target.part" "$target"
}

echo "== packages"
export DEBIAN_FRONTEND=noninteractive
missing=()
for pkg in openjdk-25-jdk-headless curl rsync jq python3 git build-essential pkg-config; do
    dpkg -s "$pkg" > /dev/null 2>&1 || missing+=("$pkg")
done
if [ ${#missing[@]} -gt 0 ]; then
    apt-get update -q
    apt-get install -y -q "${missing[@]}"
fi
timedatectl set-timezone Etc/UTC 2> /dev/null || true

# Docker engine with buildx for image tests (`DOCKER_HOST=ssh://vanillite-test2 cargo xtask
# docker`). qemu-user runs arm64 binaries (`qemu-aarch64 -L <image rootfs> ...`); binfmt cannot be
# registered in this unprivileged container, so arm64 images themselves do not run in Docker here.
echo "== docker"
if ! command -v docker > /dev/null; then
    . /etc/os-release
    install -m 0755 -d /etc/apt/keyrings
    curl -fsSL "https://download.docker.com/linux/${ID}/gpg" | gpg --dearmor --yes -o /etc/apt/keyrings/docker.gpg
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/${ID} ${VERSION_CODENAME} stable" \
        > /etc/apt/sources.list.d/docker.list
    apt-get update -q
    apt-get install -y -q docker-ce docker-ce-cli containerd.io docker-buildx-plugin
fi
systemctl enable --now docker > /dev/null
dpkg -s qemu-user > /dev/null 2>&1 || apt-get install -y -q qemu-user

echo "== users"
id minecraft > /dev/null 2>&1 || useradd --system --home-dir /opt/paper --create-home --shell /usr/sbin/nologin minecraft
id vanillite > /dev/null 2>&1 || useradd --system --home-dir /opt/vanillite --create-home --shell /usr/sbin/nologin vanillite
id vanillite-work > /dev/null 2>&1 \
    || useradd --system --home-dir /opt/vanillite-work --create-home --shell /usr/sbin/nologin vanillite-work
install -d -o vanillite -g vanillite /opt/vanillite
install -d -o vanillite-work -g vanillite-work /opt/vanillite-work /opt/vanillite-work/src

echo "== lock (vanillite-lock <owner> | status)"
write_if_changed /usr/local/bin/vanillite-lock 755 <<'EOF'
#!/usr/bin/env bash
# Usage: vanillite-lock <owner>   wait for the lock of this server (/run/vanillite-lock); prints
#                                 "busy <owner>" while another run holds it, then "locked", and
#                                 holds it until stdin closes (xtask testserver run takes it)
#        vanillite-lock status    who holds it
# Installed by scripts/testserver/provision-test2.sh.
set -uo pipefail
case "${1:-}" in
    status)
        if flock -n /run/vanillite-lock true; then
            echo free
        else
            echo "held by $(cat /run/vanillite-lock.owner 2> /dev/null)"
        fi
        ;;
    "")
        echo "usage: vanillite-lock <owner> | status" >&2
        exit 2
        ;;
    *)
        exec 9>> /run/vanillite-lock
        if ! flock -n 9; then
            echo "busy $(cat /run/vanillite-lock.owner 2> /dev/null)"
            flock 9
        fi
        echo "$1 since $(date -u '+%Y-%m-%d %H:%M UTC')" > /run/vanillite-lock.owner
        echo locked
        # The lock (fd 9) lives as long as this process: until the caller closes stdin.
        cat > /dev/null
        ;;
esac
EOF

echo "== work area (/opt/vanillite-work: rustup, vanillite-work run)"
if [ ! -x /opt/vanillite-work/.cargo/bin/rustup ]; then
    curl -fsSL https://sh.rustup.rs | runuser -u vanillite-work -- env HOME=/opt/vanillite-work \
        sh -s -- -y --quiet --no-modify-path --profile minimal --default-toolchain none
fi
write_if_changed /usr/local/bin/vanillite-work 755 <<'EOF'
#!/usr/bin/env bash
# Usage: vanillite-work run <xtask args...>   run `cargo xtask <args>` in /opt/vanillite-work/src
#                                             as user vanillite-work (the caller holds the lock:
#                                             `cargo xtask testserver run`, vanillite-nightly)
#        vanillite-work prune                 free disk space (also before every run)
# One build dir (src/target, no incremental files); when less than 6 GiB are free its build
# outputs are deleted (the Vanilla jars and golden caches under target/ stay).
# Installed by scripts/testserver/provision-test2.sh.
set -uo pipefail
work=/opt/vanillite-work
free_gib() { df --output=avail -BG / | tail -n 1 | tr -dc 0-9; }
prune() {
    rm -rf "$work"/src/target/*/incremental
    if [ "$(free_gib)" -lt 6 ]; then
        echo "less than 6 GiB free: deleting the build outputs in $work/src/target" >&2
        rm -rf "$work"/src/target/{debug,release,release-fast,profiling,tmp}
    fi
}
as_work() {
    runuser -u vanillite-work -- env HOME="$work" PATH="$work/.cargo/bin:/usr/bin:/bin" \
        JAVA_HOME=/usr/lib/jvm/java-25-openjdk-amd64 CARGO_INCREMENTAL=0 "$@"
}
case "${1:-}" in
    prune) prune ;;
    run)
        shift
        prune
        cd "$work/src" || exit 1
        as_work rustup toolchain install --profile minimal > /dev/null 2>&1
        as_work cargo xtask "$@"
        ;;
    *)
        echo "usage: vanillite-work run <xtask args...> | prune" >&2
        exit 2
        ;;
esac
EOF

echo "== session recordings of vanillite-test (nightly replay, #257)"
# The nightly copies the new recordings of Vanillite 30001 on vanillite-test with this key
# (ssh host vanillite-recordings of user vanillite-work); vanillite-test lets it only read them
# (`cargo xtask testserver provision` there authorizes the key, here it passes the address).
install -d -m 700 -o vanillite-work -g vanillite-work /opt/vanillite-work/.ssh
if [ ! -f /opt/vanillite-work/.ssh/recordings ]; then
    runuser -u vanillite-work -- ssh-keygen -q -t ed25519 -N '' -C vanillite-test2-recordings \
        -f /opt/vanillite-work/.ssh/recordings
fi
if [ -n "${VANILLITE_RECORDINGS_HOST:-}" ]; then
    write_if_changed /opt/vanillite-work/.ssh/config <<EOF
Host vanillite-recordings
    HostName $VANILLITE_RECORDINGS_HOST
    User root
    IdentityFile ~/.ssh/recordings
    IdentitiesOnly yes
    BatchMode yes
    StrictHostKeyChecking accept-new
EOF
fi

echo "== vanillite-limited (one core and a memory limit for cargo xtask loadtest weak, #270)"
write_if_changed /usr/local/bin/vanillite-limited 755 <<'EOF'
#!/usr/bin/env bash
# Usage: sudo vanillite-limited <cpu> <memory MiB> <command...>
# Runs the command as the calling user in a systemd scope with a CPU quota of one core and a
# memory limit without swap, pinned to CPU <cpu> (AllowedCPUs does not apply in this
# container): the server of `cargo xtask loadtest weak` (#270), run by vanillite-work.
# Installed by scripts/testserver/provision-test2.sh.
set -euo pipefail
usage="usage: sudo vanillite-limited <cpu> <memory MiB> <command...>"
cpu=${1:?$usage}
mem=${2:?$usage}
shift 2
if ! [[ $cpu =~ ^[0-9]+$ && $mem =~ ^[0-9]+$ ]] || [ "$#" -eq 0 ]; then
    echo "$usage" >&2
    exit 2
fi
exec systemd-run --scope --quiet --uid="${SUDO_UID:?run through sudo}" \
    --gid="${SUDO_GID:?run through sudo}" -p CPUQuota=100% -p "MemoryMax=${mem}M" \
    -p MemorySwapMax=0 -- taskset -c "$cpu" "$@"
EOF
tmp=$(mktemp)
echo 'vanillite-work ALL=(root) NOPASSWD: /usr/local/bin/vanillite-limited' > "$tmp"
if visudo -cqf "$tmp"; then
    write_if_changed /etc/sudoers.d/vanillite-limited 440 < "$tmp"
fi
rm -f "$tmp"

echo "== nightly heavy tests (vanillite-nightly.timer, 00:30 Europe/Berlin)"
install -d /var/lib/vanillite-nightly
write_if_changed /usr/local/bin/vanillite-nightly 755 <<'EOF'
#!/usr/bin/env bash
# Usage: vanillite-nightly [run | status]
# run (vanillite-nightly.timer): takes the lock, puts the newest dev commit whose CI workflow
# passed into /opt/vanillite-work/src and runs its scripts/testserver/nightly.sh (the steps live
# in the repository) with the output dir /var/lib/vanillite-nightly/<date>-<commit>/ (summary.md
# and one log per step). Keeps the newest 14 runs.
# status: the summaries of the newest runs.
# Installed by scripts/testserver/provision-test2.sh.
set -uo pipefail
api=https://api.github.com/repos/hthoene/vanillite
repo=https://github.com/hthoene/vanillite.git
work=/opt/vanillite-work
state=/var/lib/vanillite-nightly

case "${1:-run}" in
    status)
        for dir in $(ls -1d "$state"/*/ 2> /dev/null | sort | tail -n "${2:-3}"); do
            echo "== $(basename "$dir")"
            cat "$dir/summary.md" 2> /dev/null || echo "(no summary yet: still running?)"
        done
        exit 0
        ;;
    run) ;;
    *)
        echo "usage: vanillite-nightly [run | status [<count>]]" >&2
        exit 2
        ;;
esac

exec 9>> /run/vanillite-lock
flock -w 3600 9 || { echo "lock held for an hour by $(cat /run/vanillite-lock.owner)" >&2; exit 1; }
echo "vanillite-nightly since $(date -u '+%Y-%m-%d %H:%M UTC')" > /run/vanillite-lock.owner

sha=$(curl -fsS -H 'Accept: application/vnd.github+json' \
    "$api/actions/workflows/ci.yml/runs?branch=dev&event=push&status=success&per_page=1" \
    | jq -r '.workflow_runs[0].head_sha // empty')
[ -n "$sha" ] || { echo "no green dev commit found (GitHub API unavailable?)" >&2; exit 1; }
git_work() {
    runuser -u vanillite-work -- git --git-dir="$work/repo.git" --work-tree="$work/src" "$@"
}
[ -d "$work/repo.git" ] || runuser -u vanillite-work -- git clone --quiet --bare "$repo" "$work/repo.git"
git_work fetch --quiet origin "+refs/heads/dev:refs/heads/dev" || { echo "cannot fetch dev" >&2; exit 1; }
git_work reset --quiet --hard "$sha"
git_work clean -qffdx -e /target
out=$state/$(date -u +%Y%m%d-%H%M)-${sha:0:7}
install -d -o vanillite-work -g vanillite-work "$out"
echo "$sha" > "$out/commit"
vanillite-work prune
cd "$work/src" || exit 1
runuser -u vanillite-work -- env HOME="$work" PATH="$work/.cargo/bin:/usr/bin:/bin" \
    JAVA_HOME=/usr/lib/jvm/java-25-openjdk-amd64 CARGO_INCREMENTAL=0 \
    bash scripts/testserver/nightly.sh "$out" "$sha"
status=$?
ls -1d "$state"/*/ | sort | head -n -14 | xargs -r rm -rf
exit "$status"
EOF
write_if_changed /etc/systemd/system/vanillite-nightly.service <<'EOF'
[Unit]
Description=Nightly heavy tests of the newest green dev (scripts/testserver/nightly.sh)
After=network-online.target
Wants=network-online.target
[Service]
Type=oneshot
ExecStart=/usr/local/bin/vanillite-nightly run
TimeoutStartSec=4h
EOF
write_if_changed /etc/systemd/system/vanillite-nightly.timer <<'EOF'
[Unit]
Description=Nightly heavy tests on vanillite-test2 (done before the 03:00 benchmark on vanillite-test)
[Timer]
OnCalendar=*-*-* 00:30:00 Europe/Berlin
Persistent=false
[Install]
WantedBy=timers.target
EOF

echo "== rcon (paper-rcon [--port <port>] <command>, password in /root/.paper-rcon)"
if [ ! -s /root/.paper-rcon ]; then
    (umask 077 && head -c 24 /dev/urandom | base64 | tr -d '/+=' > /root/.paper-rcon)
fi
rcon_password=$(cat /root/.paper-rcon)
write_if_changed /usr/local/bin/paper-rcon 755 <<'EOF'
#!/usr/bin/env python3
"""Sends one console command to a local Paper or Vanilla server via RCON.

Usage: paper-rcon [--port <port>] <command...>  (25575: Paper, 25578: Vanilla)
Installed by scripts/testserver/provision-test2.sh."""
import socket, struct, sys
args = sys.argv[1:]
port = 25575
if args[:1] == ["--port"]:
    port, args = int(args[1]), args[2:]
pw = open("/root/.paper-rcon").read().strip()
def pkt(i, t, body):
    b = body.encode() + b"\x00\x00"
    return struct.pack("<iii", len(b) + 8, i, t) + b
def recv(s):
    n = struct.unpack("<i", s.recv(4))[0]
    data = b""
    while len(data) < n:
        data += s.recv(n - len(data))
    i, t = struct.unpack("<ii", data[:8])
    return i, data[8:-2].decode(errors="replace")
s = socket.create_connection(("127.0.0.1", port), timeout=5)
s.sendall(pkt(1, 3, pw))
if recv(s)[0] == -1:
    sys.exit("RCON auth failed")
s.sendall(pkt(2, 2, " ".join(args)))
print(recv(s)[1])
EOF

# A reference server dir: jar link, EULA (accepted by the owner for this test machine) and the
# offline settings with RCON. $1 dir, $2 port, $3 RCON port.
reference_dir() {
    if [ ! -f "$1/server.properties" ]; then
        echo "eula=true" > "$1/eula.txt"
        printf 'server-port=%s\nonline-mode=false\nwhite-list=false\nenable-rcon=true\nrcon.port=%s\nrcon.password=%s\n' \
            "$2" "$3" "$rcon_password" > "$1/server.properties"
        chown minecraft:minecraft "$1/eula.txt" "$1/server.properties"
    fi
}

echo "== paper 26.3 (port 30000, on demand)"
install -d -o minecraft -g minecraft /opt/paper /opt/paper/plugins
fetch "/opt/paper/paper-${PAPER_VERSION}-${PAPER_BUILD}.jar" "$PAPER_URL" sha256 "$PAPER_SHA256"
ln -sfn "paper-${PAPER_VERSION}-${PAPER_BUILD}.jar" /opt/paper/paper.jar
reference_dir /opt/paper 30000 25575
write_if_changed /etc/systemd/system/paper.service <<'EOF'
[Unit]
Description=Paper 26.3 (offline mode, port 30000, on demand: stop it afterwards)
After=network-online.target
[Service]
User=minecraft
WorkingDirectory=/opt/paper
ExecStart=/usr/bin/java -Xms512M -Xmx2G -jar paper.jar --nogui
LimitNOFILE=65536
SuccessExitStatus=143
EOF

echo "== vanilla 26.3 (port 30900, on demand)"
install -d -o minecraft -g minecraft /opt/vanilla
fetch "/opt/vanilla/server-$VANILLA_VERSION.jar" \
    "https://piston-data.mojang.com/v1/objects/$VANILLA_SHA1/server.jar" sha1 "$VANILLA_SHA1"
ln -sfn "server-$VANILLA_VERSION.jar" /opt/vanilla/server.jar
reference_dir /opt/vanilla 30900 25578
write_if_changed /etc/systemd/system/vanilla.service <<'EOF'
[Unit]
Description=Vanilla 26.3 (offline mode, port 30900, on demand: stop it afterwards)
After=network-online.target
[Service]
User=minecraft
WorkingDirectory=/opt/vanilla
ExecStart=/usr/bin/java -Xms512M -Xmx2G -jar server.jar --nogui
LimitNOFILE=65536
SuccessExitStatus=143
EOF

echo "== vanillite instances (ports 30001-30999)"
write_if_changed /etc/systemd/system/vanillite@.service <<'EOF'
[Unit]
Description=Vanillite experiment on port %i
After=network-online.target vanillite-console@%i.socket
Wants=network-online.target
Requires=vanillite-console@%i.socket

[Service]
User=vanillite
Group=vanillite
WorkingDirectory=/opt/vanillite/%i
ExecStart=/opt/vanillite/%i/vanillite
# Console commands: `vanillite-console <port> <command>` writes into this FIFO.
Sockets=vanillite-console@%i.socket
StandardInput=socket
StandardOutput=journal
StandardError=journal
Environment=RUST_BACKTRACE=1
Restart=on-failure
RestartSec=5
KillSignal=SIGTERM
TimeoutStopSec=60
LimitNOFILE=65536
MemoryMax=2G
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/opt/vanillite/%i

[Install]
WantedBy=multi-user.target
EOF
# The console of an instance (its stdin) is a FIFO that systemd keeps open, so it never ends.
write_if_changed /etc/systemd/system/vanillite-console@.socket <<'EOF'
[Unit]
Description=Console FIFO of Vanillite instance %i
[Socket]
ListenFIFO=/run/vanillite-console/%i
SocketMode=0600
RemoveOnStop=true
Service=vanillite@%i.service
EOF
write_if_changed /usr/local/bin/vanillite-console 755 <<'EOF'
#!/usr/bin/env bash
# Usage: vanillite-console <port> <command...>
# Runs a console command on a running Vanillite instance (e.g. `vanillite-console 30001 say hi`).
# Installed by scripts/testserver/provision-test2.sh.
set -euo pipefail
port=${1:?usage: vanillite-console <port> <command...>}
shift
fifo=/run/vanillite-console/$port
# No FIFO: an instance started before it existed. Never create a plain file in its place: the
# socket could then not start, nor the instance with it.
if ! systemctl is-active --quiet "vanillite@$port" || [ ! -p "$fifo" ]; then
    echo "vanillite@$port is not running with a console" >&2
    exit 1
fi
printf '%s\n' "$*" > "$fifo"
EOF
write_if_changed /usr/local/bin/vanillite-deploy 755 <<'EOF'
#!/usr/bin/env bash
# Usage: vanillite-deploy <port> < vanillite-binary
# Installs the binary from stdin as /opt/vanillite/<port>/vanillite (offline mode, no whitelist:
# this server is internal only) and (re)starts vanillite@<port>.service. Instances are not
# enabled at boot: stop them when done (`cargo xtask testserver --host vanillite-test2 stop <port>`).
# Installed by scripts/testserver/provision-test2.sh.
set -euo pipefail
port=${1:?usage: vanillite-deploy <port> < binary}
if ! [[ "$port" =~ ^[0-9]+$ ]] || [ "$port" -lt 30001 ] || [ "$port" -gt 30999 ] || [ "$port" = 30900 ]; then
    echo "port must be 30001-30999 (30000 is Paper, 30900 Vanilla)" >&2
    exit 2
fi
dir=/opt/vanillite/$port
install -d -o vanillite -g vanillite "$dir"
tmp=$(mktemp -p "$dir" .vanillite.XXXXXX)
cat > "$tmp"
[ -s "$tmp" ] || { rm -f "$tmp"; echo "no binary on stdin" >&2; exit 2; }
chmod 755 "$tmp"
chown vanillite:vanillite "$tmp"
systemctl stop "vanillite@$port" 2> /dev/null || true
mv -f "$tmp" "$dir/vanillite"
props=$dir/server.properties
touch "$props"
set_prop() {
    if grep -q "^$1=" "$props"; then
        sed -i "s|^$1=.*|$1=$2|" "$props"
    else
        echo "$1=$2" >> "$props"
    fi
}
set_prop server-port "$port"
set_prop online-mode false
set_prop white-list false
# The Minecraft EULA is accepted for this test server.
echo "eula=true" > "$dir/eula.txt"
chown vanillite:vanillite "$props" "$dir/eula.txt"
systemctl restart "vanillite@$port"
sleep 1
systemctl --no-pager --lines=10 status "vanillite@$port" || true
EOF

systemctl daemon-reload
systemctl enable --now vanillite-nightly.timer > /dev/null
echo "== done"
systemctl --no-pager list-timers vanillite-nightly.timer | head -n 2
df -h / | tail -n 1
