---
name: omarchy-migration
description: Replicate, extend, and network a personal Omarchy (Arch + Hyprland) fleet — migrate a machine's configs/packages/zsh/themes/Syncthing/agent-stack/browser-tabs over SSH, join new devices to the Tailscale mesh (subnet-routed NAS access), share the clipboard over SSH, and provision a fresh machine in either the always-on-server ("lily") or daily-driver ("gimli") role. Adapts for hardware (NVIDIA vs AMD) and never mutates another machine without explicit consent.
---

# Omarchy Fleet: Migration & Network Setup

Replicate one Omarchy machine onto another, **and** wire new devices into the
personal network so everything interoperates seamlessly. Drive the work
autonomously, but treat any *other* machine (especially the always-on server) as
read-only unless the user explicitly authorizes a specific change.

## Machine roles & network architecture (the mental model)

The fleet is built around one **always-on server** and one or more **daily
drivers**, glued together by **Tailscale**. Internalize this before touching
anything — most decisions fall out of it.

- **`lily` — always-on server / remote dev environment.**
  - AMD (Strix Halo): `vulkan-radeon`, `amd-ucode`, `rocm`. Runs **LLM inference**
    (llama-server, whisper, ollama, lmstudio) — the GPU box.
  - Physically on the **home NAS LAN** (`10.0.0.0/24`, via wifi `wlan0`).
  - Runs the **shared automations** (telegram bot / hermes-gateway, agent-inbox
    pollers, cron digests) — these live on lily *only* to avoid duplicate pollers.
  - Is the **Tailscale subnet router** that advertises the NAS LAN to the tailnet.
  - Always reachable by its tailscale name `lily`; users SSH into it for remote dev.

- **`gimli` — daily driver.**
  - Intel + **NVIDIA** (`intel-ucode`, `nvidia-*-dkms`; NVIDIA env vars in
    `~/.config/hypr/envs.conf`). **No** inference packages.
  - Roams on a **different LAN segment** (`192.168.1.0/24`, wired `eno1`) that is
    *not* routed to the NAS LAN — it reaches the NAS through lily over the tailnet.
  - Runs **no** duplicate automations. SSHes into lily for heavy/remote work.

- **The tailnet is the backbone.** Every machine addresses every other by its
  tailscale name regardless of physical network. New devices join the tailnet,
  accept lily's subnet route, and immediately reach the NAS and peers. Plan for
  machines that roam between networks — never assume two machines share an L2/L3
  segment just because they're "at home."

When provisioning a fresh box, first decide **which role** it plays (server vs
daily driver) — that drives packages, automations, and whether it advertises or
merely accepts subnet routes. See "Provisioning by role" at the end.

## Golden rules

1. **Never write to another machine** (especially the always-on server) unless the
   user authorizes that specific change. Reading configs/packages is fine; editing
   files, `tailscale set`, restarting services, or syncing data back is not —
   confirm first and call it out as an exception. This holds even *after* a
   migration: enabling clipboard sharing or subnet routing still means touching the
   server, so ask.
2. **Hardware differs** — GPU/CPU stacks rarely match. Don't blind-copy GPU env
   vars, microcode, or driver packages. Diff and adapt.
3. **Copy live state, not a dotfiles repo blindly.** A `~/dotfiles`/stow repo may be
   stale; the machine's *running* configs are the truth. Verify before trusting it.
4. **Ask at genuine forks** (shell framework, what to exclude, secrets, sync
   direction, duplicate automations, role of a new machine). Recommend a default.
5. **Back up every file before overwriting** (`cp -a` to a timestamped path), on
   either machine. New files are additive and safer than edits.
6. **sudo / chsh / ssh-copy-id / admin-console steps are interactive** — script them
   or hand the exact command to the user; you usually can't run them yourself.

## Phase 0 — Connectivity & SSH

- Find the source host (check `~/.ssh/known_hosts`, `tailscale status`, `/etc/hosts`).
- Add an SSH alias in `~/.ssh/config` (User, IdentityFile, IdentitiesOnly).
- Authorize the target's key on the source. `ssh-copy-id` needs the source
  password once — **interactive**, so ask the user to run it:
  `ssh-copy-id -i ~/.ssh/id_ed25519.pub user@SOURCE`
- Verify: `ssh -o BatchMode=yes SOURCE 'hostname'`.

## Phase 1 — Inventory (read-only on SOURCE)

