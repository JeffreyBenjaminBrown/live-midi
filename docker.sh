exit # This is not a script, just snippets.

echo $CONTAINER_NAME
CONTAINER_NAME=midi

HOST=/home/jeff/code/midi
docker run --name "$CONTAINER_NAME" -it -d               \
  -v /run/user/1000/pipewire-0:/run/user/1000/pipewire-0 \
  -e PIPEWIRE_RUNTIME_DIR=/run/user/1000                 \
  --network host              \
  --platform linux/amd64      \
  --user 1000:1000            \
  --dns 8.8.8.8 --dns 1.1.1.1 \
  -v "$HOST":/home/ubuntu     \
  --group-add $(getent group audio | cut -d: -f3) \
  --device /dev/snd           \
  jeffreybbrown/hode:latest

  # Above:
  # The --group-add line dynamically gets the host audio group GID.
  # The --dns line somehow lets Claude Code
  #   work through my phone's mobile hotspot.
  #   --newtowrk host binds each port
  #   to the host's port of the same number,
  #   e.g. 1729=1729 (TypeDB).

docker start $CONTAINER_NAME
docker exec -it $CONTAINER_NAME bash

docker stop $CONTAINER_NAME && docker rm $CONTAINER_NAME
