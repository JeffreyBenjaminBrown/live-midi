exit # This is not a script, just snippets.

echo $CONTAINER_NAME
CONTAINER_NAME=midi

HOST=/home/jeff/code/midi
docker run --name "$CONTAINER_NAME" -it -d               \
  -v /run/user/1000/pipewire-0:/run/user/1000/pipewire-0 \
  -e PIPEWIRE_RUNTIME_DIR=/run/user/1000                 \
  -v /tmp/.X11-unix:/tmp/.X11-unix                       \
  -e DISPLAY="${DISPLAY:-:0}"                            \
  -v /nix/store:/nix/store:ro                            \
  --network host                                         \
  --platform linux/amd64                                 \
  --user 1000:1000                                       \
  --dns 8.8.8.8 --dns 1.1.1.1                            \
  -v "$HOST":/home/ubuntu                                \
  --group-add $(getent group audio | cut -d: -f3)        \
  --device /dev/snd                                      \
  --cap-add=SYS_NICE                                     \
  --ulimit rtprio=99                                     \
  --ulimit memlock=-1                                    \
  jeffreybbrown/hode:latest

  # Above:
  # The --group-add line dynamically gets the host audio group GID.
  # The --dns line somehow lets Claude Code
  #   work through my phone's mobile hotspot.
  #   --network host binds each port
  #   to the host's port of the same number,
  #   e.g. 1729=1729 (TypeDB).
  # The /tmp/.X11-unix mount and DISPLAY let minifb/X11 GUI windows
  #   open on the host; the rebuilt image sets LD_LIBRARY_PATH for
  #   libX11/libXcursor/libXrandr.
  # The /nix/store bind keeps Nix-built image store references available
  #   when the image was built by docker-typedb-rust/docker.nix.
  # The --cap-add/ulimit lines let processes in the container raise
  #   their own thread priority to SCHED_FIFO, which grid_synth needs
  #   for glitch-free low-latency audio.
  #   - SYS_NICE: permission to change scheduling policy.
  #   - rtprio=99: ulimit cap on RT priority (default 0 blocks all RT).
  #   - memlock=-1: unlimited mlock for audio buffers (so pages aren't
  #     swapped out during the RT callback).

docker start $CONTAINER_NAME
docker exec -it $CONTAINER_NAME bash

docker stop $CONTAINER_NAME && docker rm $CONTAINER_NAME
