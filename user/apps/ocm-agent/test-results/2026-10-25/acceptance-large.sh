#!/bin/sh
/opt/ocm-agent/ocm-gateway -push-socket /opt/ocm-agent/var/chan.sock -push-bytes 120000
echo LARGE_DONE