```bash
ssh SOURCE 'hostname; lspci | grep -iE "vga|3d|display"; grep -m1 vendor_id /proc/cpuinfo'
lspci | grep -iE "vga|3d|display"; grep -m1 vendor_id /proc/cpuinfo          # TARGET
ssh SOURCE 'pacman -Qqen' ; ssh SOURCE 'pacman -Qqem'                        # native + AUR
pacman -Qqen ; pacman -Qqem                                                  # TARGET, for the diff
ssh SOURCE 'systemctl list-unit-files --state=enabled --no-legend'
ssh SOURCE 'systemctl --user list-unit-files --state=enabled --no-legend'
ssh SOURCE 'ls ~/go/bin; cargo install --list; npm ls -g --depth=0; mise ls; code --list-extensions'
ssh SOURCE 'cat /etc/fstab; mount | grep -iE "nfs|cifs|//"'                  # NAS / network mounts
```
Compute diffs with `comm -23 <(sort src) <(sort tgt)`.

## Phase 2 — Decisions to confirm with the user

- **Role** of the new machine (server vs daily driver) — gates everything below.
- **Shell framework**: replicate the source's actual `.zshrc` (may use zinit, not
  oh-my-zsh, even if `~/.oh-my-zsh` exists) vs a clean framework.
- **Exclusions**: LLM-inference (ollama, lmstudio, llama.cpp, whisper, rocm) for a
  non-inference box; AMD packages on an NVIDIA box, etc.
- **Secrets** (`~/.config/zsh/.zsh_secrets`, tokens, `~/.nibble/config.toml` bot
  tokens, NAS credentials): copy or set up manually. Handle as sensitive.
- **Sync direction** (Phase 6): receive-only first, then two-way.
- **Duplicate automations**: telegram listeners / cron digests run on ONE machine
  only — keep them on the always-on server.

## Phase 3 — Packages

Install list = (source native+AUR) − (target already has) − (role excludes).
Map hardware: `vulkan-radeon`→`nvidia`/`vulkan-*`, `amd-ucode`→`intel-ucode`.
Install via `yay -S --needed`. **sudo is interactive** — write a script, have the
user run `! bash script.sh`.

Language-level (no root, can background — slow):
```bash
mise install -y
go install <pkg>@latest                 # goimports gopls staticcheck golangci-lint
cargo install <crate>                    # some need --features (e.g. iroh-relay: server)
$(mise which npm) install -g <pkg>
code --install-extension <ext> --force   # some (copilot) need in-app auth afterward
```
Gotcha: AUR packages get removed/renamed (e.g. `google-antigravity-bin`). Flag
missing ones; offer the current substitute instead of guessing.

## Phase 4 — Configs (copy live, hardware-aware)

Copy from SOURCE, backing up each target file first. **Surgical, not wholesale:**

- **`~/.config/hypr/`**: copy `bindings/input/monitors/looknfeel/hypridle/hyprlock/
  hyprsunset/xdph.conf`. **Do NOT overwrite the target's `envs.conf`** if it holds
  GPU vars (NVIDIA `__GLX_VENDOR_LIBRARY_NAME`, `NVD_BACKEND`). `monitors.conf` is
  only safe if displays match — `hyprctl monitors` on both.
- **`hyprland.conf`**: adopting the source's is usually fine (sources `envs.conf`,
  adds keyring + `cursor { no_hardware_cursors = true }`, which helps NVIDIA).
- **Themes/backgrounds**: copy `~/.config/omarchy/backgrounds/` and repoint
  `~/.config/omarchy/current/background` to the source's `readlink` target.
- **Terminals**: copy for fonts/keybinds but **strip source-only GPU workarounds**
  like `LIBGL_ALWAYS_SOFTWARE=1` (kills NVIDIA perf).
- **CLI tools**: nvim (LazyVim re-syncs on launch), tmux, zellij, btop, fastfetch,
  mimeapps.list. Diff first; migrate only what differs.
- **git**: keep the target's richer defaults; set `user.name`/`user.email` (confirm
  the email) and copy `~/.config/git/ignore`.
- **Skip** browser/app *profile* dirs (1Password, Signal, LM Studio, Ledger,
  browsers) — auth/device state. Tabs handled in Phase 8.

Create dirs that bindings reference (e.g. `~/Pictures/Screenshots`).

## Phase 5 — zsh

