#!/bin/bash
# HOST side: copy the worktree source into ~/sss of a test VM (tar over ssh; no rsync in the VMs).
# usage: vm-sync.sh PORT      then in the VM: cd ~/sss && cargo test -p simple-screensharing-client --release --lib --no-run
#                             and cargo build --release -p simple-screensharing-server
port=$1
cd "$(git rev-parse --show-toplevel)" || exit 1
tar c --exclude=./target --exclude=./.git --exclude=./.tl-scratch --exclude=./.claude --exclude=./docs . \
  | ssh -o BatchMode=yes -p "$port" mo@127.0.0.1 'mkdir -p ~/sss && tar x -C ~/sss'
