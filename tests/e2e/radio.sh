#!/bin/bash
pkill -x fake-radio; sleep 0.3
cd /home/claude/crewaware && FAKE_PARKED_SECS=${1:-60} setsid nohup ./target/debug/fake-radio --listen 127.0.0.1:7777 >/tmp/fake.log 2>&1 < /dev/null &
sleep 1
