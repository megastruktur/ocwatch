#!/bin/bash

ocwatch daemon stop
rm ~/.local/bin/ocwatch
cargo build --release
cp target/release/ocwatch ~/.local/bin/

ocwatch daemon start
