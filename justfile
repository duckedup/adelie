# Recover the beads database in a fresh clone and wire the remote (never `bd init`, D0002)
bd-setup:
    ./scripts/bd-setup.sh

# `git push` does NOT carry issue state; the database rides refs/dolt/data.
# Publish and collect issue state (bd dolt pull, then push)
bd-sync:
    bd dolt pull
    bd dolt push
