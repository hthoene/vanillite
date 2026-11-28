#!/usr/bin/env bash
# Release metadata from the workspace version and the generated Minecraft version, as
# key=value lines (append them to $GITHUB_OUTPUT in workflows).
#
#   scripts/release/meta.sh [<git tag>]
#
# With a tag it fails unless the tag is exactly v<version>+mc<minecraft version>.
# Outputs: version, mc, tag, name (archive prefix), prerelease (true/false) and docker_tags
# (comma-separated): 0.1.0-beta.1-mc26.3, mc26.3-beta, beta for a pre-release and
# 0.1.0-mc26.3, mc26.3, latest plus mc26.3-beta, beta for a stable version (the beta tags always
# hold the newest build). latest is the newest release: a pre-release moves it only while no
# stable release exists (no git tag v<x.y.z>+mc<id>), a stable release always; a pre-release
# never moves mc<id>. The tags of a whole channel (beta, latest) move only for the newest
# Minecraft version of deploy/minecraft-versions.txt, so an update for an older Minecraft version
# does not take them back.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)"
mc="$(sed -n 's/^id = "\(.*\)"$/\1/p' "$root/data/vanilla/version.toml")"
[[ -n "$version" && -n "$mc" ]] || { echo "cannot read the version or the Minecraft version" >&2; exit 1; }
tag="v${version}+mc${mc}"

if [[ $# -gt 0 && "$1" != "$tag" ]]; then
    echo "tag $1 does not match the sources: expected $tag (Cargo.toml version $version," \
        "Minecraft $mc)" >&2
    exit 1
fi

versions="$root/deploy/minecraft-versions.txt"
newest="$(awk '!/^#/ && NF { print $1; exit }' "$versions")"
if ! awk -v mc="$mc" '$1 == mc { found = 1 } END { exit !found }' "$versions"; then
    echo "Minecraft $mc is not in deploy/minecraft-versions.txt; run \`cargo xtask egg\`" >&2
    exit 1
fi

if [[ "$version" == *-* ]]; then
    prerelease=true
    # The channel is the first identifier of the pre-release part: beta of 0.1.0-beta.1.
    channel="${version#*-}"
    channel="${channel%%.*}"
    docker_tags="${version}-mc${mc},mc${mc}-${channel}"
    if [[ "$mc" == "$newest" ]]; then
        docker_tags+=",${channel}"
        git -C "$root" tag -l 'v*' | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+\+mc' >/dev/null \
            || docker_tags+=",latest"
    fi
else
    prerelease=false
    docker_tags="${version}-mc${mc},mc${mc},mc${mc}-beta"
    [[ "$mc" == "$newest" ]] && docker_tags+=",latest,beta"
fi

echo "version=$version"
echo "mc=$mc"
echo "tag=$tag"
echo "name=vanillite-${version}-mc${mc}"
echo "prerelease=$prerelease"
echo "docker_tags=$docker_tags"
