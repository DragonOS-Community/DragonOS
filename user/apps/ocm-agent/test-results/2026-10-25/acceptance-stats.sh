#!/bin/sh
{
  echo '### uname'; uname -a
  echo '### uptime'; cat /proc/uptime
  echo '### loadavg'; cat /proc/loadavg
  echo '### meminfo'; head -8 /proc/meminfo
  echo '### ps'; ps
  PID=$(cat /opt/ocm-agent/agent.pid)
  echo "### agent pid=$PID"
  tr -d '\000' < /proc/$PID/status | grep -E '^(Name|State|Threads|VmRSS|VmSize|VmPeak):'
  echo '### agent statm'; cat /proc/$PID/statm
  echo '### agent tasks'; ls /proc/$PID/task | wc -l
  echo '### df'; df -P -B 1 /
  echo '### plugin dirs'; ls -l /opt/ocm-agent/plugins/*/
  echo '### data dir'; ls -l /opt/ocm-agent/data
  echo '### log tail'; tail -40 /opt/ocm-agent/smoke.log
} > /opt/ocm-agent/runtime-stats.txt 2>&1
echo STATS_DONE