```bash
rsync -aL SOURCE:.zshrc ~/.zshrc          # -L resolves a dotfiles symlink
rsync -aL SOURCE:.zshenv ~/.zshenv
mkdir -p ~/.config/zsh
rsync -aL SOURCE:.config/zsh/.zsh_secrets ~/.config/zsh/.zsh_secrets   # if authorized
chmod 600 ~/.config/zsh/.zsh_secrets
```
zinit auto-installs plugins on first launch. Review `.zshrc` for source-specific
bits (VPN aliases, `OLLAMA_GPU`, `mount-nas`, wrapper paths, auto-`zellij attach` —
the user may want this disabled on a daily driver that SSHes into a server which
*already* auto-attaches, to avoid nested zellij). **`chsh -s /usr/bin/zsh` is
interactive and needs a full logout/login** — the running session keeps exporting
the old `$SHELL`.

## Phase 6 — Syncthing

Pair via the **REST API** (no manual config.xml edits / restarts). Both ends:
key = `grep -oP '(?<=<apikey>)[^<]+' ~/.local/state/syncthing/config.xml`,
GUI on `127.0.0.1:8384`. Get the real device id from
`GET /rest/system/status` → `myID` (do **not** regex the first `<device id=>` in
config.xml — that's a folder share entry, not the local device).

```text
# TARGET: add SOURCE device, add folders RECEIVE-ONLY first
POST /rest/config/devices         {deviceID, name, addresses:["tcp://SOURCE:22000","dynamic"]}
POST /rest/config/folders         {id, path, type:"receiveonly", devices:[target,source]}
POST /rest/db/ignores?folder=ID   {"ignore":["/big-or-excluded-dir"]}   # e.g. llm-models
PATCH /rest/config/folders/ID     {"type":"sendreceive"}   # flip to two-way, once caught up
```
On SOURCE (only with explicit consent): add the target device, append it to the
folder's `devices`, create any new share. Pin the Tailscale address for a direct
connection. **Start receive-only** so the target can't push older content onto the
source; once `GET /rest/db/status` shows `state=idle, needBytes=0`, flip to
`sendreceive`. Check `df -h` first; exclude large model/data dirs.

**Before flipping a code workspace to two-way, exclude generated dirs on BOTH ends**
(non-rooted so they match at any depth): `target`, `venv`, `.venv`, `node_modules`,
`__pycache__`, `.pytest_cache`. Otherwise the target's local divergence is dominated
by build artifacts (cross-arch Rust `target/`, venvs, node_modules) that get pushed
onto the source — churn, cross-arch breakage, and "delete dir: not empty" errors.
Per-machine ignores are fine and asymmetric (e.g. only the daily driver ignores
`/llm-models`); the resulting `globalBytes` difference is expected, not a sync gap.
After setting ignores, a rescan of a large folder is slow and `GET /rest/db/localchanged`
shows stale (still-counted) entries mid-scan — but ignored paths are never pushed, so
it is safe to flip once `needBytes≈0` without waiting for the scan to fully settle.

**`.git` is synced two-way** (carries history without remotes) but that generates
`.sync-conflict-*` cruft across 3+ devices; `.git/objects/*` conflicts are harmless
(content-addressed, git ignores the dupes). Verify a flip created nothing new with
`find ~/workspace -name '*.sync-conflict-*' -newermt <today>` (0 = clean); pre-existing
conflicts carry an older datestamp + the *other* device's id suffix in the filename.

## Phase 7 — Agent stack (nibble etc.)

- Install nibble with the one-command installer — needs cargo/jq/podman/musl;
  builds Rust + a container (slow; background it). Re-running it updates:
  `curl -fsSL https://raw.githubusercontent.com/adlrocha/nibble/main/install.sh | bash`.
  Only if the device does nibble development, use `./install.sh` in the
  authoritative clone (`~/workspace/personal/nibble`) instead, never both.
- Install vert the same way (re-run to update):
  `curl -fsSL https://raw.githubusercontent.com/adlrocha/vert/main/install.sh | bash`.
  For `x` (explain a change), set the agent in `.zshrc`, e.g.
  `export VERT_EXPLAIN_AGENT=pi` and optionally
  `export VERT_EXPLAIN_MODEL=<provider/model>` from `pi --list-models`.
- Bring `~/.nibble` **config** over but **exclude** `cache/`, `logs/`, and the live
  `tasks.db*` (copying a live sqlite db risks corruption; it's runtime state).
- Keep wrapper paths consistent with `.zshrc` aliases (`~/.nibble/wrappers`).
- **Disable duplicate messaging/cron services** on a daily driver (e.g.
  `hermes-gateway`, telegram listeners): two pollers on one bot conflict.
  `systemctl --user disable --now <svc>`. Keep machine-local ones (resume-after-reboot).
- `config.toml` may point memory/LLM at a `localhost:NNNN` only the server runs —
  note it; it degrades gracefully or needs repointing at `lily`.

## Phase 8 — Browser tabs (Brave/Chromium)

```bash
pgrep -f "/opt/brave-bin/brave --" && exit 1     # guard on the real binary, not "brave --" (self-matches)
cp -a "$PROFILE/Sessions" backup/
rsync -a --delete SOURCE:".config/BraveSoftware/Brave-Browser/Default/Sessions/" \
   "$HOME/.config/BraveSoftware/Brave-Browser/Default/Sessions/"
rm -f "$PROFILE"/{Current,Last}\ {Session,Tabs}
# set session.restore_on_startup=1 in Default/Preferences (JSON), then launch.
```
Best-effort: the source's on-disk session lags slightly if its browser is open.

## Phase 9 — Tailscale mesh + subnet routing (reach the NAS & peers)

A new device must join the tailnet **and** be able to reach the NAS LAN, which it
is almost never on directly. The always-on server (`lily`) bridges it.

**Diagnose reachability properly — ICMP lies:**
- `ping` may be firewalled while SMB works. Test the **actual TCP port**:
  `timeout 5 bash -c 'cat </dev/null >/dev/tcp/10.0.0.31/445' && echo OPEN`.
- `traceroute -n 10.0.0.31`: if it dies right after your gateway (`* * *`), the
  segments are **not routed** to each other (classic asymmetric-NAT: a second
  router NATs the `10.0.0.0/24` segment, so server→daily-driver works but not the
  reverse). That's expected — use the subnet route below, don't chase the router.

**Make the server a subnet router (one-time, with consent):**
```bash
# on the server (needs net.ipv4.ip_forward=1 — usually already set):
sudo tailscale set --advertise-routes=10.0.0.0/24
```
Then **approve the route in the Tailscale admin console**
(login.tailscale.com/admin/machines → server → Edit route settings → approve the
subnet). This step is **human-only and mandatory** — the route is "available" but
unused until approved; you cannot do it for the user.

**Accept the route on the new device:**
```bash
sudo tailscale set --accept-routes=true        # use the explicit =true
tailscale debug prefs | grep RouteAll          # must read: "RouteAll": true,
```
- `tailscale set` applies **live — no daemon restart needed**. If `RouteAll` stays
  `false`, the command didn't actually run under sudo (no operator configured) or
  ran on the wrong host — re-run on the correct device and check `exit=$?`.
- Verify end-to-end: `ip route get 10.0.0.31` should show `dev tailscale0 table 52`,
  and the port test from above should now say OPEN.

**Mount the NAS (CIFS) like the server's `mount -a`:**
The server mounts NAS shares from `/etc/fstab`. Replicate:
```bash
# package (usually present): cifs-utils
mkdir -p ~/nas/{files,home,backups,netbackup}
# credentials (root-owned dir, 600). On the server, /etc/samba/credentials is
# user-owned so it's readable: username=... / password=...
sudo install -d -m755 /etc/samba
sudo tee /etc/samba/credentials >/dev/null <<'EOF'
username=USER
password=PASS
EOF
sudo chown USER:USER /etc/samba/credentials && sudo chmod 600 /etc/samba/credentials
# fstab — ADD `nofail` on a roaming daily driver so boot doesn't hang off-network
# (the always-on server omits it because it's always on the NAS LAN):
//10.0.0.31/files  /home/USER/nas/files  cifs credentials=/etc/samba/credentials,uid=1000,gid=1000,_netdev,nofail 0 0
# ...one line per share (files/home/Backups/NetBackup). Then:
sudo mount -a
```
Because the route runs over the tailnet via the server, `mount -a` now works **at
home and remotely** whenever the server is online.

## Phase 10 — Clipboard over SSH (OSC 52)

Goal: copy in a remote SSH terminal → text lands in the *local* machine's
clipboard. Mechanism is **OSC 52** (escape sequences tunneled through SSH; no X11
forwarding, which suits Wayland/Hyprland). Two sides:

**Local terminal (the client you sit at):** must honor OSC 52 writes. Alacritty
0.13+ defaults to `OnlyCopy`; set `terminal.osc52 = "CopyPaste"` to also allow
paste. Have `wl-clipboard` installed for the local Wayland clipboard.

**Remote host (what you SSH into):** configure the apps that copy.
- **nvim** (Neovim 0.10+ has a built-in OSC 52 provider): drop
  `~/.config/nvim/plugin/osc52.lua`, **gated on `vim.env.SSH_TTY`** so it only
  activates over SSH (local console keeps wl-clipboard):
  ```lua
  if vim.env.SSH_TTY and vim.env.SSH_TTY ~= "" then
    local ok, osc52 = pcall(require, "vim.ui.clipboard.osc52")
    if ok then vim.g.clipboard = { name = "OSC 52",
      copy = { ["+"]=osc52.copy("+"), ["*"]=osc52.copy("*") },
      paste = { ["+"]=osc52.paste("+"), ["*"]=osc52.paste("*") } } end
  end
  ```
  LazyVim already sets `clipboard=unnamedplus`, so plain `y` reaches the client.
- **zellij** (on the remote host): if `config.kdl` has an active
  `copy_command "wl-copy"`, it sends copies to the *remote's* clipboard — comment
  it out so zellij falls back to OSC 52 and reaches the client. Restart the session
  (`zellij kill-session main`) to pick up the change.
- **shell helper** `~/.local/bin/clip` (pipe anything to the client clipboard):
  ```bash
  #!/usr/bin/env bash
  data="$(cat)"; b64="$(printf '%s' "$data" | base64 | tr -d '\r\n')"
  printf '\033]52;c;%s\a' "$b64" > /dev/tty
  ```
- **mouse selection** already works through a local terminal; inside zellij hold
  **Shift** to bypass its mouse capture and use the terminal's native copy.

**On the client/daily-driver itself:** add only the **SSH-gated** `osc52.lua` and
`clip` (useful when *it* SSHes out). **Keep** its local zellij `copy_command
"wl-copy"` — local copies should use the native clipboard (no OSC 52 size cap).

Caveats: OSC 52 has a payload **size cap** (terminals truncate large copies) — fine
for normal use, not for huge buffers. OSC 52 **paste** needs the terminal to answer
back; if a paste hangs, switch the nvim provider to copy-only.

## Provisioning a fresh machine by role

After a clean Omarchy install, run Phases 0–10, specialized by role:

**Server / inference box (a "lily"):**
- Include AMD/inference packages (or the box's GPU stack); it's the inference host.
- Put it on the NAS LAN; mount the NAS directly (no `nofail` needed if always-on).
- Make it the **subnet router** (`--advertise-routes`, approve in console);
  ensure `net.ipv4.ip_forward=1`.
- Host the **shared automations** (telegram/hermes-gateway, agent-inbox, crons) —
  enable them here and *only* here.
- Set up the remote-dev affordances others rely on: zsh auto-attach zellij, the
  OSC 52 clipboard configs from Phase 10, stable tailscale name.

**Daily driver (a "gimli"):**
- **Exclude** inference packages; install the box's own GPU stack (e.g. NVIDIA) and
  preserve its `envs.conf`.
- It roams → **accept** the server's subnet route, mount NAS with `nofail`.
- **Disable** duplicate automations (single-poller rule).
- Disable auto-`zellij attach` if it primarily SSHes into the server (avoid nesting).
- Add SSH-gated `osc52.lua` + `clip`; keep local `wl-copy`.

## Common gotchas

- `sudo`, `chsh`, `ssh-copy-id`, and the Tailscale **admin-console route approval**
  are interactive / human-only — hand the exact command over; don't assume you ran it.
- `chsh` needs a full re-login to take effect.
- `tailscale set` applies live (no restart). `--accept-routes` must be set with
  sudo on the device itself; verify `RouteAll: true`. Advertising ≠ approved ≠
  accepted — all three are required.
- Diagnose NAS reachability with a **TCP port test**, not ping; a traceroute dying
  at the gateway means unrouted segments → use a subnet route, not router hacking.
- `pgrep -f "<pattern>"` matches your own command line — guard with a specific path.
- Preserve the target's NVIDIA `envs.conf`; never copy the source's over it.
- OSC 52: gate the nvim provider on `SSH_TTY`; only disable zellij `wl-copy` on the
  *remote* host, never on the local daily driver; mind the size cap & paste hang.
