#!/usr/bin/env bash
# Build driver: cargo with the build tree in build/ instead of target/.
set -euo pipefail

# --cont[=IMAGE] | --cont [IMAGE]: rerun this build inside a build container (sp-cont-build)
for a in "$@"; do [[ $a == --cont || $a == --cont=* ]] && exec sp-cont-build "$0" "$@"; done

NAME=ai-session-manager
MUSL=x86_64-unknown-linux-musl
cd "$(dirname "$0")"

usage() {
    cat <<EOF
Usage: build.sh [OPTION]... [debug|release]

    debug|release       cargo profile (default: release)
    --musl              build the static $MUSL binary (what releases ship)
    --clippy            also run clippy with -D warnings (the CI gate)
    --test              also run the tests
    --clean             remove build/ and exit
    --cont[=IMAGE]      run this same build inside a build container (default image
                        rl8-build, via sp-cont-build); its build/ is cont_build/ here
    -h, --help          this text

The binary lands in build/[<target>/]<profile>/$NAME.
EOF
}

profile=release target='' clippy=0 test=0
while (($#)); do
    case $1 in
        debug | release) profile=$1 ;;
        --musl) target=$MUSL ;;
        --clippy) clippy=1 ;;
        --test) test=1 ;;
        --clean) rm -rf build; echo "removed build/"; exit 0 ;;
        -h | --help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
    shift
done

args=(--locked --target-dir build)
[[ $profile == release ]] && args+=(--release)
[[ -n $target ]] && args+=(--target "$target")

cargo build "${args[@]}"
if ((clippy)); then cargo clippy "${args[@]}" --all-targets -- -D warnings; fi
if ((test)); then cargo test "${args[@]}"; fi

bin=build/${target:+$target/}$profile/$NAME
"$bin" --help >/dev/null
echo "Built: $PWD/$bin"
