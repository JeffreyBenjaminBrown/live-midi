# Launch the midi dev container. Run this from anywhere inside a worktree, e.g.
#   bash docker.sh
# Container name and every mount are derived from git, so this single committed
# file works unchanged in every worktree -- no per-instance edits to drift out
# of sync, and it keeps working as worktrees come and go.
#
# Access model (see research_write-worktree-but-read-whole-project.org):
#   * the WHOLE repo is mounted READ-ONLY  -> Claude can read all git history
#     (every branch) and browse the tree, but cannot clobber sibling worktrees;
#   * THIS worktree is mounted READ-WRITE  -> Claude edits its own files;
#   * the shared git DB (.bare) is mounted READ-WRITE -> commits work (new
#     objects and this branch's ref live there).
# A worktree's committed history lives in .bare, two levels above the worktree,
# so mounting only the worktree would leave git with no object DB -- that's why
# .bare is mounted explicitly. Because .bare/config sets
# extensions.relativeWorktrees=true, every git pointer is relative, so
# reproducing the same relative layout under $ROOT below makes git resolve
# cleanly with no metadata rewriting (proven on git 2.53).

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "Run this from inside a git worktree." >&2
  exit 1
fi

WT_TOP="$(git rev-parse --show-toplevel)"                   # <repo>/worktrees/<name>
COMMON="$(cd "$(git rev-parse --git-common-dir)" && pwd)"   # <repo>/.bare  (absolute)
REPO_ROOT="$(dirname "$COMMON")"                            # <repo>
WT_NAME="$(basename "$WT_TOP")"                             # <name>
REL="${WT_TOP#"$REPO_ROOT"/}"                               # worktrees/<name>
COMMON_REL="$(basename "$COMMON")"                          # .bare
if [ "$REL" = "$WT_TOP" ]; then
  echo "Worktree ($WT_TOP) is not under the repo root ($REPO_ROOT);" >&2
  echo "can't derive container mounts for this layout." >&2
  exit 1
fi

ROOT=/home/ubuntu/host
CONTAINER_NAME="midi-$WT_NAME"

# CLAUDE_CONFIG_DIR is baked into the image as $ROOT/my-dot-claude, but the
# config now lives one level down inside the worktree, so override it here. It
# sits inside the read-write worktree mount, so each worktree keeps its own
# Claude state (sessions, credentials) and that state survives docker rm.

docker run --name "$CONTAINER_NAME" -it -d               \
  -v /run/user/1000/pipewire-0:/run/user/1000/pipewire-0 \
  -e PIPEWIRE_RUNTIME_DIR=/run/user/1000                 \
  -v /tmp/.X11-unix:/tmp/.X11-unix                       \
  -e DISPLAY="${DISPLAY:-:0}"                            \
  --network host                                         \
  --platform linux/amd64                                 \
  --user 1000:1000                                       \
  --dns 8.8.8.8 --dns 1.1.1.1                            \
  -v "$REPO_ROOT":"$ROOT":ro                             \
  -v "$COMMON":"$ROOT/$COMMON_REL"                       \
  -v "$WT_TOP":"$ROOT/$REL"                              \
  -w "$ROOT/$REL"                                        \
  -e CLAUDE_CONFIG_DIR="$ROOT/$REL/my-dot-claude"        \
  --group-add "$(getent group audio | cut -d: -f3)"      \
  --device /dev/snd                                      \
  --cap-add=SYS_NICE                                     \
  --ulimit rtprio=99                                     \
  --ulimit memlock=-1                                    \
  jeffreybbrown/hode:latest

# Handy follow-ups:
#   docker exec -it "$CONTAINER_NAME" bash                 # attach a shell
#   docker stop "$CONTAINER_NAME" && docker rm "$CONTAINER_NAME"

  # MOUNTS, matching the three -v lines above, top to bottom:
  #   1. whole repo, READ-ONLY at $ROOT -- read history + browse, no clobber.
  #   2. shared git DB (.bare), READ-WRITE -- needed to commit; this rw mount is
  #      nested inside the ro one, which Docker honors (more specific wins).
  #   3. this worktree, READ-WRITE at $ROOT/$REL -- where edits go, and -w points
  #      here. The deeper path is required so the worktree's relative
  #      "gitdir: ../../.bare/..." resolves to the .bare mounted in (2).
  # PITFALL: keep every mount under the /home/ubuntu/host SUBPATH, never over
  #   /home/ubuntu itself -- that hides the image's ~/.bashrc and the writable
  #   ~/.local/npm-global prefix (used by the AI-CLI bootstrap and loader).
  # PITFALL: do NOT bind-mount the host /nix/store. This image
  #   (dockerTools.buildLayeredImage) already carries its full closure; mounting
  #   the host store shadows it, and a host GC that dropped the bespoke
  #   local-bin path leaves /bin/claude a dangling symlink, i.e.
  #   "claude: command not found". See research_no-claude-in-midi.org.
  # The --group-add line dynamically gets the host audio group GID.
  # --network host binds each container port to the host port of the same
  #   number; the --dns lines let Claude Code work through a phone hotspot.
  # The /tmp/.X11-unix mount + DISPLAY let minifb/X11 GUI windows open on the
  #   host; the image sets LD_LIBRARY_PATH for libX11/libXcursor/libXrandr.
  # --cap-add=SYS_NICE + rtprio=99 + memlock=-1 let container processes raise
  #   their own thread priority to SCHED_FIFO, which grid_synth needs for
  #   glitch-free low-latency audio.
