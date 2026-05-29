# Agent Testing Scenarios

These scenarios validate that an AI agent can correctly use dotsync by following the `docs/SKILL.md` instructions.

## Setup

```bash
chmod +x setup.sh
./setup.sh /tmp/dotsync-test
```

This creates an isolated environment with:
- A fake `$HOME` with dotsync initialized
- Scope branches: `all -> linux -> hyprland -> mx-xps-cy`
- Pre-seeded files: `.gitconfig` (all), `.config/fish/config.fish` (linux), `.config/hypr/hyprland.conf` (hyprland)

## Scenarios

### 1. Add universal config

**Task for agent:** "Add a `.config/starship.toml` with `add_newline = false` — this applies to all machines."

**Expected:**
- Agent edits `~/.config/starship.toml`
- Agent runs `dotsync all -m "add starship config" -- .config/starship.toml`
- File exists in `all` scope tree
- File cascaded to all descendants

### 2. Modify OS-specific config

**Task for agent:** "Add `set -gx RUST_BACKTRACE 1` to the fish config."

**Expected:**
- Agent edits `~/.config/fish/config.fish`
- Agent runs `dotsync linux -m "add RUST_BACKTRACE to fish" -- .config/fish/config.fish`
- Change is on `linux` scope (not `all`, not machine)

### 3. Machine-specific change

**Task for agent:** "Set the hyprland monitor to `DP-1,2560x1440@144,0x0,1` — this is specific to this machine."

**Expected:**
- Agent edits `~/.config/hypr/hyprland.conf`
- Agent runs `dotsync mx-xps-cy -m "set DP-1 monitor" -- .config/hypr/hyprland.conf`
- Change is on machine scope

### 4. Check status before committing

**Task for agent:** "Something changed in my fish config. Check what's different, then commit it to the right scope."

**Expected:**
- Agent runs `dotsync status` first
- Agent sees the change
- Agent commits to appropriate scope

## Validation

After each scenario, check:
1. Exit code 0 from dotsync
2. Correct scope used (check via `dotsync --output json status` or repo inspection)
3. File contents correct at `~/`
4. No unexpected side effects

## Running with an agent

```bash
# Set up environment
./setup.sh /tmp/dotsync-test
export HOME=/tmp/dotsync-test/home
export DOTSYNC_OS=linux
export DOTSYNC_HOSTNAME=mx-xps-cy

# Launch OpenCode in isolated mode pointed at $HOME
# Give it the scenario task + the docs/SKILL.md instructions
```
