#!/bin/sh
rm -f /opt/ocm-agent/data/client.crt /opt/ocm-agent/data/client.key
rm -f /opt/ocm-agent/data/ongoing.db /opt/ocm-agent/data/ongoing.db-shm /opt/ocm-agent/data/ongoing.db-wal
rm -rf /opt/ocm-agent/data/tasks
rm -f /opt/ocm-agent/smoke.log
/opt/ocm-agent/ocm-agent -config /opt/ocm-agent/config.yaml >/opt/ocm-agent/smoke.log 2>&1 &
echo $! > /opt/ocm-agent/agent.pid
echo SMOKE_STARTED=$!
