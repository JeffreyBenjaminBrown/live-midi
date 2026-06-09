# Launch the midi dev container. Run this from the worktree root, e.g.
#   bash docker.sh
# Identity (container name, mounted dir) is derived from the worktree, so this
# single committed file works unchanged in every worktree -- no per-instance
# edits to drift out of sync.

if [ ! -e .git ]; then
  echo "Run this from the worktree root (no .git here)." >&2
  exit 1
fi

HOST="$(pwd)"
# The worktree basename is usually something like "main" (no "midi" in it),
# so prefix "midi-"  ->  e.g. main -> midi-main.
CONTAINER_NAME="midi-$(basename "$HOST")"

# Claude Code state (the transcripts `--resume` reads, prompt history, OAuth
# credentials, config) AND our shared user-level config both live in
# my-dot-claude -- a git repo inside the worktree at $HOST/my-dot-claude.
# Because the worktree is bind-mounted to /home/ubuntu/host below, that state
# rides along on the one mount and survives `docker rm` + rebuild; no separate
# .claude mount is needed. CLAUDE_CONFIG_DIR (=/home/ubuntu/host/my-dot-claude)
# and PIPEWIRE_RUNTIME_DIR are baked into the image (docker.nix), so they are
# NOT passed with -e here. NOTE: this assumes the NEW image; with an older one
# lacking the baked env, Claude falls back to the ephemeral ~/.claude.

docker run --name "$CONTAINER_NAME" -it -d               \
  -e PIPEWIRE_RUNTIME_DIR=/run/user/1000                 \
  -v /tmp/.X11-unix:/tmp/.X11-unix                       \
  -e DISPLAY="${DISPLAY:-:0}"                            \
  -v /nix/store:/nix/store:ro                            \
  --network host                                         \
  --platform linux/amd64                                 \
  --user 1000:1000                                       \
  --dns 8.8.8.8 --dns 1.1.1.1                            \
  -w /home/ubuntu/host                                   \
  -v "$HOST":/home/ubuntu/host                           \
  --group-add "$(getent group audio | cut -d: -f3)"      \
  --device /dev/snd                                      \
  --cap-add=SYS_NICE                                     \
  --ulimit rtprio=99                                     \
  --ulimit memlock=-1                                    \
  jeffreybbrown/hode:latest

# Handy follow-ups:
#   docker exec -it "$CONTAINER_NAME" bash                 # attach a shell
#   docker stop "$CONTAINER_NAME" && docker rm "$CONTAINER_NAME"

  # PITFALL: mount the worktree at the /home/ubuntu/host SUBPATH, never over
  #   /home/ubuntu itself -- that hides the image's ~/.bashrc and the writable
  #   ~/.local/npm-global prefix (used by the AI-CLI bootstrap and the
  #   my-dot-claude auto-clone), and breaks CLAUDE_CONFIG_DIR persistence.
  # The --group-add line dynamically gets the host audio group GID.
  # --network host binds each container port to the host port of the same
  #   number (e.g. 1729=1729 for TypeDB); the --dns lines let Claude Code work
  #   through a phone hotspot.
  # The /tmp/.X11-unix mount + DISPLAY let minifb/X11 GUI windows open on the
  #   host; the image sets LD_LIBRARY_PATH for libX11/libXcursor/libXrandr.
  # The /nix/store bind keeps Nix-built image store references available.
  # --cap-add=SYS_NICE + rtprio=99 + memlock=-1 let container processes raise
  #   their own thread priority to SCHED_FIFO, which grid_synth needs for
  #   glitch-free low-latency audio.
