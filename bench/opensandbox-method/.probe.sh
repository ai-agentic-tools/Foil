export PATH=$HOME/foil-demo-bin:$PATH
ws=$HOME/foil-django-demo/django
foil session -- foil serve --listen 127.0.0.1:18311 -w $ws >/tmp/s2.log 2>&1 &
pid=$!; sleep 2
H="OPEN-SANDBOX-API-KEY: bench"
for c in 1 10; do
  for r in 1 2; do
    start=$(date +%s.%N)
    for i in $(seq $c); do curl -s -o /tmp/c$i.json -w "%{time_total}\n" -H "$H" -H 'Content-Type: application/json' -d '{"image":{"uri":"foil"}}' http://127.0.0.1:18311/v1/sandboxes & done | sort -n | tr '\n' ' '
    wait; end=$(date +%s.%N); echo " c=$c wall=$(echo "$end - $start" | bc)"
    for i in $(seq $c); do id=$(python3 -c "import json;print(json.load(open('/tmp/c$i.json'))['id'])"); curl -s -X DELETE -H "$H" http://127.0.0.1:18311/v1/sandboxes/$id; done
    sleep 1
  done
done
nproc; uptime
kill $pid; sleep 1; ps -eo pid,args | grep 18311 | grep -v grep | awk '{print $1}' | xargs -r kill
