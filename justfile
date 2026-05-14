# Default: show available targets
default:
    @just --list

# Install development tool dependencies
[group('development')] 
install-dev-deps:
    cargo install ratchets@0.2.6

# Build and install the local version of pristine
[group('development')] 
install:
    cargo install --path="."

# Run tests via cargo-nextest
[group('test')]
test:
    cargo nextest run

# Launch the Python JSON-RPC client
[group('development')]
chat:
    uv run client.py

specs-bookmark := "specs"
specs-manifest := "spec-files"

# Snapshot spec-files (and itself) onto a new commit on the `specs` bookmark
[group('specs')]
export-specs:
    #!/usr/bin/env bash
    set -euo pipefail
    manifest="{{specs-manifest}}"
    bookmark="{{specs-bookmark}}"
    if [[ ! -f "$manifest" ]]; then
        echo "manifest '$manifest' not found in working copy" >&2
        exit 1
    fi
    if jj bookmark list "$bookmark" 2>/dev/null | grep -q "^$bookmark:"; then
        parent="$bookmark"
        had_bookmark=1
    else
        parent='root()'
        had_bookmark=0
    fi
    mapfile -t files < <(grep -vE '^[[:space:]]*(#|$)' "$manifest")
    files+=("$manifest")
    tag="__specs_export_${$}_$(date +%s%N)__"
    jj new --no-edit -m "$tag" "$parent"
    new_id=$(jj log --no-graph -r "description(exact:\"$tag\n\")" -T 'change_id' --limit 1)
    if [[ -z "$new_id" ]]; then
        echo "failed to locate freshly created commit" >&2
        exit 1
    fi
    jj restore --into "$new_id" --from 'root()'
    jj restore --into "$new_id" --from @ -- "${files[@]}"
    msg="Spec snapshot $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    jj describe "$new_id" -m "$msg"
    if [[ "$had_bookmark" -eq 1 ]]; then
        jj bookmark move "$bookmark" --to "$new_id"
    else
        jj bookmark create "$bookmark" -r "$new_id"
    fi
    echo "$bookmark -> $new_id ($msg)"

# Restore spec files from the `specs` bookmark into the working copy
[group('specs')]
restore-specs:
    #!/usr/bin/env bash
    set -euo pipefail
    manifest="{{specs-manifest}}"
    bookmark="{{specs-bookmark}}"
    if ! jj bookmark list "$bookmark" 2>/dev/null | grep -q "^$bookmark:"; then
        echo "bookmark '$bookmark' does not exist" >&2
        exit 1
    fi
    jj restore --into @ --from "$bookmark" -- "$manifest"
    if [[ ! -f "$manifest" ]]; then
        echo "specs bookmark has no '$manifest' file; nothing to restore" >&2
        exit 1
    fi
    mapfile -t files < <(grep -vE '^[[:space:]]*(#|$)' "$manifest")
    if [[ "${#files[@]}" -gt 0 ]]; then
        jj restore --into @ --from "$bookmark" -- "${files[@]}"
    fi
    echo "restored ${#files[@]} manifest entries from $bookmark"
