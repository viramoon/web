#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
snapshot="$root/openapi/finance.openapi.json"
provenance="$root/openapi/finance.commit"
client="$root/crates/finance-client"
version=7.27.0
checksum=db9c4b738727698748746144394cb7ee96512fe50166555baaeeca9311ee1dba
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

usage() {
    cat >&2 <<'USAGE'
Usage:
  gen-finance-client.sh from-finance <local-finance-checkout> <full-commit-sha>
  gen-finance-client.sh from-snapshot [<external-snapshot> <full-finance-sha>]
  gen-finance-client.sh --check [<external-snapshot>]
Set OPENAPI_GENERATOR_JAR to use a local copy of the pinned OpenAPI Generator CLI.
USAGE
    exit 2
}
check_sha() { [[ $1 =~ ^[[:xdigit:]]{40}$ ]] || usage; }

mode=${1:-}
case "$mode" in
    from-finance)
        [[ $# == 3 ]] || usage
        sha=$3
        check_sha "$sha"
        # Export from the named commit even if the checkout has local modifications.
        mkdir "$work/finance"
        sha=$(git -C "$2" rev-parse --verify "$sha^{commit}")
        git -C "$2" archive "$sha" | tar -x -C "$work/finance"
        (cd "$work/finance" && cargo run --locked --quiet -p finance-api -- export-openapi) > "$work/finance.json"
        input="$work/finance.json"
        ;;
    from-snapshot)
        case $# in
            1) input=$snapshot; sha=$(cat "$provenance") ;;
            3) input=$2; sha=$3 ;;
            *) usage ;;
        esac
        check_sha "$sha"
        ;;
    --check)
        [[ $# == 1 || $# == 2 ]] || usage
        sha=$(cat "$provenance")
        check_sha "$sha"
        input=$snapshot
        if [[ $# == 2 ]]; then cmp -- "$2" "$snapshot"; fi
        ;;
    *) usage ;;
esac

jar=${OPENAPI_GENERATOR_JAR:-${XDG_CACHE_HOME:-$HOME/.cache}/openapi-generator/openapi-generator-cli-$version.jar}
if [[ ! -f $jar && -z ${OPENAPI_GENERATOR_JAR:-} ]]; then
    mkdir -p "$(dirname "$jar")"
    download=$(mktemp "$(dirname "$jar")/download.XXXXXX")
    trap 'rm -rf -- "$work"; rm -f -- "$download"' EXIT
    curl --fail --location --silent --show-error --retry 2 \
        "https://repo.maven.apache.org/maven2/org/openapitools/openapi-generator-cli/$version/openapi-generator-cli-$version.jar" -o "$download"
    printf '%s  %s\n' "$checksum" "$download" | sha256sum --check --status
    mv -- "$download" "$jar"
fi
printf '%s  %s\n' "$checksum" "$jar" | sha256sum --check

mkdir -p "$work/classes" "$work/templates/reqwest"
javac -proc:none --release 17 -cp "$jar" -d "$work/classes" "$root/scripts/openapi/FinanceNumbers.java"
for template in Cargo.mustache model.mustache reqwest/api.mustache reqwest/api_mod.mustache; do
    unzip -p "$jar" "rust/$template" > "$work/templates/$template"
done
patch -s -p1 --fuzz=0 -d "$work/templates" < "$root/scripts/openapi/rust-templates.patch"
java -XX:ActiveProcessorCount=2 -Xmx384m -cp "$jar:$work/classes" org.openapitools.codegen.OpenAPIGenerator generate \
    -i "$input" -g rust -t "$work/templates" -o "$work/generated" \
    --openapi-normalizer NORMALIZER_CLASS=FinanceNumbers \
    --type-mappings 'integer+uint64=u64,integer+uint32=u32' --language-specific-primitives u32,u64 \
    --additional-properties 'packageName=finance-client,library=reqwest,hideGenerationTimestamp=true,useChrono=false' \
    --global-property 'apiDocs=false,modelDocs=false,apiTests=false,modelTests=false'
# Keep the generator's crate sources and manifest. Templates also emit release helpers
# and project documentation, which are outside this path dependency.
(cd "$root" && rustfmt --edition 2021 "$work/generated/src/lib.rs")
if [[ $mode == --check ]]; then
    diff -u "$client/Cargo.toml" "$work/generated/Cargo.toml"
    diff -ru "$client/src" "$work/generated/src"
else
    mkdir -p "$client" "$(dirname "$snapshot")"
    if [[ ! $input -ef $snapshot ]]; then cp -- "$input" "$snapshot"; fi
    printf '%s\n' "$sha" > "$provenance"
    # This directory is reserved for generated modules. Drop models removed by the new contract.
    rm -rf -- "$client/src"
    cp -R -- "$work/generated/src" "$client/src"
    cp -- "$work/generated/Cargo.toml" "$client/Cargo.toml"
fi
printf 'finance %s · generator %s · %s\n' "$sha" "$version" "$mode"
