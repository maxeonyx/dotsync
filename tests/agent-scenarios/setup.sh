#!/usr/bin/env bash
# Setup script for agent testing scenarios.
# Creates an isolated dotsync environment in a temp directory.
#
# Usage: ./setup.sh [output-dir]
# If output-dir is not specified, creates a tempdir and prints its path.

set -euo pipefail

DOTSYNC_BIN="${DOTSYNC_BIN:-dotsync}"
OUTPUT_DIR="${1:-$(mktemp -d /tmp/dotsync-agent-test.XXXXXX)}"

echo "Setting up agent test environment in: $OUTPUT_DIR"

# Create fake home structure
FAKE_HOME="$OUTPUT_DIR/home"
mkdir -p "$FAKE_HOME"

# Create a bare git remote with scope branches
REMOTE_DIR="$OUTPUT_DIR/remote.git"
git init --bare "$REMOTE_DIR" >/dev/null 2>&1

# Helper to create a branch on the remote with initial content
create_branch() {
    local branch="$1"
    local clone_dir="$OUTPUT_DIR/setup-$branch"
    
    if git -C "$REMOTE_DIR" rev-parse --verify "$branch" >/dev/null 2>&1; then
        git clone --branch "$branch" --single-branch "$REMOTE_DIR" "$clone_dir" >/dev/null 2>&1
    else
        git clone "$REMOTE_DIR" "$clone_dir" >/dev/null 2>&1 || git init "$clone_dir" >/dev/null 2>&1
        cd "$clone_dir"
        git checkout -b "$branch" 2>/dev/null || true
    fi
    
    cd "$clone_dir"
    echo "$clone_dir"
}

commit_and_push() {
    local clone_dir="$1"
    local branch="$2"
    local message="$3"
    cd "$clone_dir"
    git add .
    # Only commit if there are staged changes
    if ! git diff --cached --quiet 2>/dev/null; then
        GIT_AUTHOR_NAME="test" GIT_AUTHOR_EMAIL="test@test.com" \
        GIT_COMMITTER_NAME="test" GIT_COMMITTER_EMAIL="test@test.com" \
        git commit -m "$message" >/dev/null 2>&1
    fi
    git push origin "$branch" >/dev/null 2>&1
    rm -rf "$clone_dir"
}

# Create 'all' scope with config
ALL_DIR=$(create_branch "all")
mkdir -p "$ALL_DIR/.config/dotsync"
cat > "$ALL_DIR/.config/dotsync/config.toml" << 'EOF'
# dotsync scope configuration
# Scopes form a DAG — changes cascade from parent to child.

[scopes]
# Universal config — applies to every machine
all = {}
# Linux-specific config
linux = { parents = ["all"] }
# Windows-specific config  
windows = { parents = ["all"] }
# Hyprland desktop environment config (linux only)
hyprland = { parents = ["linux"] }
# Max's XPS laptop
mx-xps-cy = { parents = ["hyprland"] }

[sync]
state_path = ".local/state/dotsync/sync-state.json"
EOF

# Add a universal gitconfig
cat > "$ALL_DIR/.gitconfig" << 'EOF'
[user]
    name = Test User
    email = test@example.com
[init]
    defaultBranch = main
EOF
commit_and_push "$ALL_DIR" "all" "initial all scope"

# Create linux scope (inherits from all)
LINUX_DIR=$(create_branch "linux")
# Merge from all
cd "$LINUX_DIR"
git merge origin/all --no-edit -m "merge all into linux" >/dev/null 2>&1 || true
mkdir -p "$LINUX_DIR/.config/fish"
cat > "$LINUX_DIR/.config/fish/config.fish" << 'EOF'
# Fish shell config (linux)
set -gx EDITOR nvim
set -gx PATH $HOME/.local/bin $PATH
EOF
commit_and_push "$LINUX_DIR" "linux" "initial linux scope"

# Create hyprland scope
HYPR_DIR=$(create_branch "hyprland")
cd "$HYPR_DIR"
git merge origin/linux --no-edit -m "merge linux into hyprland" >/dev/null 2>&1 || true
mkdir -p "$HYPR_DIR/.config/hypr"
cat > "$HYPR_DIR/.config/hypr/hyprland.conf" << 'EOF'
# Hyprland config
monitor=,preferred,auto,1
input {
    kb_layout = us
}
EOF
commit_and_push "$HYPR_DIR" "hyprland" "initial hyprland scope"

# Create machine scope
MACHINE_DIR=$(create_branch "mx-xps-cy")
cd "$MACHINE_DIR"
git merge origin/hyprland --no-edit -m "merge hyprland into mx-xps-cy" >/dev/null 2>&1 || true
commit_and_push "$MACHINE_DIR" "mx-xps-cy" "initial mx-xps-cy scope"

# Now init dotsync in the fake home
cd "$OUTPUT_DIR"
export HOME="$FAKE_HOME"
export DOTSYNC_OS="linux"
export DOTSYNC_HOSTNAME="mx-xps-cy"
"$DOTSYNC_BIN" init "$REMOTE_DIR"

echo ""
echo "=== Agent test environment ready ==="
echo "FAKE_HOME=$FAKE_HOME"
echo "REMOTE_DIR=$REMOTE_DIR"
echo ""
echo "To use:"
echo "  export HOME=$FAKE_HOME"
echo "  export DOTSYNC_OS=linux"
echo "  export DOTSYNC_HOSTNAME=mx-xps-cy"
echo ""
echo "Current managed files:"
ls -la "$FAKE_HOME/.gitconfig" "$FAKE_HOME/.config/fish/config.fish" "$FAKE_HOME/.config/hypr/hyprland.conf" 2>/dev/null || true
