#!/bin/bash

# Test script that provides input to mdump
echo "Testing mdump with SD_Card"

# Run mdump with automatic selections (select first media, then local_backup destination)
echo -e "0\n2\nn" | ./target/release/mdump --dry-run backup --destination local_backup