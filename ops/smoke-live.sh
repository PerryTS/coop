#!/usr/bin/env bash
set -euo pipefail

ops_root=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repository=$(cd "$ops_root/.." && pwd)

: "${PROMETHEUS_BIN:?set PROMETHEUS_BIN to a Prometheus server binary}"
: "${GRAFANA_BIN:?set GRAFANA_BIN to the Grafana binary}"
: "${GRAFANA_HOME:?set GRAFANA_HOME to the Grafana share/home directory}"

metrics_port=${COOP_SMOKE_METRICS_PORT:-19101}
prometheus_port=${COOP_SMOKE_PROMETHEUS_PORT:-19090}
grafana_port=${COOP_SMOKE_GRAFANA_PORT:-13000}

for dependency in node curl jq sed seq; do
  command -v "$dependency" >/dev/null || {
    echo "missing required executable: $dependency" >&2
    exit 1
  }
done
for executable in "$PROMETHEUS_BIN" "$GRAFANA_BIN"; do
  [[ -x "$executable" ]] || {
    echo "not an executable: $executable" >&2
    exit 1
  }
done
[[ -f "$GRAFANA_HOME/conf/defaults.ini" ]] || {
  echo "GRAFANA_HOME does not contain conf/defaults.ini: $GRAFANA_HOME" >&2
  exit 1
}

smoke_tmp=$(mktemp -d "${TMPDIR:-/tmp}/coop-ops-smoke.XXXXXX")
fixture_pid=
prometheus_pid=
grafana_pid=

cleanup() {
  local status=$?
  if [[ $status -ne 0 ]]; then
    for log in "$smoke_tmp/prometheus.log" "$smoke_tmp/grafana.log" "$smoke_tmp/fixture.log"; do
      if [[ -f "$log" ]]; then
        echo "last lines from $log:" >&2
        tail -n 40 "$log" >&2 || true
      fi
    done
  fi
  for pid in "$grafana_pid" "$prometheus_pid" "$fixture_pid"; do
    if [[ -n "$pid" ]]; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  case "$smoke_tmp" in
    "${TMPDIR:-/tmp}"/coop-ops-smoke.*|/tmp/coop-ops-smoke.*)
      rm -rf -- "$smoke_tmp"
      ;;
    *)
      echo "refusing to remove unexpected smoke directory: $smoke_tmp" >&2
      ;;
  esac
  return "$status"
}
trap cleanup EXIT INT TERM

wait_for_url() {
  local url=$1
  local label=$2
  for _ in $(seq 1 200); do
    if curl --fail --silent --show-error "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  echo "$label did not become ready at $url" >&2
  return 1
}

mkdir -p \
  "$smoke_tmp/prometheus-data" \
  "$smoke_tmp/grafana-data" \
  "$smoke_tmp/grafana-logs" \
  "$smoke_tmp/grafana-plugins" \
  "$smoke_tmp/provisioning/datasources" \
  "$smoke_tmp/provisioning/dashboards"

escaped_rules=$(printf '%s' "$ops_root/prometheus/coop-rules.yml" | sed 's/[&|]/\\&/g')
escaped_dashboard=$(printf '%s' "$ops_root/grafana" | sed 's/[&|]/\\&/g')
sed \
  -e "s|@RULES_PATH@|$escaped_rules|g" \
  -e "s|@METRICS_PORT@|$metrics_port|g" \
  "$ops_root/smoke/prometheus.yml.in" >"$smoke_tmp/prometheus.yml"
sed \
  -e "s|@PROMETHEUS_PORT@|$prometheus_port|g" \
  "$ops_root/smoke/datasource.yml.in" \
  >"$smoke_tmp/provisioning/datasources/coop.yml"
sed \
  -e "s|@DASHBOARD_PATH@|$escaped_dashboard|g" \
  "$ops_root/smoke/dashboard.yml.in" \
  >"$smoke_tmp/provisioning/dashboards/coop.yml"

COOP_SMOKE_METRICS_PORT=$metrics_port \
  node "$ops_root/smoke/metrics-fixture.mjs" \
  >"$smoke_tmp/fixture.log" 2>&1 &
fixture_pid=$!
wait_for_url "http://127.0.0.1:$metrics_port/ready" "metric fixture"

"$PROMETHEUS_BIN" \
  --config.file="$smoke_tmp/prometheus.yml" \
  --storage.tsdb.path="$smoke_tmp/prometheus-data" \
  --storage.tsdb.retention.time=1h \
  --web.listen-address="127.0.0.1:$prometheus_port" \
  --web.enable-lifecycle \
  >"$smoke_tmp/prometheus.log" 2>&1 &
prometheus_pid=$!
wait_for_url "http://127.0.0.1:$prometheus_port/-/ready" "Prometheus"

for _ in $(seq 1 100); do
  if curl --fail --silent --show-error \
    "http://127.0.0.1:$prometheus_port/api/v1/query?query=coop_deployments_total" \
    | jq -e '.status == "success" and (.data.result | length) == 1' >/dev/null; then
    break
  fi
  sleep 0.1
done
curl --fail --silent --show-error \
  "http://127.0.0.1:$prometheus_port/api/v1/query?query=coop_deployments_total" \
  | jq -e '.status == "success" and (.data.result | length) == 1' >/dev/null
curl --fail --silent --show-error \
  "http://127.0.0.1:$prometheus_port/api/v1/rules" \
  | jq -e '.status == "success" and ([.data.groups[].rules[]] | length) == 22' >/dev/null

GF_PATHS_DATA="$smoke_tmp/grafana-data" \
GF_PATHS_LOGS="$smoke_tmp/grafana-logs" \
GF_PATHS_PLUGINS="$smoke_tmp/grafana-plugins" \
GF_PATHS_PROVISIONING="$smoke_tmp/provisioning" \
GF_SERVER_HTTP_ADDR=127.0.0.1 \
GF_SERVER_HTTP_PORT=$grafana_port \
GF_SECURITY_ADMIN_USER=coop-smoke \
GF_SECURITY_ADMIN_PASSWORD=coop-smoke-password \
GF_AUTH_ANONYMOUS_ENABLED=true \
GF_AUTH_ANONYMOUS_ORG_ROLE=Viewer \
GF_USERS_ALLOW_SIGN_UP=false \
  "$GRAFANA_BIN" server --homepath "$GRAFANA_HOME" \
  >"$smoke_tmp/grafana.log" 2>&1 &
grafana_pid=$!
wait_for_url "http://127.0.0.1:$grafana_port/api/health" "Grafana"

for _ in $(seq 1 100); do
  if curl --fail --silent --show-error \
    "http://127.0.0.1:$grafana_port/api/search?query=Coop%20server%20overview" \
    | jq -e 'any(.[]; .uid == "coop-server-overview")' >/dev/null; then
    break
  fi
  sleep 0.1
done
curl --fail --silent --show-error \
  "http://127.0.0.1:$grafana_port/api/search?query=Coop%20server%20overview" \
  | jq -e 'any(.[]; .uid == "coop-server-overview")' >/dev/null
curl --fail --silent --show-error \
  "http://127.0.0.1:$grafana_port/api/datasources/uid/coop-prometheus/health" \
  | jq -e '.status == "OK" or .status == "success"' >/dev/null

printf '{"prometheus_rules":22,"prometheus_series":1,"grafana_dashboard_uid":"coop-server-overview","grafana_datasource_uid":"coop-prometheus"}\n'
